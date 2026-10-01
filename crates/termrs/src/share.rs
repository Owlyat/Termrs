//! Per-pane terminal sharing over iroh (peer-to-peer QUIC).
//!
//! `Share terminal` starts a fresh iroh [`Endpoint`] + [`Router`] for the
//! focused pane and shows a ticket/link. A viewer (the termrs share page, with
//! iroh compiled to WebAssembly) dials the endpoint, receives a snapshot of the
//! pane plus a live stream of its output, and — if it is the first controller —
//! can type back. Closing sharing drops the endpoint and invalidates the
//! ticket. There is deliberately no way to run arbitrary host commands; a
//! viewer only ever drives the shared pane.
//!
//! Like the MCP server, the iroh accept loop runs on the window's tokio
//! runtime, but every touch of pane state is marshalled to the UI thread over a
//! crossbeam [`ShareQuery`] with a oneshot reply, answered in
//! [`crate::app::App::poll_share`].

use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::endpoint::{Connection, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointId};
use iroh_tickets::endpoint::EndpointTicket;
use termrs_share_proto::{ALPN, Frame, LEN_PREFIX, Mode, body_len};

/// Capacity of the pane-output broadcast ring. A viewer that falls behind by
/// more than this gets a fresh snapshot instead of every missed byte.
const OUT_CAPACITY: usize = 2048;

/// A request from a share connection to the UI thread.
pub struct ShareQuery {
    pub pane_id: usize,
    pub kind: ShareQueryKind,
    /// UI thread fills this in; the connection awaits it.
    pub reply: tokio::sync::oneshot::Sender<ShareReply>,
}

/// What a share connection wants the UI thread to do.
pub enum ShareQueryKind {
    /// Full-screen redraw at the given viewer size (`0, 0` = pane's own size).
    Snapshot { cols: u16, rows: u16 },
    /// Keystrokes/paste from the controller, to write into the pane.
    Input(Vec<u8>),
}

/// The UI thread's answer.
pub enum ShareReply {
    Bytes(Vec<u8>),
    Ok,
    Err(String),
}

/// State shared between the UI thread and the running iroh accept loop.
#[derive(Debug)]
pub struct ShareShared {
    pub pane_id: usize,
    /// Session code a viewer must present; empty disables the check.
    pub code: String,
    /// When false, every viewer is read-only.
    pub allow_control: bool,
    /// Raw PTY output fanned out to connected viewers.
    pub out_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    query_tx: crossbeam_channel::Sender<ShareQuery>,
    /// The single viewer allowed to type, once one has claimed it.
    controller: Mutex<Option<EndpointId>>,
}

impl ShareShared {
    async fn query(&self, kind: ShareQueryKind) -> Result<ShareReply, String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.query_tx
            .send(ShareQuery {
                pane_id: self.pane_id,
                kind,
                reply: tx,
            })
            .map_err(|e| format!("pane is gone: {e}"))?;
        rx.await.map_err(|e| format!("pane did not answer: {e}"))
    }

    /// Ask the UI thread for a full-screen redraw.
    pub async fn snapshot(&self, cols: u16, rows: u16) -> Result<Vec<u8>, String> {
        match self.query(ShareQueryKind::Snapshot { cols, rows }).await? {
            ShareReply::Bytes(b) => Ok(b),
            ShareReply::Err(e) => Err(e),
            _ => Err("pane returned no snapshot".into()),
        }
    }

    /// Send input bytes to the UI thread for writing into the pane.
    pub async fn input(&self, bytes: Vec<u8>) -> Result<(), String> {
        match self.query(ShareQueryKind::Input(bytes)).await? {
            ShareReply::Err(e) => Err(e),
            _ => Ok(()),
        }
    }

    /// Claim the controller slot for `id`. The first claimant wins; it keeps
    /// control on reconnect, and other viewers stay read-only.
    fn claim_controller(&self, id: &EndpointId) -> bool {
        if !self.allow_control {
            return false;
        }
        let mut guard = self.controller.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            None => {
                *guard = Some(*id);
                true
            }
            Some(current) => current == id,
        }
    }

    fn release_controller(&self, id: &EndpointId) {
        let mut guard = self.controller.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_ref() == Some(id) {
            *guard = None;
        }
    }
}

