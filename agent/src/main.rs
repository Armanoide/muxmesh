mod config;
mod error;

use std::time::Duration;
use futures_util::{SinkExt, StreamExt};
use muxmesh_core::{
    decode, encode, hex_bytes, ChallengeOk, ChallengeWire, Hello, Message, ReportState, BarStatus,
};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::error::Result;

/// Dial the hub with identity headers, run the handshake, then heartbeat.
async fn connect_and_run(cfg: &Config) -> Result<()> {
    let fp = cfg.fingerprint();

    // Outbound dial carrying the identity headers (token, label, fingerprint).
    let mut request = cfg.hub_url.as_str().into_client_request()?;
    let h = request.headers_mut();
    h.insert("X-Muxmesh-Token", cfg.token.parse().unwrap());
    h.insert("X-Muxmesh-Label", cfg.label.parse().unwrap());
    h.insert("X-Muxmesh-Fp", fp.as_str().parse().unwrap());

    let (ws, _resp) = connect_async(request).await?;
    info!("connected to hub");
    let (mut sink, mut stream) = ws.split();

    // Announce ourselves.
    let hello = encode(&Message::Hello(Hello { label: cfg.label.clone() }))?;
    sink.send(WsMessage::Text(hello.into())).await?;

    // Read the hub's CHALLENGE and verify it signed by the known hub key.
    let pub_key = hub_pubkey(cfg)?;
    match stream.next().await {
        Some(Ok(WsMessage::Text(line))) => {
            if let Message::Challenge(ChallengeWire { nonce, sig }) = decode(&line)? {
                muxmesh_core::Challenge::verify_hex(&nonce, &sig, &pub_key, fp.as_str())?;
                info!("hub identity verified");
                // Prove we verified: echo the nonce.
                let ok = encode(&Message::ChallengeOk(ChallengeOk { nonce }))?;
                sink.send(WsMessage::Text(ok.into())).await?;
            } else {
                warn!("expected CHALLENGE, got something else");
            }
        }
        other => {
            warn!(?other, "no CHALLENGE received");
        }
    }

    // Heartbeat loop.
    let mut beat = tokio::time::interval(Duration::from_secs(15));
    loop {
        beat.tick().await;
        let st = encode(&Message::ReportState(ReportState {
            seen: now_secs(),
            status: BarStatus::On,
        }))?;
        if sink.send(WsMessage::Text(st.into())).await.is_err() {
            break;
        }
    }
    Ok(())
}

fn hub_pubkey(cfg: &Config) -> Result<ed25519_dalek::VerifyingKey> {
    let bytes = hex_bytes(&cfg.hub_pubkey_hex)?;
    let arr: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
        muxmesh_core::ErrorCore::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("MUXMESH_HUB_PUBKEY must be 32 bytes (64 hex), got {}", v.len()),
        ))
    })?;
    Ok(ed25519_dalek::VerifyingKey::from_bytes(&arr).map_err(muxmesh_core::ErrorCore::from)?)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::main]
async fn main() {
    muxmesh_core::logger::init_tracing();
    let cfg = Config::from_env();

    loop {
        if let Err(e) = connect_and_run(&cfg).await {
            error!("hub session ended: {e} — reconnecting in 5s");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
