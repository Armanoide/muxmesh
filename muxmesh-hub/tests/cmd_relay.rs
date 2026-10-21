//! Verifier regression test: hub POST /cmd relay flow over loopback.
//! Drives the real hub binary; a fake target agent (std::net) records what
//! the hub actually forwarded. The hub loads tokens.json ONCE at startup,
//! so all tokens are minted offline BEFORE spawning.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

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

fn hub_cfg(dir: &Path, cmd_port: u16) -> Command {
    let mut c = Command::new(hub_bin());
    c.env("MUXMESH_CONFIG_DIR", dir.join("cfg"))
        .env("MUXMESH_STATE", dir.join("state.json"))
        .env("MUXMESH_TOKENS", dir.join("tokens.json"))
        .env("MUXMESH_HUB_CMD_PORT", cmd_port.to_string())
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
    s.write_all(req.as_bytes()).unwrap();
    s.flush().unwrap();
    s.write_all(body.as_bytes()).unwrap();
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
    cmd_port: u16,
    driver_token: String,
}

impl Hub {
    /// Fresh temp dir + hub bound to a free port. Call `mint` for every host
    /// BEFORE `spawn` (the hub loads tokens.json once at startup).
    fn new() -> Self {
        let port = free_port();
        let dir = std::env::temp_dir().join(format!("muxmesh-hub-it-{port}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Hub {
            child: None,
            dir,
            bind: String::new(),
            cmd_port: port,
            driver_token: String::new(),
        }
    }

    /// Offline mint (console mode): prints machine + relay tokens once and
    /// writes relay/<host>.token. Returns (machine, relay).
    fn mint(&self, host: &str) -> (String, String) {
        let out = hub_cfg(&self.dir, self.cmd_port)
            .args(["generate-token", host])
            .output()
            .expect("generate-token");
        assert!(out.status.success(), "generate-token {host}");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut lines = text.lines();
        let machine = lines.next().expect("machine token").to_string();
        let relay = lines.next().expect("relay token").to_string();
        let file = self.relay_file(host);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap().trim(),
            relay,
            "relay file must match the printed relay token"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600,
                "relay file mode"
            );
        }
        (machine, relay)
    }

    fn spawn(&mut self) {
        self.bind = format!("127.0.0.1:{}", free_port());
        let child = hub_cfg(&self.dir, self.cmd_port)
            .env("MUXMESH_BIND", &self.bind)
            // default poll_hub = own bind -> the embedded badge poller would
            // GET /state forever; point it at a dead port instead.
            .env("MUXMESH_POLL_HUB", "127.0.0.1:1")
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

    fn arm_host(&self, host: &str, token: &str) {
        let body = format!(
            "{{\"host\":\"{host}\",\"armed\":true,\"seen\":0,\"sessions\":[{{\"name\":\"main\",\"windows\":1,\"active_window\":\"zsh\"}}]}}"
        );
        let (code, resp) = post_http(&self.bind, "/state", host, token, &body);
        assert_eq!(code, 200, "arming {host}: {resp}");
    }

    fn relay_file(&self, host: &str) -> PathBuf {
        self.dir.join("relay").join(format!("{host}.token"))
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

/// Fake agent: accepts one connection at a time, records (token, host header,
/// body) of each POST /cmd, answers a 200 Ack echoing the envelope id.
fn fake_agent(addr: &str) -> mpsc::Receiver<(String, String, String)> {
    let l = TcpListener::bind(addr).expect("bind fake agent");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for stream in l.incoming() {
            let tx = tx.clone();
            thread::spawn(move || {
                let mut s = stream.unwrap();
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = vec![0u8; 8192];
                let mut head: Vec<u8> = Vec::new();
                loop {
                    let n = match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    head.extend_from_slice(&buf[..n]);
                    let Some(pos) = head.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let text = String::from_utf8_lossy(&head[..pos]).into_owned();
                    let mut content_len = 0usize;
                    let mut token = String::new();
                    let mut host_hdr = String::new();
                    for line in text.lines() {
                        let Some((k, v)) = line.split_once(':') else {
                            continue;
                        };
                        let k = k.trim().to_ascii_lowercase();
                        let v = v.trim().to_string(); // value keeps its case (tokens)
                        match k.as_str() {
                            "content-length" => content_len = v.parse().unwrap_or(0),
                            "x-muxmesh-token" => token = v,
                            "x-muxmesh-host" => host_hdr = v,
                            _ => {}
                        }
                    }
                    let mut body: Vec<u8> = head[pos + 4..].to_vec();
                    while body.len() < content_len {
                        match s.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => body.extend_from_slice(&buf[..n]),
                        }
                    }
                    let id = String::from_utf8_lossy(&body)
                        .split("\"id\":\"")
                        .nth(1)
                        .map(|s| s.split('"').next().unwrap_or("?").to_string())
                        .unwrap_or_else(|| "?".into());
                    let _ = tx.send((token, host_hdr, String::from_utf8_lossy(&body).into_owned()));
                    let ack = format!("{{\"v\":1,\"id\":\"{id}\",\"ok\":true}}");
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        ack.len()
                    );
                    let _ = s.write_all(resp.as_bytes());
                    let _ = s.write_all(ack.as_bytes());
                    break;
                }
            });
        }
    });
    rx
}