/// A running share session owned by one pane.
pub struct ShareSession {
    pub code: String,
    /// iroh ticket locating the endpoint (the "code" a viewer pastes).
    pub ticket: String,
    /// Ready-to-share link (page URL with the ticket/code in the fragment).
    pub link: String,
    pub shared: Arc<ShareShared>,
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ShareSession {
    /// Ask the endpoint to shut down. Idempotent.
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// A short, unambiguous session code (no `0/O/1/I`).
pub fn generate_code(len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let len = len.clamp(4, 16);
    (0..len)
        .map(|_| ALPHABET[rand::random_range(0..ALPHABET.len())] as char)
        .collect()
}

/// Build the link a viewer opens: the page URL with `#ticket=…&code=…`.
/// Without a page URL the ticket itself is returned (paste it into the page).
fn build_link(page_url: &str, ticket: &str, code: &str) -> String {
    let base = page_url.trim();
    if base.is_empty() {
        ticket.to_string()
    } else {
        format!("{base}#ticket={ticket}&code={code}")
    }
}

/// Start sharing `pane_id`. Binds a fresh iroh endpoint on `rt` and returns as
/// soon as it is ready, along with the ticket/link to show.
pub fn start(
    rt: &tokio::runtime::Handle,
    query_tx: crossbeam_channel::Sender<ShareQuery>,
    pane_id: usize,
    code: String,
    page_url: &str,
    allow_control: bool,
) -> Result<ShareSession, String> {
    let (out_tx, _keep) = tokio::sync::broadcast::channel::<Vec<u8>>(OUT_CAPACITY);
    let shared = Arc::new(ShareShared {
        pane_id,
        code: code.clone(),
        allow_control,
        out_tx,
        query_tx,
        controller: Mutex::new(None),
    });

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<String, String>>();
    let handler = ShareHandler {
        shared: Arc::clone(&shared),
    };

    rt.spawn(async move {
        let endpoint = match Endpoint::builder(presets::N0).bind().await {
            Ok(e) => e,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("iroh bind failed: {e}")));
                return;
            }
        };
        let ticket = EndpointTicket::new(endpoint.addr()).to_string();
        let router = Router::builder(endpoint)
            .accept(ALPN, handler)
            .spawn();
        let _ = ready_tx.send(Ok(ticket));
        let _ = stop_rx.await;
        let _ = router.shutdown().await;
        log::info!("share pane {pane_id}: endpoint closed");
    });

    match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(ticket)) => {
            let link = build_link(page_url, &ticket, &code);
            Ok(ShareSession {
                code,
                ticket,
                link,
                shared,
                stop_tx: Some(stop_tx),
            })
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err("iroh endpoint did not start in time".into()),
    }
}

/// iroh protocol handler: one per share session.
#[derive(Debug)]
struct ShareHandler {
    shared: Arc<ShareShared>,
}

impl ProtocolHandler for ShareHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if let Err(e) = self.handle(connection).await {
            log::warn!("share: connection ended: {e}");
        }
        Ok(())
    }
}

