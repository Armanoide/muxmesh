//! muxmesh-hub — state aggregator + command dispatcher (runs on the always-on
//! pivot machine).
//!
//! One axum server on ONE port (tokio runtime). Agents push their state as
//! ReportState frames on their own WS; the hub merges into state.json with
//! ATOMIC writes (tmp + rename) so readers (tmux status bar, CLI) never see
//! a half-written file. POST /state stays as a compat endpoint (old agents).
//!
//! Commands flow driver -> HUB -> agent over the agent's OWN outbound WebSocket
//! (/agent/ws): the hub NEVER dials an agent. Each agent holds a persistent WS
//! session, authenticated at the handshake from headers BEFORE the upgrade
//! (X-Muxmesh-Host + X-Muxmesh-Token = its own machine token — same registry
//! and KDF; 401 before any 101). A driver POSTs /cmd; the hub
//! authenticates it, checks dst is registered + has a LIVE ws channel right
//! now, enqueues the frame on that channel's mpsc sender and answers 202
//! queued immediately. The agent executes tmux locally and replies with an ack
//! frame {v,id,ok,err}, which the hub logs (and mirrors into state.json's
//! last_cmd so the driver can read it back via GET /state).
//!
//! Liveness: the hub pings each agent every 30 s; two consecutive unanswered
//! pings close the channel (dead-peer detection). There is NO socket read
//! timeout — a quiet agent is an idle agent, not a dead one.
//!
//! Layout (lm-sanaga conventions):
//!   state/       persistence + shared AppState (the only fs writers)
//!   interfaces/  HTTP routes + auth gate (the only axum handlers)
//!   ws/          per-agent WS session loop
//!   main.rs      bootstrap: config, registry, router, generate-token

mod interfaces;
mod state;
mod ws;

use std::collections::HashMap;
use std::process;
use std::sync::{Arc, Mutex};

use axum::{
    middleware,
    routing::{get, post},
    Router,
};
use muxmesh_core::{valid_host, HubConfig, TokenStore};

use crate::interfaces::{auth::ws_auth_gate, route};
use crate::state::{log_path_for, purge_and_write, write_file_atomic, AppState};

fn main() {
    // Config: env > ~/.config/muxmesh/config.toml > default.
    let args: Vec<String> = std::env::args().skip(1).collect();
    // -v/--verbose anywhere -> RUST_LOG=muxmesh=debug (explicit RUST_LOG wins).
    let args = muxmesh_core::log::parse_verbose_flag(&args);
    muxmesh_core::log::init();
    let cfg = HubConfig::load();

    // Console mode (herdr-style `add machine`): mint a token for a host,
    // print it ONCE, exit. No server, no network. The operator pastes the
    // output on the agent machine; the agent then dials /agent/ws with it.
    if args.first().map(|s| s.as_str()) == Some("generate-token") {
        generate_token(&cfg, &args[1..]); // diverges (process::exit)
    }

    // Per-machine token registry (digests only).
    let tokens_path = cfg.tokens_path.clone();
    let store = TokenStore::load(&tokens_path);
    if store.hosts.is_empty() {
        tracing::warn!("no registered hosts in {} -> every /state POST and /agent/ws handshake is 401 until you mint one: muxmesh-hub generate-token <host>", tokens_path.display());
    }

    // Embedded badge poller: tmux windows on THIS machine get the
    // @muxmesh-status badge without the operator launching anything. On a
    // headless pivot (no tmux server) it is inert by design.
    {
        let pcfg = cfg.clone();
        std::thread::spawn(move || muxmesh_core::poll::poll_hub_and_badge_windows(&pcfg));
    }

    // purge expired entries at startup so state.json doesn't grow forever.
    if let Err(e) = purge_and_write(&cfg.state_path, cfg.ttl) {
        tracing::warn!("startup purge: {e}");
    }

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(run(cfg, store)) {
        tracing::error!("server error: {e}");
        process::exit(1);
    }
}

