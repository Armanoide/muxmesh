//! One live WS session: the four tasks that share a connected socket, plus
//! the command cycle and the central-log tracing layer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use muxmesh_core::{Ack, CommandEnvelope, DirectoryAnswer};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::tmux::{build_state, resolve_socket, tmux_prefix_and_status, tmux_sessions};
use crate::ws::{Channel, Ws, MAX_MISSED_PONGS, PING_INTERVAL};

/// Hold ONE connected socket open until it dies: four concurrent tasks share
/// it — read pushed commands, write outbound frames, ping, watch liveness —
/// and the first to finish ends the session so dial_hub_forever reconnects.
pub async fn run_socket_until_dead(
    cfg: &Arc<muxmesh_core::AgentConfig>,
    chan: &Arc<Channel>,
    ws: Ws,
    log_rx: tokio::sync::broadcast::Receiver<String>,
) {
    // liveness counters are SESSION-local: statics shared across reconnects
    // would corrupt the missed-pong accounting between sessions.
    let last_ping_seq = Arc::new(AtomicU64::new(0)); // last ping WE sent
    let alive_seq = Arc::new(AtomicU64::new(0)); // last ping the hub answered
    let (sink, stream) = ws.split();
    // outbound frames (acks + keepalive pings + CLI relays) go through one
    // channel so a single task owns the sink (a SplitSink cannot be shared).
    let (tx, rx) = mpsc::unbounded_channel::<Message>();
    // hand the live sender to the shared channel and flush what queued while
    // the WS was down (CLI relays); detach when this session ends. The sink
    // task converts text frames to Messages; the channel speaks plain strings.
    let (text_tx, mut text_rx) = mpsc::unbounded_channel::<String>();
    chan.attach_socket(Some(text_tx)).await;
    let pump_tx = tx.clone();
    let pump = tokio::spawn(async move {
        while let Some(text) = text_rx.recv().await {
            if pump_tx.send(Message::Text(text.into())).is_err() {
                break; // writer gone -> session over (the select! ends too)
            }
        }
    });

    tokio::select! {
        _ = write_frames_until_dead(cfg, sink, rx, log_rx) => {}
        _ = read_and_execute_commands(cfg, chan, stream, tx.clone(), alive_seq.clone()) => {}
        _ = send_keepalive_ping(tx.clone(), last_ping_seq.clone()) => {}
        _ = watch_for_dead_hub(last_ping_seq, alive_seq) => {}
    }

    chan.attach_socket(None).await; // this socket is gone: queue again
    pump.abort(); // the text->Message pump has no sink anymore
}

/// The ONLY owner of the sink: drains outbound frames (acks queued by the
/// reader first — a command answer is more urgent than a log line), forwards
/// central-log lines as {"log":...} frames, and on every cfg.interval pushes
/// a ReportState frame (the heartbeat — replaces HTTP POST /state). One
/// channel, one clock. Quiet at INFO unless armed/sessions CHANGES.
/// shortcut: frames emitted while the WS is DOWN are lost (stderr keeps them)
/// — add replay-on-reconnect if the hub must hold full history.
async fn write_frames_until_dead(
    cfg: &Arc<muxmesh_core::AgentConfig>,
    mut sink: SplitSink<Ws, Message>,
    mut rx: mpsc::UnboundedReceiver<Message>,
    mut log_rx: tokio::sync::broadcast::Receiver<String>,
) {
    let mut beat = tokio::time::interval(Duration::from_secs(cfg.interval.max(1)));
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last = (false, usize::MAX);
    loop {
        tokio::select! {
            biased;
            Some(msg) = rx.recv() => {
                if sink.send(msg).await.is_err() {
                    return; // socket error -> session over
                }
            }
            Ok(line) = log_rx.recv() => {
                let frame = format!("{{\"log\":{}}}", serde_json::json!(line));
                if sink.send(Message::Text(frame.into())).await.is_err() {
                    return;
                }
            }
            _ = beat.tick() => {
                let state = build_state(&cfg.host, &resolve_socket(cfg));
                let frame = serde_json::json!({
                    "v": 1,
                    "id": format!("s-{}", muxmesh_core::now_unix()),
                    "src": cfg.host,
                    "dst": "hub",
                    "ts": muxmesh_core::now_unix(),
                    "cmd": { "type": "ReportState", "state": state },
                });
                if sink.send(Message::Text(frame.to_string().into())).await.is_err() {
                    return; // socket error -> session over; reconnect resyncs
                }
                if (state.armed, state.sessions.len()) != last {
                    tracing::info!(
                        "armed={} sessions={} push=ok (ws)",
                        state.armed,
                        state.sessions.len()
                    );
                }
                last = (state.armed, state.sessions.len());
            }
        }
    }
}

