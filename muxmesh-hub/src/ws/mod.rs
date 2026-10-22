//! Per-agent WebSocket session: the hub's only contact with agents, and it
//! is always the agent that dialed in. The hub pushes queued /cmd frames on
//! this socket, reads acks + log lines back, and detects dead peers by ping.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use muxmesh_core::{Ack, HostState};

use crate::state::{merge_state, record_ack, record_agent_log, AppState, Sender};

static WS_GEN: AtomicU64 = AtomicU64::new(0);

/// Liveness cadence: ping every 30 s, close after 2 unanswered.
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_MISSED_PONGS: u32 = 2;

/// Per-agent WS session: drain the mpsc queue to the sink, read acks into
/// state.json last_cmd, ping every 30 s, close after 2 missed pongs.
/// No read timeout — a quiet channel is alive; the pings prove it.
pub async fn handle_agent(mut socket: WebSocket, host: String, state: Arc<AppState>) {
    let gen = WS_GEN.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    {
        let mut ch = state.channels.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = ch.insert(host.clone(), Sender { tx, gen }) {
            tracing::info!(
                "/agent/ws host={host}: replacing existing connection (gen {} -> {gen})",
                old.gen
            );
        }
    }
    tracing::info!("/agent/ws host={host} connected (gen {gen})");

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let _ = ping.tick().await; // first tick fires immediately: skip it
    let mut missed: u32 = 0;

    loop {
        tokio::select! {
            // push queued commands to the agent (frames arrive via POST /cmd)
            maybe_frame = rx.recv() => {
                match maybe_frame {
                    Some(frame) => {
                        if socket.send(Message::Text(frame.into())).await.is_err() {
                            tracing::warn!("/agent/ws host={host}: send failed");
                            break;
                        }
                    }
                    // sender dropped (replaced session): stop touching this socket
                    None => break,
                }
            }
            // acks and pongs from the agent. tungstenite answers our pings via
            // the client's auto-pong and delivers Pong HERE; a wedged half-open
            // socket never pongs -> missed keeps climbing.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // order matters: a ReportState frame also parses as an
                        // Ack (serde ignores unknown fields), so check it FIRST.
                        if let Some(st) = report_state(&text) {
                            // agent -> hub state push (replaces HTTP POST /state).
                            // Identity = the authenticated WS host, never the
                            // payload's own host field.
                            let _g = state.write_lock.lock().unwrap_or_else(|e| e.into_inner());
                            match merge_state(&state.state_path, &host, &st) {
                                Ok(trace) => tracing::info!(
                                    "/agent/ws host={host} state {trace}"
                                ),
                                Err(e) => tracing::warn!(
                                    "/agent/ws host={host}: state merge: {e}"
                                ),
                            }
                        } else {
                            match serde_json::from_str::<Ack>(&text) {
                            Ok(ack) => {
                                tracing::info!(
                                    "/agent/ws host={host} ack id={} ok={}{}",
                                    ack.id,
                                    ack.ok,
                                    ack.err.as_deref().map(|e| format!(" err={e}")).unwrap_or_default()
                                );
                                record_ack(&state, &host, &ack);
                            }
                            // NOT an ack: a forwarded log line from the agent's
                            // tracing layer ({"log":...}). Append verbatim to the
                            // central hub.log — one file, every machine.
                            Err(_) => {
                                if let Some(line) = crate::state::agent_log_line(&text) {
                                    record_agent_log(&state, &host, &line);
                                } else {
                                    tracing::debug!("/agent/ws host={host}: non-ack frame ignored");
                                }
                            }
                            }
                        }
                    }
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

/// Parse an agent state-push frame `{"cmd":{"type":"ReportState","state":{..}}}`.
/// None for acks/log frames. Cheap pre-check on the substring before the
/// full parse — a state frame is identified by its verb, nothing else.
fn report_state(text: &str) -> Option<HostState> {
    if !text.contains("\"ReportState\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("cmd")?.get("type")?.as_str()? != "ReportState" {
        return None;
    }
    serde_json::from_value(v.get("cmd")?.get("state")?.clone()).ok()
}