impl ShareHandler {
    async fn handle(&self, conn: Connection) -> Result<(), String> {
        let remote = conn.remote_id();
        let (mut send, mut recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;

        // First frame must be the handshake.
        let hello = match read_frame(&mut recv).await? {
            Frame::Hello(h) => h,
            _ => {
                let _ = write_frame(&mut send, &Frame::Error("expected hello".into())).await;
                return Err("first frame was not hello".into());
            }
        };
        if !self.shared.code.is_empty() && hello.code != self.shared.code {
            let _ = write_frame(&mut send, &Frame::Error("invalid code".into())).await;
            conn.close(1u8.into(), b"invalid code");
            return Ok(());
        }
        let control = hello.mode == Mode::Control && self.shared.claim_controller(&remote);
        log::info!(
            "share pane {}: viewer {remote} connected ({})",
            self.shared.pane_id,
            if control { "control" } else { "read-only" }
        );

        // Seed the viewer with the current screen.
        let snapshot = self.shared.snapshot(hello.cols, hello.rows).await?;
        write_frame(&mut send, &Frame::Snapshot(snapshot)).await?;

        // Fan pane output out to this viewer until the stream dies.
        let shared = Arc::clone(&self.shared);
        let out_task = tokio::spawn(async move {
            let mut rx = shared.out_tx.subscribe();
            loop {
                match rx.recv().await {
                    Ok(bytes) => {
                        if write_frame(&mut send, &Frame::Output(bytes)).await.is_err() {
                            break;
                        }
                    }
                    // Fell behind: resend a full snapshot to resync.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        match shared.snapshot(0, 0).await {
                            Ok(snap) => {
                                if write_frame(&mut send, &Frame::Snapshot(snap))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Read viewer input (ignored unless this viewer controls).
        while let Ok(frame) = read_frame(&mut recv).await {
            match frame {
                Frame::Input(bytes) if control => {
                    let _ = self.shared.input(bytes).await;
                }
                // Resize is accepted for forward-compatibility but does not
                // resize the host pane (the local view stays authoritative).
                _ => {}
            }
        }

        out_task.abort();
        if control {
            self.shared.release_controller(&remote);
        }
        log::info!("share pane {}: viewer {remote} disconnected", self.shared.pane_id);
        Ok(())
    }
}

/// Read one length-prefixed frame.
async fn read_frame<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> Result<Frame, String> {
    use tokio::io::AsyncReadExt;
    let mut prefix = [0u8; LEN_PREFIX];
    reader
        .read_exact(&mut prefix)
        .await
        .map_err(|e| e.to_string())?;
    let len = body_len(prefix).map_err(|e| e.to_string())?;
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| e.to_string())?;
    let tag = body[0];
    Frame::decode(tag, &body[1..]).map_err(|e| e.to_string())
}

/// Write one length-prefixed frame.
async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    writer
        .write_all(&frame.encode())
        .await
        .map_err(|e| e.to_string())?;
    writer.flush().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_random_and_unambiguous() {
        let a = generate_code(6);
        let b = generate_code(6);
        assert_eq!(a.len(), 6);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(!a.contains(['0', 'O', '1', 'I']), "code: {a}");
        // Astronomically unlikely to collide; guards a broken generator.
        assert_ne!(a, b);
        assert_eq!(generate_code(1).len(), 4, "clamped to minimum 4");
    }

    fn shared_with(allow_control: bool) -> ShareShared {
        let (query_tx, _rx) = crossbeam_channel::unbounded();
        let (out_tx, _keep) = tokio::sync::broadcast::channel(4);
        ShareShared {
            pane_id: 1,
            code: "CODE".into(),
            allow_control,
            out_tx,
            query_tx,
            controller: Mutex::new(None),
        }
    }

    #[test]
    fn controller_is_exclusive_and_releasable() {
        let shared = shared_with(true);
        let a = iroh::SecretKey::generate().public();
        let b = iroh::SecretKey::generate().public();
        assert!(shared.claim_controller(&a), "first viewer controls");
        assert!(shared.claim_controller(&a), "same viewer keeps control");
        assert!(!shared.claim_controller(&b), "second viewer is read-only");
        shared.release_controller(&b);
        assert!(shared.claim_controller(&a), "wrong release is ignored");
        shared.release_controller(&a);
        assert!(shared.claim_controller(&b), "control transfers after release");
    }

    #[test]
    fn control_can_be_disabled() {
        let shared = shared_with(false);
        let a = iroh::SecretKey::generate().public();
        assert!(!shared.claim_controller(&a), "control disabled for everyone");
    }

    #[test]
    fn links_carry_ticket_and_code() {
        assert_eq!(
            build_link("https://x/#", "TICKET", "AB12"),
            "https://x/##ticket=TICKET&code=AB12"
        );
        assert_eq!(build_link("", "TICKET", "AB12"), "TICKET");
    }
}