/// Read pushed frames until the socket dies: execute commands addressed to us
/// (ack queued on `tx`), deposit directory answers on the shared channel's
/// rendez-vous, record pongs as liveness proof, end on Close/error.
async fn read_and_execute_commands(
    cfg: &Arc<muxmesh_core::AgentConfig>,
    chan: &Arc<Channel>,
    mut stream: SplitStream<Ws>,
    tx: mpsc::UnboundedSender<Message>,
    alive_seq: Arc<AtomicU64>,
) {
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                // directory answer first: it carries no envelope and must reach
                // the IPC waiter, not the command executor.
                if let Some(ans) = parse_directory_answer(text.as_str()) {
                    chan.rv.deposit(ans).await;
                    continue;
                }
                // role facts from the LOCAL scan (same data ReportState pushes):
                // who has a live ssh INTO us. Zero network, every command.
                let parents = muxmesh_core::tmux::ssh_parent_hosts();
                if let Some(ack) = execute_and_ack(cfg, text.as_str(), &parents) {
                    if tx.send(Message::Text(ack.into())).is_err() {
                        return;
                    }
                }
            }
            Ok(Message::Pong(data)) => {
                // tungstenite echoes the ping payload back verbatim: a pong
                // carrying our latest sent seq proves the hub is alive.
                let seq: u64 = String::from_utf8_lossy(&data).parse().unwrap_or(0);
                if seq != 0 {
                    alive_seq.fetch_max(seq, Ordering::Relaxed);
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
}

/// Parse a hub directory-answer frame `{"dir":{"me":..,"directory":{..}}}`.
/// None for commands/acks/log frames. Cheap substring pre-check first.
fn parse_directory_answer(text: &str) -> Option<DirectoryAnswer> {
    if !text.contains("\"dir\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    serde_json::from_value(v.get("dir")?.clone()).ok()
}

/// Outbound keepalive: one seq-stamped WS ping per PING_INTERVAL, so the
/// pong that comes back proves to watch_for_dead_hub that the hub is alive.
async fn send_keepalive_ping(tx: mpsc::UnboundedSender<Message>, last_ping_seq: Arc<AtomicU64>) {
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ping.tick().await;
        let seq = last_ping_seq.fetch_add(1, Ordering::Relaxed) + 1;
        if tx.send(Message::Ping(seq.to_string().into())).is_err() {
            return; // writer gone -> session over
        }
    }
}

/// Dead-hub watchdog: MAX_MISSED_PONGS pings sent without a matching pong
/// -> end the session so dial_hub_forever reconnects. A silent half-open
/// socket surfaces here, not as a read timeout (there is none).
async fn watch_for_dead_hub(last_ping_seq: Arc<AtomicU64>, alive_seq: Arc<AtomicU64>) {
    loop {
        tokio::time::sleep(PING_INTERVAL).await;
        let last = last_ping_seq.load(Ordering::Relaxed);
        if last.saturating_sub(alive_seq.load(Ordering::Relaxed)) >= MAX_MISSED_PONGS {
            tracing::warn!(
                "ws: {MAX_MISSED_PONGS} unanswered pings -> dropping connection (dead hub)"
            );
            return;
        }
    }
}

/// Pure command cycle: envelope -> ack JSON (no socket held here). Executes the
/// argv-scoped tmux call for a command addressed to us; dst mismatch, a pushed
/// ReportState (agent->hub only) or a bad frame -> no ack (None): the hub logs
/// the queue TTL drop instead.
///
/// Role guard: `inbound_parents` = host keys with a live ssh INTO us, from
/// OUR OWN local scan (the same data ReportState carries to the hub — zero
/// network here). A child ignores the parent's verb (GiveControl, meta
/// `ignore_by_child`); no inbound ssh visible -> we look like a parent -> the
/// verb is ours to execute. The hub routed by its directory; this re-checks
/// with local truth (WS-Refacto §9: the agent makes no HTTP calls).
pub fn execute_and_ack(
    cfg: &muxmesh_core::AgentConfig,
    text: &str,
    inbound_parents: &[String],
) -> Option<String> {
    let env: CommandEnvelope = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("ws: bad frame (not a command envelope): {e}");
            return None;
        }
    };
    if env.dst != cfg.host {
        tracing::warn!(
            "ws: ignoring command id={} dst={} (not me: {})",
            env.id,
            env.dst,
            cfg.host
        );
        return None;
    }
    // ReportState is agent->hub only: a hub that pushes one back (bug or
    // malice) is ignored — never executed, never acked.
    if env.cmd.is_report_state() {
        tracing::warn!("ws: ignoring inbound ReportState id={} (agent->hub only)", env.id);
        return None;
    }
    // Role guard, declarative (tmux_argv_meta): a child ignores the parent's
    // verb. Childhood = inbound_parents non-empty, passed by the caller from
    // our local ssh-parent scan (no network — see fn doc).
    let meta = env.cmd.tmux_argv_meta();
    if meta.ignore_by_child && !inbound_parents.is_empty() {
        tracing::warn!(
            "ws: ignoring {:?} id={} (I am a child — not my verb)",
            env.cmd,
            env.id
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
