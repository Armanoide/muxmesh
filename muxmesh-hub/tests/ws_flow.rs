//! Regression test: axum hub /agent/ws + POST /cmd flow over loopback.
//! Drives the real hub binary; a fake WS agent (tokio-tungstenite client)
//! connects + authenticates, then records what the hub pushes on its channel
//! and acks.
//!
//! The hub loads tokens.json ONCE at startup, so all tokens are minted
//! offline BEFORE spawning.

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::client_async_with_config;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::Message;

/// The WS path the axum hub serves (canonical). Legacy /ws is also routed.
const WS_PATH: &str = "/agent/ws";

/// Read-timeout for test frames: a missing frame must FAIL the test fast,
/// never hang it. Client-side only — the hub has no socket timeouts.
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

fn hub_bin() -> PathBuf {
    let mut p = std::env::current_exe().expect("exe"); // target/debug/deps/
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join(if cfg!(windows) {
        "muxmesh-hub.exe"
    } else {
        "muxmesh-hub"
    })
}

fn hub_cfg(dir: &Path) -> Command {
    let mut c = Command::new(hub_bin());
    c.env("MUXMESH_CONFIG_DIR", dir.join("cfg"))
        .env("MUXMESH_STATE", dir.join("state.json"))
        .env("MUXMESH_TOKENS", dir.join("tokens.json"))
        .env("MUXMESH_TTL", "30");
    c
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn post_http(addr: &str, path: &str, host: &str, token: &str, body: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nX-Muxmesh-Host: {host}\r\nX-Muxmesh-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let mut msg = req.into_bytes();
    msg.extend_from_slice(body.as_bytes());
    stream.write_all(&msg).await.unwrap();
    stream.flush().await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(RECV_TIMEOUT, stream.read_to_end(&mut raw))
        .await
        .expect("response within timeout")
        .unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let code = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (code, text)
}

struct Hub {
    child: Option<Child>,
    dir: PathBuf,
    bind: String,
}

impl Hub {
    /// Fresh temp dir. Call `mint` for every host BEFORE `spawn` (the hub
    /// loads tokens.json once at startup).
    fn new() -> Self {
        let port = free_port();
        let dir = std::env::temp_dir().join(format!("muxmesh-hub-it-{port}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Hub {
            child: None,
            dir,
            bind: String::new(),
        }
    }

    /// Offline mint (console mode): prints the ONE machine token once.
    fn mint(&self, host: &str) -> String {
        let out = hub_cfg(&self.dir)
            .args(["generate-token", host])
            .output()
            .expect("generate-token");
        assert!(out.status.success(), "generate-token {host}");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let machine = text.lines().next().expect("machine token").to_string();
        // exactly ONE token printed — no relay tokens anymore
        assert_eq!(
            text.lines().count(),
            1,
            "generate-token must print exactly one token, got: {text:?}"
        );
        assert!(
            !self.dir.join("relay").exists(),
            "no relay/ dir must ever be written"
        );
        machine
    }

    fn spawn(&mut self) {
        self.bind = format!("127.0.0.1:{}", free_port());
        let child = hub_cfg(&self.dir)
            .env("MUXMESH_BIND", &self.bind)
            // default poll_hub = own bind -> the embedded badge poller would
            // GET /state forever; point it at a dead port instead.
            .env("MUXMESH_POLL_HUB", "127.0.0.1:1")
            .env("RUST_LOG", "debug")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn hub");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if TcpStream::connect(&self.bind).is_ok() {
                break;
            }
            if Instant::now() > deadline {
                panic!("hub did not bind {}", self.bind);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.child = Some(child);
    }

    async fn post_cmd(&self, token: &str, body: &str) -> (u16, String) {
        // src comes from the envelope; the header must match the token's
        // identity (the hub rejects src != authenticated name). Parse it.
        let src = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("src").and_then(|s| s.as_str().map(String::from)))
            .unwrap_or_else(|| "driver".into());
        post_http(&self.bind, "/cmd", &src, token, body).await
    }

    /// Fake WS agent: dial /agent/ws with host+token headers. Returns the
    /// live client (split sink/stream); the caller reads pushed frames and
    /// sends acks. Every recv is wrapped in RECV_TIMEOUT so a missing frame
    /// fails the test instead of hanging it.
    async fn ws_connect(&self, host: &str, token: &str) -> Ws {
        self.ws_connect_path(WS_PATH, host, token).await
    }

    async fn ws_connect_path(&self, path: &str, host: &str, token: &str) -> Ws {
        let url = format!("ws://{}{}", self.bind, path);
        let mut req: Request = url.into_client_request().expect("ws request");
        {
            let h = req.headers_mut();
            h.insert("X-Muxmesh-Host", host.parse().unwrap());
            h.insert("X-Muxmesh-Token", token.parse().unwrap());
        }
        let stream = tokio::net::TcpStream::connect(&self.bind)
            .await
            .expect("tcp connect");
        let (ws, _resp) = client_async_with_config(req, stream, None)
            .await
            .expect("ws handshake");
        ws
    }

    /// Raw /agent/ws handshake attempt (for the 401-before-upgrade assertion).
    async fn ws_handshake_raw(&self, host: &str, token: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(&self.bind)
            .await
            .expect("connect");
        // Full head incl. the blank line; axum parses it and answers 401
        // (never a 101) when the auth headers are wrong.
        let req = format!(
            "GET {WS_PATH} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nX-Muxmesh-Host: {host}\r\nX-Muxmesh-Token: {token}\r\n\r\n",
            self.bind
        );
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        s.write_all(req.as_bytes()).await.unwrap();
        s.flush().await.unwrap();
        // axum answers the bad-cred upgrade with 401 but may keep the idle
        // keep-alive connection open -> read until the status line is in,
        // not until EOF.
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = tokio::time::Instant::now() + RECV_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, s.read(&mut buf)).await {
                Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
                Ok(Ok(n)) => {
                    raw.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&raw).contains("\r\n\r\n") || raw.len() > 8192 {
                        break;
                    }
                }
            }
        }
        String::from_utf8_lossy(&raw).into_owned()
    }

    /// Read one TEXT frame, failing after RECV_TIMEOUT. Pings/pongs from the
    /// hub are consumed transparently (tungstenite auto-pongs inside next()).
    async fn recv_text(ws: &mut Ws) -> String {
        loop {
            let msg = tokio::time::timeout(RECV_TIMEOUT, ws.next())
                .await
                .expect("frame within timeout")
                .expect("stream alive")
                .expect("valid frame");
            match msg {
                Message::Text(t) => return t.to_string(),
                Message::Close(_) => panic!("channel closed while awaiting a frame"),
                _ => {} // ping/pong/binary: keep reading
            }
        }
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn envelope(id: &str, dst: &str) -> String {
    format!("{{\"v\":1,\"id\":\"{id}\",\"src\":\"driver\",\"dst\":\"{dst}\",\"cmd\":{{\"type\":\"BarHide\"}},\"ts\":1}}")
}

type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

#[tokio::test(flavor = "multi_thread")]
async fn hub_ws_flow() {
    let dst = "agent-a";
    let mut hub = Hub::new();
    let tok_drv = hub.mint("driver");
    let tok_dst = hub.mint(dst);
    hub.spawn();

    // 1) bad driver token -> 401 (unchanged auth path)
    let (code, _) = hub
        .post_cmd("bogus-token-000000", &envelope("c-1", dst))
        .await;
    assert_eq!(code, 401, "unknown driver must 401");

    // 2) valid driver, dst missing (serde default empty) -> 400, no panic
    let body =
        "{\"v\":1,\"id\":\"c-2\",\"src\":\"driver\",\"cmd\":{\"type\":\"BarHide\"},\"ts\":1}";
    let (code, _) = hub.post_cmd(&tok_drv, body).await;
    assert_eq!(code, 400, "missing dst must 400");

    // 3) dst registered but NOT WS-connected -> 409 target not connected
    let (code, text) = hub.post_cmd(&tok_drv, &envelope("c-3", dst)).await;
    assert_eq!(code, 409, "offline dst must 409");
    assert!(text.contains("target not connected"), "body: {text}");

    // 4) bad token -> 401 BEFORE the upgrade (no 101 ever sent).
    let raw = hub.ws_handshake_raw(dst, "wrong-token-00000").await;
    assert!(
        raw.starts_with("HTTP/1.1 401"),
        "must 401 pre-upgrade: {raw}"
    );
    assert!(
        !raw.starts_with("HTTP/1.1 101"),
        "no upgrade on bad token: {raw}"
    );

    // 5) fake agent connects + authenticates with its OWN machine token
    let mut agent = hub.ws_connect(dst, &tok_dst).await;

    // 6) POST /cmd -> 202 queued + frame pushed on the agent's channel
    let (code, text) = hub.post_cmd(&tok_drv, &envelope("c-6", dst)).await;
    assert_eq!(code, 202, "connected dst must queue: {text}");
    assert!(
        text.contains("\"queued\":true") && text.contains("c-6"),
        "body: {text}"
    );
    let frame = Hub::recv_text(&mut agent).await;
    assert!(frame.contains("\"id\":\"c-6\""), "frame id: {frame}");
    assert!(
        frame.contains("\"type\":\"BarHide\""),
        "frame verb: {frame}"
    );
    assert!(frame.contains("\"dst\":\"agent-a\""), "frame dst: {frame}");

    // 7) agent acks -> hub logs it and mirrors last_cmd into state.json
    agent
        .send(Message::Text("{\"v\":1,\"id\":\"c-6\",\"ok\":true}".into()))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let doc: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(hub.dir.join("state.json")).unwrap_or_default(),
        )
        .unwrap_or(serde_json::Value::Null);
        if doc["hosts"][dst]["last_cmd"]["id"] == "c-6" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ack never mirrored into state.json"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 8) agent socket closes -> channel removed -> next /cmd is 409 again
    let _ = agent.close(None).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (code, _) = hub.post_cmd(&tok_drv, &envelope("c-8", dst)).await;
        if code == 409 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "channel never removed (code {code})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 9) unregistered dst -> 409 (token-decoupled design: no separate
    // registered check — dst must hold a LIVE channel; an unknown name has
    // none, so it is indistinguishable from "offline" and answers 409).
    let (code, text) = hub.post_cmd(&tok_drv, &envelope("c-9", "ghost")).await;
    assert_eq!(code, 409, "unknown dst must 409 (no live channel)");
    assert!(text.contains("not connected"), "body: {text}");

    // 10) legacy path /ws (old agents dial it): same handler, same auth gate
    let mut agent2 = hub.ws_connect_path("/ws", dst, &tok_dst).await;
    let (code, _) = hub.post_cmd(&tok_drv, &envelope("c-10", dst)).await;
    assert_eq!(code, 202, "legacy /ws channel must queue");
    let frame = Hub::recv_text(&mut agent2).await;
    assert!(frame.contains("\"id\":\"c-10\""), "frame: {frame}");

    // 11) reconnect + replay of the same id on /agent/ws: pushed again (no    // dedup — the agent's job, documented).
    let mut agent3 = hub.ws_connect(dst, &tok_dst).await;
    let (code, _) = hub.post_cmd(&tok_drv, &envelope("c-6", dst)).await;
    assert_eq!(code, 202, "replay queues again");
    let frame = Hub::recv_text(&mut agent3).await;
    assert!(
        frame.contains("\"id\":\"c-6\""),
        "same id re-pushed: {frame}"
    );

    // 12) RENAMED MACHINE, SAME TOKEN: the token is the identity, the name is
    // a label. agent-a dials /agent/ws declaring "agent-renamed" with the
    // SAME token -> 101, and /cmd addressed to "agent-renamed" queues.
    // No re-mint needed; the old label is replaced on connect (gen guard).
    let mut renamed = hub.ws_connect("agent-renamed", &tok_dst).await;
    let (code, _) = hub
        .post_cmd(&tok_drv, &envelope_src("c-12", "driver", "agent-renamed"))
        .await;
    assert_eq!(code, 202, "renamed dst (same token) must queue: {code}");
    let frame = Hub::recv_text(&mut renamed).await;
    assert!(frame.contains("\"id\":\"c-12\""), "renamed push: {frame}");
}

