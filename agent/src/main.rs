mod config;
mod error;

use std::time::Duration;
use futures_util::{SinkExt, StreamExt};
use muxmesh_core::{
    decode, encode, now_secs, ChallengeOk, ChallengeWire, Hello, Message, ReportState, BarStatus,
};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::error::Result;

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type Sink = futures_util::stream::SplitSink<Socket, WsMessage>;
type Stream = futures_util::stream::SplitStream<Socket>;

/// Dial the hub over WebSocket, carrying identity headers.
async fn connect(cfg: &Config) -> Result<Socket> {
    let fp = cfg.fingerprint();
    let mut request = cfg.hub_url.as_str().into_client_request()?;
    let h = request.headers_mut();
    h.insert("X-Muxmesh-Token", cfg.token.parse().unwrap());
    h.insert("X-Muxmesh-Label", cfg.label.parse().unwrap());
    h.insert("X-Muxmesh-Fp", fp.as_str().parse().unwrap());

    let (ws, _resp) = connect_async(request).await?;
    info!("connected to hub");
    Ok(ws)
}

/// Announce ourselves and confirm the hub's challenge on the open socket.
async fn prove_identity(sink: &mut Sink, stream: &mut Stream, cfg: &Config) -> Result<()> {
    let hello = encode(&Message::Hello(Hello { label: cfg.label.clone() }))?;
    sink.send(WsMessage::Text(hello.into())).await?;

    let fp = cfg.fingerprint();
    match stream.next().await {
        Some(Ok(WsMessage::Text(line))) => {
            if let Message::Challenge(ChallengeWire { nonce, sig }) = decode(&line)? {
                muxmesh_core::Challenge::verify_hex(&nonce, &sig, &cfg.hub_pubkey, fp.as_str())?;
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
    Ok(())
}

/// Beat the heartbeat until the hub closes the socket.
async fn beat_forever(sink: &mut Sink) -> Result<()> {
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

/// Run one live session: prove identity, then beat forever.
async fn run(ws: Socket, cfg: &Config) -> Result<()> {
    let (mut sink, mut stream) = ws.split();
    prove_identity(&mut sink, &mut stream, cfg).await?;
    beat_forever(&mut sink).await
}

#[tokio::main]
async fn main() {
    muxmesh_core::logger::init_tracing();
    let cfg = Config::from_env().expect("invalid agent configuration");

    loop {
        match connect(&cfg).await {
            Ok(ws) => {
                if let Err(e) = run(ws, &cfg).await {
                    error!("hub session ended: {e} — reconnecting in 5s");
                }
            }
            Err(e) => error!("dial failed: {e} — retrying in 5s"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
