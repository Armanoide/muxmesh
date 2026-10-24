//! The agent's local IPC listener: the socket the CLI dials, and the shared
//! hub channel it relays on.
//!
//! Architecture rule (WS-Refacto §9): the CLI never touches the network. It
//! writes one JSON line to `~/.muxmesh/agent.sock`; this module answers —
//! locally for `send` (enqueue on the live WS), or after asking the hub over
//! the WS for `ask_directory`. No agent running -> no socket -> the CLI
//! fails. That is intended: this process is the machine's only gateway.

use std::sync::Arc;

use muxmesh_core::{
    ipc_socket_path, AskDirectory, CommandEnvelope, DirectoryAnswer, IpcRequest, IpcResponse,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// The hub's answers on the agent's long-lived WS need somewhere to wait:
/// the session task posts each DirectoryAnswer into this mailbox, the CLI's
/// handler asks a question and waits for its letter. Capacity 8; a full
/// mailbox drops the answer (the CLI degrades, never hangs). Clonable so the
/// session and the IPC listener hold handles to ONE mailbox.
#[derive(Clone)]
pub struct HubMailbox {
    inbox: Arc<tokio::sync::mpsc::Sender<DirectoryAnswer>>,
    unread: Arc<Mutex<tokio::sync::mpsc::Receiver<DirectoryAnswer>>>,
}

impl Default for HubMailbox {
    fn default() -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        HubMailbox { inbox: Arc::new(tx), unread: Arc::new(Mutex::new(rx)) }
    }
}

impl HubMailbox {
    /// Post a hub answer into the mailbox. Full (8 unread) drops it — the
    /// waiter times out, the CLI degrades. Never blocks the WS reader.
    pub async fn deposit(&self, ans: DirectoryAnswer) {
        let _ = self.inbox.try_send(ans);
    }

    /// Ask the hub over the WS and wait for its letter. `ask` sends the
    /// request frame (the caller owns the WS). Mailbox semantics: an answer
    /// that arrived BEFORE we start waiting is kept, not lost (a Notify woke
    /// nobody in that window — the race the e2e caught). The mailbox is read
    /// newest-first: the freshest answer for `me` wins, older unread ones are
    /// discarded. Err = hub silent past 5s or channel down.
    pub async fn ask_hub(
        &self,
        me: &str,
        ask: impl std::future::Future<Output = Result<(), String>>,
    ) -> Result<DirectoryAnswer, String> {
        let mut unread = self.unread.lock().await;
        ask.await?;
        // answers already in the box (hub faster than our scheduling): take
        // the newest one addressed to us, drop the rest.
        let mut fresh: Option<DirectoryAnswer> = None;
        while let Ok(a) = unread.try_recv() {
            if a.me == me {
                fresh = Some(a);
            }
        }
        if let Some(a) = fresh {
            return Ok(a);
        }
        // nothing yet: wait for the next letter addressed to me.
        loop {
            let got = tokio::select! {
                g = unread.recv() => g,
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                    return Err("hub silent (5s)".into());
                }
            };
            match got {
                Some(a) if a.me == me => return Ok(a),
                Some(_) => continue, // addressed elsewhere: skip, keep waiting
                None => return Err("no directory answer".into()),
            }
        }
    }
}

/// Serve the CLI socket forever: bind `~/.muxmesh/agent.sock` (0600), accept
/// one request per connection, answer, close. A bind failure (stale socket
/// file, bad path) is logged and retried — the agent must not die because a
/// CLI connection was fumbled.
pub async fn serve_cli_forever(chan: Arc<crate::ws::Channel>) -> std::io::Result<()> {
    let path = ipc_socket_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A previous crash leaves the socket file behind -> bind would EADDRINUSE.
    // But a live second agent answers on it: NEVER delete its socket (two
    // listeners would run tmux verbs on one machine). Probe first: connect
    // succeeds = someone is home -> refuse to start; refused/stale = ours now.
    if path.exists() {
        match tokio::net::UnixStream::connect(&path).await {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!("another agent is live on {} — refusing to start", path.display()),
                ))
            }
            Err(_) => {
                tracing::warn!("ipc: stale socket {} -> removing", path.display());
                std::fs::remove_file(&path)?;
            }
        }
    }
    let listener = tokio::net::UnixListener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 0600 immediately after bind: the window is one syscall, and every
        // peer here (the CLI) runs as the same user anyway.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::info!("ipc: listening on {}", path.display());

    
    let mut connections = tokio::task::JoinSet::new();
    loop {
        let (stream, _) = listener.accept().await?;
        let chan = chan.clone();
        connections.spawn(async move {
            if let Err(e) = serve_one_connection(stream, &chan).await {
                tracing::warn!("ipc: connection: {e}");
            }
        });
    }
}