/// envelope with explicit src (rename tests).
fn envelope_src(id: &str, src: &str, dst: &str) -> String {
    format!("{{\"v\":1,\"id\":\"{id}\",\"src\":\"{src}\",\"dst\":\"{dst}\",\"cmd\":{{\"type\":\"BarHide\"}},\"ts\":1}}")
}

/// Regression guard for the 5.07s killer: a QUIET channel (no frames, no
/// traffic) must stay alive well past the old 5 s accept-loop read timeout.
/// >8 s of silence with a live POST /cmd answer = the persistent-channel bug
/// is gone. The hub pings every 30 s; tungstenite auto-pongs inside recv.
#[tokio::test(flavor = "multi_thread")]
async fn quiet_channel_survives_past_old_timeout() {
    let dst = "agent-q";
    let mut hub = Hub::new();
    let tok_drv = hub.mint("driver");
    let tok_dst = hub.mint(dst);
    hub.spawn();

    let mut agent = hub.ws_connect(dst, &tok_dst).await;

    // stay quiet for 8.5 s — the old hub dropped every channel at ~5.07 s
    tokio::time::sleep(Duration::from_millis(8_500)).await;

    // the channel must still be live: /cmd queues AND the frame arrives
    let (code, text) = hub.post_cmd(&tok_drv, &envelope("c-quiet", dst)).await;
    assert_eq!(code, 202, "quiet channel must still queue: {text}");
    let frame = Hub::recv_text(&mut agent).await;
    assert!(
        frame.contains("\"id\":\"c-quiet\""),
        "frame after silence: {frame}"
    );
}