/// Build the router and serve until the listener dies.
async fn run(cfg: HubConfig, store: TokenStore) -> std::io::Result<()> {
    let state = Arc::new(AppState {
        tokens: Mutex::new(store),
        state_path: cfg.state_path.clone(),
        log_path: log_path_for(&cfg.state_path),
        write_lock: Mutex::new(()),
        channels: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/state", get(route::get_state).post(route::post_state))
        .route("/cmd", post(route::post_cmd))
        .route("/agent/ws", get(interfaces::route::agent_ws))
        // legacy path: old agents dial /ws — same handler, same auth.
        .route("/ws", get(interfaces::route::agent_ws))
        .fallback(route::not_found)
        // 401 before any route runs when the upgrade headers are missing or
        // wrong (never a bare 404 on the WS paths, never a 101).
        .route_layer(middleware::from_fn_with_state(state.clone(), ws_auth_gate))
        // body cap 1 MiB on POSTs (agents send a few KB; beyond that = abuse)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(
        "listening on {} -> {} (tokens: {}, agents dial /agent/ws)",
        cfg.bind,
        cfg.state_path.display(),
        cfg.tokens_path.display()
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(std::io::Error::other)
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received");
}

/// Console mode: `muxmesh-hub generate-token <host>` — mint a machine token,
/// register it in tokens.json, print it ONCE. herdr-style `add machine`:
/// operator runs this on the pivot, pastes the printed token on the agent
/// (~/.config/muxmesh/token), done. Re-issue rotates the previous token.
fn generate_token(cfg: &HubConfig, args: &[String]) -> ! {
    let host = match args.first() {
        Some(h) if valid_host(h) => h.clone(),
        _ => {
            eprintln!(
                "usage: muxmesh-hub generate-token <host>   (host = what you type after ssh)"
            );
            process::exit(2);
        }
    };
    let mut store = TokenStore::load(&cfg.tokens_path);
    let token = match store.issue(&host) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("muxmesh-hub: generate-token: {e}");
            process::exit(1);
        }
    };
    // persist BEFORE printing: if the file write fails, no token was issued.
    let json = match store.to_json_pretty() {
        Ok(j) => j,
        Err(e) => {
            eprintln!("muxmesh-hub: generate-token: serialize: {e}");
            process::exit(1);
        }
    };
    if let Err(e) = write_file_atomic(&cfg.tokens_path, json.as_bytes()) {
        eprintln!(
            "muxmesh-hub: generate-token: write {}: {e}",
            cfg.tokens_path.display()
        );
        process::exit(1);
    }
    // the ONLY place raw tokens ever appear: stdout, once, for the operator.
    println!("{token}");
    eprintln!(
        "muxmesh-hub: host '{host}' paired — machine token in {}. Paste it on the agent as ~/.config/muxmesh/token (0600). The agent uses it for its /agent/ws handshake. Shown once.",
        cfg.tokens_path.display()
    );
    process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxmesh_core::{now_unix, Ack, HostState, StateDoc};

    fn tmp_state(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir()
            .join(format!("muxmesh-hub-test-{}-{}", std::process::id(), name))
            .join("state.json");
        if let Some(d) = p.parent() {
            let _ = std::fs::remove_dir_all(d);
            std::fs::create_dir_all(d).unwrap();
        }
        p
    }

    #[test]
    fn write_atomic_then_read_roundtrip() {
        let p = tmp_state("roundtrip");
        let mut doc = StateDoc::default();
        doc.hosts.insert(
            "host-a".into(),
            HostState {
                host: "host-a".into(),
                armed: true,
                seen: 1_700_000_000,
                sessions: vec![],
                parents: vec![],
            },
        );
        state::write_atomic(&p, &doc).unwrap();
        let back = state::read_doc(&p);
        assert!(back.hosts.contains_key("host-a"));
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn purge_removes_stale_keeps_fresh() {
        let p = tmp_state("purge");
        let now = now_unix();
        let mut doc = StateDoc::default();
        doc.hosts.insert(
            "fresh".into(),
            HostState {
                host: "fresh".into(),
                armed: true,
                seen: now,
                sessions: vec![],
                parents: vec![],
            },
        );
        doc.hosts.insert(
            "stale".into(),
            HostState {
                host: "stale".into(),
                armed: true,
                seen: now - 9_999,
                sessions: vec![],
                parents: vec![],
            },
        );
        state::write_atomic(&p, &doc).unwrap();
        purge_and_write(&p, 30).unwrap();
        let back = state::read_doc(&p);
        assert!(back.hosts.contains_key("fresh"));
        assert!(!back.hosts.contains_key("stale"));
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn read_doc_missing_file_is_empty_not_panic() {
        let p = std::env::temp_dir().join("muxmesh-hub-test-absent/state.json");
        let doc = state::read_doc(&p);
        assert!(doc.hosts.is_empty());
    }

    #[test]
    fn record_ack_mirrors_last_cmd_into_state_json() {
        let p = tmp_state("last-cmd");
        let mut doc = StateDoc::default();
        doc.hosts.insert(
            "host-a".into(),
            HostState {
                host: "host-a".into(),
                armed: true,
                seen: now_unix(),
                sessions: vec![],
                parents: vec![],
            },
        );
        state::write_atomic(&p, &doc).unwrap();
        let state = AppState {
            tokens: Mutex::new(TokenStore::default()),
                state_path: p.clone(),
            log_path: log_path_for(&p),
            write_lock: Mutex::new(()),
            channels: Mutex::new(HashMap::new()),
        };
        state::record_ack(&state, "host-a", &Ack::ok(1, "c-1"));
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(raw["hosts"]["host-a"]["last_cmd"]["id"], "c-1");
        assert_eq!(raw["hosts"]["host-a"]["last_cmd"]["ok"], true);
        // unknown host -> no panic, file untouched shape
        state::record_ack(&state, "ghost", &Ack::err(1, "c-2", "nope"));
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn write_atomic_uses_unique_tmp_names() {
        // two writes must not collide on the scratch file even without the lock
        let p = tmp_state("uniq-tmp");
        let mut doc = StateDoc::default();
        doc.hosts.insert(
            "h".into(),
            HostState {
                host: "h".into(),
                armed: true,
                seen: 1,
                sessions: vec![],
                parents: vec![],
            },
        );
        state::write_atomic(&p, &doc).unwrap();
        state::write_atomic(&p, &doc).unwrap();
        assert!(state::read_doc(&p).hosts.contains_key("h"));
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn merge_state_preserves_out_of_band_last_cmd() {
        let p = tmp_state("merge-lastcmd");
        let state = AppState {
            tokens: Mutex::new(TokenStore::default()),
                state_path: p.clone(),
            log_path: log_path_for(&p),
            write_lock: Mutex::new(()),
            channels: Mutex::new(HashMap::new()),
        };
        state::record_ack(&state, "host-a", &Ack::ok(1, "c-9"));
        let st = HostState {
            host: "host-a".into(),
            armed: true,
            seen: now_unix(),
            sessions: vec![],
            parents: vec![],
        };
        let _g = state.write_lock.lock();
        state::merge_state(&p, "host-a", &st).unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(raw["hosts"]["host-a"]["last_cmd"]["id"], "c-9");
        assert_eq!(raw["hosts"]["host-a"]["armed"], true);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }
}