/// One CLI request per connection: read the first JSON line, dispatch, write
/// one response line. Extra input is ignored (the CLI half-closes anyway).
async fn serve_one_connection(
    stream: tokio::net::UnixStream,
    chan: &crate::ws::Channel,
) -> std::io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    let line = match lines.next_line().await? {
        Some(l) => l,
        None => return Ok(()), // client closed without asking
    };
    let req: IpcRequest = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            write_line(&mut w, &IpcResponse::err(format!("bad request: {e}"))).await?;
            return Ok(());
        }
    };
    let resp = dispatch(req, chan).await;
    write_line(&mut w, &resp).await
}

/// Route one request to its answer. `send` is answered as soon as the frame
/// is on the WS (the hub acks later, in its own time); `ask_directory` waits
/// waiting for the hub's reply in the mailbox.
async fn dispatch(req: IpcRequest, chan: &crate::ws::Channel) -> IpcResponse {
    match req {
        IpcRequest::Send { host, verb } => {
            // unique id per send: same-second commands must not collide in the
            // hub's ack mirror (id-keyed).
            static SEND_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let env = CommandEnvelope {
                v: 1,
                id: format!(
                    "c-{}-{}",
                    muxmesh_core::now_unix(),
                    SEND_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ),
                src: chan.host.clone(),
                dst: host.clone(),
                cmd: verb,
                ts: muxmesh_core::now_unix(),
            };
            match serde_json::to_string(&env) {
                Ok(frame) => match chan.push_frame(frame).await {
                    Ok(()) => IpcResponse::ok(format!("queued for {host} (relayed on agent WS)")),
                    Err(e) => IpcResponse::err(format!("ws down: {e} — command not sent")),
                },
                Err(e) => IpcResponse::err(format!("serialize: {e}")),
            }
        }
        IpcRequest::AskDirectory => {
            let me = chan.host.clone();
            let ask = chan.push_frame(serde_json::to_string(&AskDirectory {}).unwrap());
            match chan.mailbox.ask_hub(&me, ask).await {
                Ok(ans) => {
                    let mut data = serde_json::Map::new();
                    data.insert(
                        "directory".into(),
                        serde_json::to_value(&ans.directory).unwrap_or_default(),
                    );
                    IpcResponse { ok: true, msg: "directory".into(), data }
                }
                Err(e) => IpcResponse::err(format!("hub: {e}")),
            }
        }
    }
}