fn envelope(id: &str, dst: &str) -> String {
    format!("{{\"v\":1,\"id\":\"{id}\",\"src\":\"driver\",\"dst\":\"{dst}\",\"cmd\":{{\"type\":\"BarHide\"}},\"ts\":1}}")
}

#[test]
fn hub_cmd_relay_flow() {
    let dst = "127.0.0.1";
    let mut hub = Hub::new();
    let (tok_drv, _relay_drv) = hub.mint("driver");
    hub.driver_token = tok_drv.clone();
    let (tok_dst, relay_dst) = hub.mint(dst);
    hub.spawn();

    // 1) unknown driver token -> 401
    let (code, _) = hub.post_cmd("bogus-token-000000", &envelope("c-1", dst));
    assert_eq!(code, 401, "unknown driver must 401");

    // 2) valid driver, dst missing (serde default empty) -> 400, no panic
    let body =
        "{\"v\":1,\"id\":\"c-2\",\"src\":\"driver\",\"cmd\":{\"type\":\"BarHide\"},\"ts\":1}";
    let (code, _) = hub.post_cmd(&hub.driver_token, body);
    assert_eq!(code, 400, "missing dst must 400");

    // 3) valid driver, dst not armed -> 403
    let (code, text) = hub.post_cmd(&hub.driver_token, &envelope("c-3", dst));
    assert_eq!(code, 403, "unarmed dst must 403");
    assert!(text.contains("target not armed"), "body: {text}");

    // arm dst
    hub.arm_host(dst, &tok_dst);

    // 4) armed but relay file absent -> 502 + hint naming dst
    let relay_path = hub.relay_file(dst);
    let stash = hub.dir.join("relay.stash");
    std::fs::rename(&relay_path, &stash).unwrap();
    let (code, text) = hub.post_cmd(&hub.driver_token, &envelope("c-4", dst));
    assert_eq!(code, 502, "no relay file must 502");
    assert!(
        text.contains(dst) && text.contains("generate-token"),
        "hint: {text}"
    );
    std::fs::rename(&stash, &relay_path).unwrap();

    // 5) full relay: hub forwards to dst:cmd_port with X-Muxmesh-Host: hub
    // and the per-host relay secret; the agent ack passes through.
    let rx = fake_agent(&format!("127.0.0.1:{}", hub.cmd_port));
    let (code, text) = hub.post_cmd(&hub.driver_token, &envelope("c-5", dst));
    assert_eq!(code, 200, "ack must pass through: {text}");
    assert!(
        text.contains("\"ok\":true") && text.contains("c-5"),
        "ack: {text}"
    );
    let (fwd_token, fwd_host, got_body) =
        rx.recv_timeout(Duration::from_secs(3)).expect("agent recv");
    assert_eq!(fwd_host, "hub", "hub re-signs as hub");
    assert_eq!(
        fwd_token,
        relay_dst.trim(),
        "hub forwards the dst relay secret"
    );
    assert_ne!(
        fwd_token, hub.driver_token,
        "driver token never reaches the agent"
    );
    assert!(
        got_body.contains("\"dst\":\"127.0.0.1\""),
        "envelope kept: {got_body}"
    );

    // 6) replay of the same cmd id through the hub: relayed again (the hub
    // does NOT dedup — documented; any dedup is the agent's job).
    let (code, _) = hub.post_cmd(&hub.driver_token, &envelope("c-5", dst));
    assert_eq!(code, 200, "replay relayed again");
    let (_, _, replay_body) = rx
        .recv_timeout(Duration::from_secs(3))
        .expect("agent recv replay");
    assert!(
        replay_body.contains("\"id\":\"c-5\""),
        "same id re-relayed: {replay_body}"
    );

    // 7) driver == dst (self-command): NOT explicitly rejected by the hub.
    // The armed+relay checks pass for dst=driver (hub logs show it reaches
    // the relay stage); DNS then fails in this sandbox ("driver" is not a
    // resolvable name), which proves the hub attempted the relay.
    hub.arm_host("driver", &tok_drv);
    let (code, text) = hub.post_cmd(&hub.driver_token, &envelope("c-7", "driver"));
    assert_ne!(code, 403, "armed self must not be rejected as unarmed");
    assert_ne!(code, 401, "armed self must not be rejected as unauthorized");
    assert_eq!(code, 502, "relay attempted for self: {text}");
    assert!(
        text.contains("relay to driver"),
        "hub attempted relay: {text}"
    );
}
