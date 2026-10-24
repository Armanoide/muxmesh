//! Per-agent WebSocket session: the hub's only contact with agents, and it
//! is always the agent that dialed in. The hub pushes queued /cmd frames on
//! this socket, reads acks + state reports + log lines back, and detects dead
//! peers by ping.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use muxmesh_core::{Ack, HostState};

use crate::state::{merge_state, record_ack, record_agent_log, AppState, Sender};

static WS_GEN: AtomicU64 = AtomicU64::new(0);

/// Liveness cadence: ping every 30 s, close after 2 unanswered.
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_MISSED_PONGS: u32 = 2;

/// Register this agent's channel, then serve its socket until it dies: push
/// queued commands, read acks/state/logs back, ping every 30 s. No read
/// timeout — a quiet channel is alive; the pings prove it.
pub async fn serve_agent_until_disconnected(mut socket: WebSocket, host: String, state: Arc<AppState>) {
    let gen = WS_GEN.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // keep a handle to the raw sender: directory answers go straight onto
    // this socket (register_channel moves its clone into the channel map).
    let tx_handle = tx.clone();
    register_channel(&state, &host, tx, gen);
    tracing::info!("/agent/ws host={host} connected (gen {gen})");

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let _ = ping.tick().await; // first tick fires immediately: skip it
    let mut missed: u32 = 0;

    loop {
        tokio::select! {
            // push queued commands to the agent (frames arrive via POST /cmd)
            frame = rx.recv() => {
                match frame {
                    Some(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            tracing::warn!("/agent/ws host={host}: send failed");
                            break;
                        }
                    }
                    // sender dropped (replaced session): stop touching this socket
                    None => break,
                }
            }
            // acks, state reports and log lines from the agent. tungstenite
            // answers our pings via the client's auto-pong and delivers Pong
            // HERE; a wedged half-open socket never pongs -> missed climbs.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => read_agent_text(&state, &host, text.as_str(), &tx_handle).await,
                    Some(Ok(Message::Pong(_))) => missed = 0,
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {} // binary/ping from agent: ignored
                    Some(Err(e)) => {
                        tracing::warn!("/agent/ws host={host}: recv: {e}");
                        break;
                    }
                }
            }
            _ = ping.tick() => {
                missed += 1;
                if missed > MAX_MISSED_PONGS {
                    tracing::warn!("/agent/ws host={host}: {MAX_MISSED_PONGS} unanswered pings -> closing (dead peer)");
                    let _ = socket.send(Message::Close(Some(CloseFrame {
                        code: axum::extract::ws::close_code::NORMAL,
                        reason: "dead peer".into(),
                    }))).await;
                    break;
                }
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }

    // socket closed: remove OUR channel (a replacement connection keeps its own)
    {
        let mut ch = state.channels.lock().unwrap_or_else(|e| e.into_inner());
        if ch.get(&host).is_some_and(|s| s.gen == gen) {
            ch.remove(&host);
        }
    }
    tracing::info!("/agent/ws host={host} disconnected (gen {gen})");
}

/// Publish this connection's sender so POST /cmd can enqueue frames for it.
/// A reconnect replaces the old sender (gen tracks which one is live).
fn register_channel(
    state: &AppState,
    host: &str,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    gen: u64,
) {
    let mut ch = state.channels.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = ch.insert(host.to_string(), Sender { tx, gen }) {
        tracing::info!(
            "/agent/ws host={host}: replacing existing connection (gen {} -> {gen})",
            old.gen
        );
    }
}

