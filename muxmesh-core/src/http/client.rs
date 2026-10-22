//! Minimal HTTP client for the hub's open endpoints (std::net only, no deps).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::StateDoc;

/// GET /state from the hub ("host:port"). Minimal HTTP/1.0 over std::net
/// (no extra deps). GET /state is open on the hub — no token needed.
pub fn get_state(hub_addr: &str) -> Result<StateDoc, String> {
    let mut stream = TcpStream::connect(hub_addr).map_err(|e| format!("{hub_addr}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let req = format!("GET /state HTTP/1.0\r\nHost: {hub_addr}\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;
    // capped read (1 MiB): a misdirected poll.hub must not exhaust memory.
    let mut raw = String::new();
    {
        let mut buf = [0u8; 8192];
        loop {
            let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            if raw.len() + n > 1_048_576 {
                return Err("response over 1 MiB (wrong hub?)".into());
            }
            raw.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    }
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| "malformed http response".to_string())?;
    let status = head.lines().next().unwrap_or("");
    if !(status.ends_with("200") || status.contains(" 200 ")) {
        return Err(format!("hub returned: {status}"));
    }
    StateDoc::from_json_str(body).map_err(|e| format!("bad state json: {e}"))
}
