//! One live WS session: reader/writer/pinger/watchdog tasks + command
//! dispatch + the central-log tracing layer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use muxmesh_core::{Ack, CommandEnvelope};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::tmux::{resolve_socket, tmux_prefix_and_status, tmux_sessions};
use crate::ws::{Ws, MAX_MISSED_PONGS, PING_INTERVAL};

/// Read pushed frames until the socket dies: reader (dispatch+ack), writer
/// (sink owner: acks + forwarded log lines), seq-stamped pinger, and a
/// dead-hub watchdog. First task to finish ends the session.
pub async fn ws_session(
    cfg: &Arc<muxmesh_core::AgentConfig>,
    ws: Ws,
    mut log_rx: tokio::sync::broadcast::Receiver<String>,
) {
    let (mut sink, mut stream) = ws.split(); // liveness counters are SESSION-local: statics shared across reconnects
                                             // would corrupt the missed-pong accounting between sessions.
    let last_ping_seq = Arc::new(AtomicU64::new(0)); // last ping WE sent
    let alive_seq = Arc::new(AtomicU64::new(0)); // last ping the hub answered
                                                 // outbound frames (acks + keepalive pings) go through one channel so a
                                                 // single task owns the sink (a SplitSink cannot be shared).
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    // ping loop: outbound keepalive, seq-stamped so pongs prove liveness.
    let tx_p = tx.clone();
    let last_ping_p = last_ping_seq.clone();
    let pinger = async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ping.tick().await;
            let seq = last_ping_p.fetch_add(1, Ordering::Relaxed) + 1;
            if tx_p.send(Message::Ping(seq.to_string().into())).is_err() {
                return; // writer gone -> session over
            }
        }
    };

    // writer task: the ONLY owner of the sink. Central-log lines from the
    // tracing layer arrive on `log_rx` and are forwarded as {"log":...} frames;
    // the hub appends them to its hub.log (one file for the whole fleet).
    // shortcut: lines emitted while the WS is DOWN are lost (stderr keeps
    // them) — add replay-on-reconnect if the hub must hold full history.
    let tx2 = tx.clone();
    let writer = async move {
        loop {
            tokio::select! {
                biased;
                Ok(line) = log_rx.recv() => {
                    let frame = format!("{{\"log\":{}}}", serde_json::json!(line));
                    if tx2.send(Message::Text(frame.into())).is_err() {
                        return;
                    }
                }
                Some(msg) = rx.recv() => {
                    if sink.send(msg).await.is_err() {
                        return; // socket error -> session over
                    }
                }
            }
        }
    };

    let last_ping_r = last_ping_seq.clone();
    let alive_r = alive_seq.clone();
    let cfg_r = cfg.clone();
    let reader = async move {
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Some(ack) = dispatch_command(&cfg_r, &cfg_r.host, text.as_str()) {
                        if tx.send(Message::Text(ack.into())).is_err() {
                            return;
                        }
                    }
                }
                Ok(Message::Pong(data)) => {
                    // tungstenite echoes the ping payload back verbatim: a pong
                    // carrying our latest sent seq proves the hub is alive.
                    let seq: u64 = String::from_utf8_lossy(&data).parse().unwrap_or(0);
                    if seq != 0 && seq == last_ping_r.load(Ordering::Relaxed) {
                        alive_r.store(seq, Ordering::Relaxed);
                    }
                }
                Ok(Message::Close(_)) => return,
                Ok(_) => {} // binary/ping: ping auto-answered by tungstenite
                Err(e) => {
                    tracing::warn!("ws: recv error: {e}");
                    return;
                }
            }
        }
    };

    // Dead-hub watchdog: MAX_MISSED_PONGS pings sent without a matching pong
    // -> end the session so the reconnect loop takes over. A silent half-open
    // socket surfaces here, not as a read timeout (there is none).
    let last_ping_w = last_ping_seq.clone();
    let alive_w = alive_seq.clone();
    let watchdog = async move {
        loop {
            tokio::time::sleep(PING_INTERVAL).await;
            let last = last_ping_w.load(Ordering::Relaxed);
            if last.saturating_sub(alive_w.load(Ordering::Relaxed)) >= MAX_MISSED_PONGS {
                tracing::warn!(
                    "ws: {MAX_MISSED_PONGS} unanswered pings -> dropping connection (dead hub)"
                );
                return;
            }
        }
    };

    tokio::select! {
        _ = reader => {}
        _ = pinger => {}
        _ = writer => {}
        _ = watchdog => {}
    }
}

/// Pure dispatch decision: envelope -> ack JSON (no socket, no exec side
/// effects beyond the argv-scoped tmux call). dst mismatch or bad frame ->
/// no ack (None): the hub logs the queue TTL drop instead.
pub fn dispatch_command(
    cfg: &muxmesh_core::AgentConfig,
    host: &str,
    text: &str,
) -> Option<String> {
    let env: CommandEnvelope = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("ws: bad frame (not a command envelope): {e}");
            return None;
        }
    };
    if env.dst != host {
        tracing::warn!(
            "ws: ignoring command id={} dst={} (not me: {host})",
            env.id,
            env.dst
        );
        return None;
    }
    tracing::info!("ws: command id={} src={} cmd={:?}", env.id, env.src, env.cmd);

    let ack = if env.v != 1 {
        Ack::err(env.v, &env.id, "unsupported v")
    } else if !env.cmd.args_valid() {
        Ack::err(env.v, &env.id, "invalid args")
    } else {
        // "" => resolve the live socket (see resolve_socket); a literal "default"
        // fails when tmux runs from a per-user socket dir macOS rotates.
        let socket = resolve_socket(cfg);
        // the bar is a GLOBAL option: it hits every session/window on this
        // socket — name them in the trace.
        let affected = tmux_sessions(&socket);
        tracing::info!("ws: executing on socket={socket} — affects: {affected}");
        let before = tmux_prefix_and_status(&socket);
        match env.cmd.execute(&socket) {
            Ok(()) => {
                let after = tmux_prefix_and_status(&socket);
                tracing::info!("ws: done id={} ok — state {before} -> {after}", env.id);
                Ack::ok(env.v, &env.id)
            }
            Err(e) => {
                tracing::error!("ws: exec failed id={} socket={socket}: {e}", env.id);
                Ack::err(env.v, &env.id, &e)
            }
        }
    };
    Some(serde_json::to_string(&ack).unwrap_or_else(|_| "{}".into()))
}

/// tracing layer: every log event becomes {"log":"<formatted line>"} on the
/// agent's outbound WS channel (the hub appends it to central hub.log).
/// Non-blocking: a full/closed channel drops the line (stderr still has it).
pub struct WsLogLayer(pub tokio::sync::broadcast::Sender<String>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WsLogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct C(Option<String>);
        impl tracing::field::Visit for C {
            fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
                if f.name() == "message" {
                    self.0 = Some(v.to_string());
                }
            }
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                if self.0.is_none() && f.name() == "message" {
                    self.0 = Some(format!("{v:?}"));
                }
            }
        }
        let mut c = C(None);
        event.record(&mut c);
        if let Some(msg) = c.0 {
            let level = event.metadata().level();
            let line = format!("{level} {msg}");
            // best effort: no receiver up (WS down) = Err, stderr remains the
            // durable record. Never block a log call on the network.
            let _ = self.0.send(line);
        }
    }
}
