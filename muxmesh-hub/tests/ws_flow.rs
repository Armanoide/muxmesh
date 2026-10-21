//! Verifier regression test: hub /ws + POST /cmd flow over loopback.
//! Drives the real hub binary; a fake WS agent (tungstenite client) connects
//! + authenticates, then records what the hub pushes on its channel and acks.
//!
//! The hub loads tokens.json ONCE at startup, so all tokens are minted
//! offline BEFORE spawning.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tungstenite::client::IntoClientRequest;
use tungstenite::{connect, Message};

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

fn post_http(addr: &str, path: &str, host: &str, token: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nX-Muxmesh-Host: {host}\r\nX-Muxmesh-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    // one segment = head + body: the hub's BufReader would otherwise buffer
    // the body and the separate-segment write could land after its FIN.
    let mut msg = req.into_bytes();
    msg.extend_from_slice(body.as_bytes());
    s.write_all(&msg).unwrap();
    s.flush().unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
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
    driver_token: String,
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
            driver_token: String::new(),
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

    fn post_cmd(&self, token: &str, body: &str) -> (u16, String) {
        post_http(&self.bind, "/cmd", "driver", token, body)
    }

    // arm_host (POST /state) intentionally unused here: connectivity is the
    // WS channel, not the armed flag (see handle_cmd).

    /// Fake WS agent: dial /ws with host+token headers. Returns the live
    /// client; the caller reads pushed frames and sends acks. The underlying
    /// socket gets a 5 s read timeout so a missing frame fails the test
    /// instead of hanging it.
    fn ws_connect(&self, host: &str, token: &str) -> Ws {
        let url = format!("ws://{}/ws", self.bind);
        let mut req = url.into_client_request().expect("ws request");
        {
            let h = req.headers_mut();
            h.insert("X-Muxmesh-Host", host.parse().unwrap());
            h.insert("X-Muxmesh-Token", token.parse().unwrap());
        }
        let (ws, _resp) = connect(req).expect("ws connect");
        if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_ref() {
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        }
        ws
    }

    /// Raw /ws handshake attempt (for the 401-before-upgrade assertion).
    fn ws_handshake_raw(&self, host: &str, token: &str) -> String {
        let mut s = TcpStream::connect(&self.bind).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
        // Full head incl. the blank line: the hub's read_head terminates on
        // it, so omitting it would make the hub wait and close empty.
        let req = format!(
            "GET /ws HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nX-Muxmesh-Host: {host}\r\nX-Muxmesh-Token: {token}\r\n\r\n",
            self.bind
        );
        s.write_all(req.as_bytes()).unwrap();
        s.flush().unwrap();
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).unwrap();
        String::from_utf8_lossy(&raw).into_owned()
    }

    fn read_frame_with_timeout(ws: &mut Ws, secs: u64) -> Option<String> {
        // the socket already has a 5 s read timeout (set at connect)
        let _ = secs;
        read_frame(ws)
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

type Ws = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

fn read_frame(ws: &mut Ws) -> Option<String> {
    match ws.read() {
        Ok(Message::Text(t)) => Some(t.to_string()),
        _ => None,
    }
}

#[test]
fn hub_ws_flow() {
    let dst = "agent-a";
    let mut hub = Hub::new();
    let tok_drv = hub.mint("driver");
    hub.driver_token = tok_drv.clone();
    let tok_dst = hub.mint(dst);
    hub.spawn();

    // 1) bad driver token -> 401 (unchanged auth path)
    let (code, _) = hub.post_cmd("bogus-token-000000", &envelope("c-1", dst));
    assert_eq!(code, 401, "unknown driver must 401");

    // 2) valid driver, dst missing (serde default empty) -> 400, no panic
    let body =
        "{\"v\":1,\"id\":\"c-2\",\"src\":\"driver\",\"cmd\":{\"type\":\"BarHide\"},\"ts\":1}";
    let (code, _) = hub.post_cmd(&hub.driver_token, body);
    assert_eq!(code, 400, "missing dst must 400");

    // 3) dst registered but NOT WS-connected -> 409 target not connected
    let (code, text) = hub.post_cmd(&hub.driver_token, &envelope("c-3", dst));
    assert_eq!(code, 409, "offline dst must 409");
    assert!(text.contains("target not connected"), "body: {text}");

    // 4) bad token -> 401 BEFORE the upgrade (no 101 ever sent)
    let raw = hub.ws_handshake_raw(dst, "wrong-token-00000");
    assert!(
        raw.starts_with("HTTP/1.1 401"),
        "must 401 pre-upgrade: {raw}"
    );
    assert!(!raw.contains("101"), "no upgrade on bad token: {raw}");

    // 5) fake agent connects + authenticates with its OWN machine token
    let mut agent = hub.ws_connect(dst, &tok_dst);

    // 6) POST /cmd -> 202 queued + frame pushed on the agent's channel
    let (code, text) = hub.post_cmd(&hub.driver_token, &envelope("c-6", dst));
    assert_eq!(code, 202, "connected dst must queue: {text}");
    assert!(
        text.contains("\"queued\":true") && text.contains("c-6"),
        "body: {text}"
    );
    let frame = Hub::read_frame_with_timeout(&mut agent, 5).expect("agent recv frame");
    assert!(frame.contains("\"id\":\"c-6\""), "frame id: {frame}");
    assert!(
        frame.contains("\"type\":\"BarHide\""),
        "frame verb: {frame}"
    );
    assert!(frame.contains("\"dst\":\"agent-a\""), "frame dst: {frame}");

    // 7) agent acks -> hub logs it and mirrors last_cmd into state.json
    agent
        .send(Message::Text("{\"v\":1,\"id\":\"c-6\",\"ok\":true}".into()))
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
        std::thread::sleep(Duration::from_millis(100));
    }

    // 8) agent socket closes -> channel removed -> next /cmd is 409 again
    let _ = agent.close(None);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (code, _) = hub.post_cmd(&hub.driver_token, &envelope("c-8", dst));
        if code == 409 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "channel never removed (code {code})"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // 9) unregistered dst -> 403 (registered check precedes connectivity)
    let (code, text) = hub.post_cmd(&hub.driver_token, &envelope("c-9", "ghost"));
    assert_eq!(code, 403, "unregistered dst must 403");
    assert!(text.contains("not registered"), "body: {text}");

    // 10) reconnect + replay of the same id: pushed again (no dedup — the
    // agent's job, documented).
    let mut agent2 = hub.ws_connect(dst, &tok_dst);
    let (code, _) = hub.post_cmd(&hub.driver_token, &envelope("c-6", dst));
    assert_eq!(code, 202, "replay queues again");
    let frame = Hub::read_frame_with_timeout(&mut agent2, 5).expect("agent recv replay");
    assert!(
        frame.contains("\"id\":\"c-6\""),
        "same id re-pushed: {frame}"
    );
}