async fn write_line(w: &mut tokio::net::unix::OwnedWriteHalf, resp: &IpcResponse) -> std::io::Result<()> {
    let line = serde_json::to_string(resp).unwrap_or_else(|_| "{\"ok\":false,\"msg\":\"serialize\"}".into());
    w.write_all(format!("{line}\n").as_bytes()).await?;
    w.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mailbox_delivers_the_hubs_answer() {
        let mailbox = Arc::new(HubMailbox::default());
        let rv2 = mailbox.clone();
        // the hub answers 50ms after the question, as the WS reader would.
        let answer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            rv2.deposit(DirectoryAnswer {
                me: "me".into(),
                directory: muxmesh_core::StateDoc::default(),
            })
            .await;
        });
        let ans = mailbox.ask_hub("me", std::future::ready(Ok(()))).await.unwrap();
        assert_eq!(ans.me, "me");
        answer.await.unwrap();
    }

    #[tokio::test]
    async fn mailbox_refuses_an_answer_addressed_elsewhere() {
        // only OUR letters count: an answer addressed to another host never
        // satisfies our question — we keep waiting and hit the 5s guard. The
        // depositing task must DIE with the test: a loop that outlives it
        // keeps the runtime alive and cargo never finishes (seen: hang).
        let mailbox = Arc::new(HubMailbox::default());
        let rv2 = mailbox.clone();
        let spam = tokio::spawn(async move {
            for _ in 0..400 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                rv2.deposit(DirectoryAnswer {
                    me: "someone-else".into(),
                    directory: muxmesh_core::StateDoc::default(),
                })
                .await;
            }
        });
        let e = mailbox.ask_hub("me", std::future::ready(Ok(()))).await.unwrap_err();
        assert!(e.contains("silent"), "{e}");
        spam.abort(); // the loop has no reason to outlive the question it fed
    }

    #[tokio::test]
    async fn mailbox_keeps_answer_that_arrived_before_waiting() {
        // THE race the e2e caught: the hub answers before ask_hub reaches its
        // select! (WS faster than the scheduler). mpsc holds the answer —
        // a Notify would have lost it and burned the 5s timeout.
        let mailbox = HubMailbox::default();
        mailbox.deposit(DirectoryAnswer { me: "me".into(), directory: muxmesh_core::StateDoc::default() }).await;
        let ans = mailbox.ask_hub("me", std::future::ready(Ok(()))).await.unwrap();
        assert_eq!(ans.me, "me");
    }

    #[tokio::test]
    async fn mailbox_drains_letters_from_a_past_question() {
        // an answer to a PAST question must not satisfy the next one: it is
        // drained, and the fresh answer (addressed correctly) wins.
        let mailbox = HubMailbox::default();
        mailbox.deposit(DirectoryAnswer { me: "old".into(), directory: muxmesh_core::StateDoc::default() }).await;
        let rv2 = mailbox.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            rv2.deposit(DirectoryAnswer { me: "me".into(), directory: muxmesh_core::StateDoc::default() }).await;
        });
        let ans = mailbox.ask_hub("me", std::future::ready(Ok(()))).await.unwrap();
        assert_eq!(ans.me, "me");
    }

    #[tokio::test]
    async fn ask_hub_fails_when_ask_itself_fails() {
        let mailbox = HubMailbox::default();
        let e = mailbox.ask_hub("me", std::future::ready(Err("ws down".into()))).await.unwrap_err();
        assert_eq!(e, "ws down");
    }

    #[tokio::test]
    async fn dispatch_send_without_channel_reports_ws_down() {
        // a channel with no live socket: push_frame fails, the CLI gets an
        // actionable "ws down" — never a hang, never a panic.
        let chan = crate::ws::Channel::new("me".into());
        let resp = dispatch(
            IpcRequest::Send { host: "child".into(), verb: muxmesh_core::Command::BarHide },
            &chan,
        )
        .await;
        assert!(!resp.ok);
        assert!(resp.msg.contains("ws down"), "{}", resp.msg);
    }

    #[tokio::test]
    async fn dispatch_send_queued_when_socket_attached() {
        // attach a live sender: the relay frame must land on it, in order.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let chan = crate::ws::Channel::new("me".into());
        chan.attach_socket(Some(tx)).await;
        let resp = dispatch(
            IpcRequest::Send { host: "child".into(), verb: muxmesh_core::Command::GiveControl },
            &chan,
        )
        .await;
        assert!(resp.ok, "{}", resp.msg);
        let frame = rx.recv().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["dst"], "child");
        assert_eq!(v["cmd"]["type"], "GiveControl");
    }

    #[tokio::test]
    async fn dispatch_ask_directory_serves_hub_answer() {
        // the hub posts its reply into the mailbox; dispatch must hand the
        // directory to the CLI in data.directory. The ask frame itself needs
        // a live socket (attached here) — the hub reads it and answers back.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let chan = Arc::new(crate::ws::Channel::new("me".into()));
        chan.attach_socket(Some(tx)).await;
        let c2 = chan.clone();
        tokio::spawn(async move {
            // the hub reads the ask frame, then posts its reply ~20ms later.
            let _ask = rx.recv().await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            c2.mailbox
                .deposit(DirectoryAnswer { me: "me".into(), directory: muxmesh_core::StateDoc::default() })
                .await;
        });
        let resp = dispatch(IpcRequest::AskDirectory, &chan).await;
        assert!(resp.ok, "{}", resp.msg);
        assert!(resp.data.contains_key("directory"));
    }

    #[tokio::test]
    async fn serve_one_connection_answers_bad_request() {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let chan = Arc::new(crate::ws::Channel::new("me".into()));
        let c2 = chan.clone();
        let h = tokio::spawn(async move { serve_one_connection(server, &c2).await });
        let mut c = client;
        c.write_all(b"not json\n").await.unwrap();
        let mut buf = String::new();
        BufReader::new(c).read_line(&mut buf).await.unwrap();
        let resp: IpcResponse = serde_json::from_str(&buf).unwrap();
        assert!(!resp.ok);
        assert!(resp.msg.contains("bad request"));
        h.await.unwrap().unwrap();
    }
}
