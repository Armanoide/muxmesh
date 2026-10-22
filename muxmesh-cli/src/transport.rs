//! HTTP transport to the hub + identity resolution. std::net only.
//!
//! Everything that touches the wire lives here: the raw POST /cmd, hub
//! address parsing, and the driver's hub identity. The bar state machine
//! (`bar.rs`) never opens a socket or reads env vars directly.

use std::io::Read;
use std::io::Write;
use std::net::TcpStream;

use muxmesh_core::{Command, CommandEnvelope};

/// Millisecond-free unix seconds — envelope ids and timestamps.
pub fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// POST the envelope to the hub's /cmd over a plain TCP connection and read
/// the raw HTTP response. The hub NEVER dials an agent: this is the driver's
/// side of driver -> HUB -> agent.
pub fn send_cmd(
    host: &str,
    port: u16,
    token: &str,
    env: &CommandEnvelope,
) -> Result<String, String> {
    let addr = format!("{host}:{port}");
    let body = serde_json::to_vec(env).map_err(|e| e.to_string())?;
    let mut stream = TcpStream::connect(&addr).map_err(|e| format!("{addr}: {e}"))?;
    let req = format!(
        "POST /cmd HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nX-Muxmesh-Host: {}\r\nX-Muxmesh-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        env.src,
        body.len()
    );
    // headers flushed before the body: two segments (works with axum, kept).
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    stream.write_all(&body).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// Split "host:port" / "[v6]:port" into (host, port-or-default-9997).
pub fn split_host_port(addr: &str) -> (&str, u16) {
    let a = addr.trim();
    if let Some(rest) = a.strip_prefix('[') {
        if let Some((h, tail)) = rest.split_once(']') {
            let p = tail
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(9997);
            return (h, p);
        }
        return (a, 9997);
    }
    match a.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h, p.parse().unwrap_or(9997)),
        _ => (a, 9997),
    }
}

/// Hub command endpoint (host, port) from config.
pub fn hub_endpoint(cfg: &muxmesh_core::HubConfig) -> (String, u16) {
    let (h, p) = split_host_port(&cfg.poll_hub);
    (h.to_string(), p)
}

/// Driver token: MUXMESH_CMD_TOKEN overrides the on-disk machine token.
pub fn cmd_token() -> String {
    std::env::var("MUXMESH_CMD_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| muxmesh_core::read_token().as_str().to_string())
}

/// Identity for the hub: MUST equal the agent's canonical host name (the key
/// in tokens.json). NOT `hostname -s` — macOS mDNS advertises a different
/// name, which the hub never registered (401 on every POST /cmd).
/// Order: MUXMESH_HOST env > config.toml agent.host > hostname -s fallback.
pub fn driver_host() -> String {
    if let Ok(h) = std::env::var("MUXMESH_HOST") {
        let h = h.trim().to_string();
        if !h.is_empty() {
            return h;
        }
    }
    let map = muxmesh_core::load_map();
    let h = map.get("agent.host").cloned().unwrap_or_default();
    if !h.is_empty() {
        return h;
    }
    hostname_short()
}

fn hostname_short() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "client".into())
}

/// POST one verb to the hub for `dst`. True only on HTTP 202 (queued).
pub fn post_verb(verb: Command, dst: &str, label: &str, cfg: &muxmesh_core::HubConfig) -> bool {
    let (hub_host, hub_port) = hub_endpoint(cfg);
    let env = CommandEnvelope {
        v: 1,
        id: format!("c-{}", now_ts()),
        src: driver_host(),
        dst: dst.to_string(),
        cmd: verb,
        ts: now_ts(),
    };
    match send_cmd(&hub_host, hub_port, &cmd_token(), &env) {
        Ok(resp) if resp.starts_with("HTTP/1.1 202") => {
            tracing::info!("bar: hub {hub_host}:{hub_port} queued {label} for {dst} (pushed on its /ws channel)");
            true
        }
        other => {
            tracing::error!(
                "bar: hub {hub_host}:{hub_port} {label} for {dst} FAILED: {:?}",
                other.map(|r| r.lines().next().unwrap_or("").to_string())
            );
            false
        }
    }
}

/// Answer a press-2 back to the parent: find who armed an ssh INTO us (the
/// hub directory knows) and POST ParentReclaim via the hub. Best-effort —
/// never block the local bar toggle on the network.
pub fn answer_parent_reclaim(cfg: &muxmesh_core::HubConfig) {
    let me = driver_host();
    let doc = muxmesh_core::poll::fetch_state(&cfg.poll_hub);
    let parent = match muxmesh_core::tmux::parents_of(&doc, &me).into_iter().next() {
        Some(p) => p,
        None => {
            tracing::info!(
                "child: no host has a live ssh into '{me}' — local-only toggle, nothing to answer"
            );
            return;
        }
    };
    if post_verb(Command::ParentReclaim, &parent, "ParentReclaim", cfg) {
        tracing::info!("child: answered press-2 -> ParentReclaim queued for parent '{parent}' via hub");
    }
}