/// Route one inbound text frame: state report → merge, ack → mirror, directory
/// question → answer on this same socket, else a forwarded log line → hub.log.
/// Order matters: a ReportState frame also parses as an Ack (serde ignores
/// unknown fields), so check it FIRST.
async fn read_agent_text(
    state: &AppState,
    host: &str,
    text: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<String>,
) {
    if let Some(st) = parse_report_state(text) {
        // agent -> hub state push (replaces HTTP POST /state). Identity = the
        // authenticated WS host, never the payload's own host field.
        let _g = state.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        match merge_state(&state.state_path, host, &st) {
            Ok(trace) => tracing::info!("/agent/ws host={host} state {trace}"),
            Err(e) => tracing::warn!("/agent/ws host={host}: state merge: {e}"),
        }
    } else if is_ask_directory(text) {
        // the agent's CLI wants the directory (tree / armed-host / why / F10).
        // Answer on the SAME socket — no GET /state anywhere in the fleet now.
        answer_directory(state, host, tx);
    } else if let Ok(ack) = serde_json::from_str::<Ack>(text) {
        tracing::info!(
            "/agent/ws host={host} ack id={} ok={}{}",
            ack.id,
            ack.ok,
            ack.err.as_deref().map(|e| format!(" err={e}")).unwrap_or_default()
        );
        record_ack(state, host, &ack);
    } else if text.contains("\"dst\"") {
        // a relayed command from a driver agent: route it to the dst agent's
        // live channel. Same enqueue path as POST /cmd (the WS is now the
        // only way a command reaches an agent).
        forward_command(state, host, text);
    } else if let Some(line) = crate::state::agent_log_line(text) {
        // NOT an ack: a forwarded log line from the agent's tracing layer
        // ({"log":...}). Append verbatim to hub.log — one file, every machine.
        record_agent_log(state, host, &line);
    } else {
        tracing::debug!("/agent/ws host={host}: non-ack frame ignored");
    }
}

/// Cheap pre-check: is this the agent's directory question `{"ask_directory":{}}`?
fn is_ask_directory(text: &str) -> bool {
    text.contains("\"ask_directory\"")
}

/// Answer an agent's directory question on its own WS: read state.json (the
/// hub's directory) and reply {"dir":{"me":<host>,"directory":{..}}}. The
/// agent waits on its rendez-vous; a failed send is logged, not fatal.
fn answer_directory(
    state: &AppState,
    host: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<String>,
) {
    let doc = crate::state::read_doc(&state.state_path);
    let ans = muxmesh_core::DirectoryAnswer { me: host.to_string(), directory: doc };
    let frame = format!("{{\"dir\":{}}}", serde_json::to_string(&ans).unwrap_or_default());
    if tx.send(frame).is_err() {
        tracing::warn!("/agent/ws host={host}: directory answer: channel gone");
    } else {
        tracing::debug!("/agent/ws host={host}: directory answered");
    }
}

/// Route a relayed command frame to its dst agent's live WS channel. The
/// envelope is re-serialized from the parsed value (never forwarded verbatim),
/// so a frame that cannot be parsed never reaches an agent. The sender is the
/// authenticated channel host; dst routing mirrors POST /cmd's enqueue path.
fn forward_command(state: &AppState, src: &str, text: &str) {
    let env: muxmesh_core::CommandEnvelope = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!("/agent/ws host={src}: command frame rejected: {e}");
            return;
        }
    };
    let dst = env.dst.clone();
    let enqueued = {
        let ch = state.channels.lock().unwrap_or_else(|e| e.into_inner());
        match ch.get(&dst) {
            Some(s) => match serde_json::to_string(&env) {
                Ok(frame) => s.tx.send(frame).is_ok(),
                Err(e) => {
                    tracing::error!("forward id={} serialize failed: {e}", env.id);
                    false
                }
            },
            None => false,
        }
    };
    if enqueued {
        tracing::info!("/agent/ws src={src} -> queued dst={dst} id={}", env.id);
    } else {
        tracing::warn!("/agent/ws src={src}: dst={dst} not connected — command dropped id={}", env.id);
    }
}

/// Parse an agent state-push frame `{"cmd":{"type":"ReportState","state":{..}}}`.
/// None for acks/log frames. Cheap pre-check on the substring before the
/// full parse — a state frame is identified by its verb, nothing else.
fn parse_report_state(text: &str) -> Option<HostState> {
    if !text.contains("\"ReportState\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("cmd")?.get("type")?.as_str()? != "ReportState" {
        return None;
    }
    serde_json::from_value(v.get("cmd")?.get("state")?.clone()).ok()
}
