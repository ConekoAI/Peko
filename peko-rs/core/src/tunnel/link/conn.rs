//! One link's connection (ADR-125 D3): the listener, the dialer, the
//! hello exchange, and the loops that carry frames once both hellos are
//! accepted.
//!
//! Nothing crosses before both hellos are accepted; a side that has not
//! said a valid hello within [`HELLO_WAIT`] is dropped, and at most
//! [`MAX_PENDING_HELLOS`] connections wait to say one. After that a frame
//! is taken only when its type is sealed, its `sourceRuntimeId` is the
//! computer that said hello, and its `recipientRuntimeId` is this one;
//! anything else closes the link. Frames are counted per minute against
//! the caps PekoHub applies per runtime (D8).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::hello::{self, hello_frame, hello_of, sign_hello, verify_hello};
use super::router::Router;
use super::tls;
use crate::tunnel::protocol::TunnelMessage;
use crate::tunnel::seal::is_sealed_type;

/// The path a link is served at.
pub const LINK_PATH: &str = "/v1/link";
/// How long a side has to say its hello (from the TCP connect).
pub const HELLO_WAIT: Duration = Duration::from_secs(10);
/// How many connections may wait to say hello at once.
pub const MAX_PENDING_HELLOS: usize = 32;
/// How often a link is asked whether it is there.
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);
/// Heartbeats a link may miss before it is closed.
pub const MAX_MISSED: u32 = 3;
/// How many frames wait to be written on one link.
const LINK_QUEUE: usize = 1024;
/// How long a dial may take to connect.
const CONNECT_WAIT: Duration = Duration::from_secs(5);

/// One link to another computer.
pub struct Link {
    pub(crate) id: u64,
    /// The computer on the other end (it said hello).
    pub(crate) runtime: String,
    /// The address of the other end.
    pub(crate) address: String,
    /// Whether this computer dialed it.
    pub(crate) dialed_by_us: bool,
    pub(crate) up_at: Instant,
    /// RFC 3339 of `up_at`.
    pub(crate) since: String,
    /// It answered heartbeats for `WARM_FOR` (D6).
    pub(crate) warm: AtomicBool,
    /// A network change was heard and it has not answered since.
    in_doubt: AtomicBool,
    last_answer: Mutex<Instant>,
    heartbeat_seq: AtomicU64,
    missed: AtomicU64,
    tx: mpsc::Sender<Message>,
    close: CancellationToken,
    closed: AtomicBool,
    reason: Mutex<Option<String>>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("runtime", &self.runtime)
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl Link {
    /// Whether it carries frames now: up, answered heartbeats for a while
    /// and not in doubt; in `direct`, as soon as it is up.
    #[must_use]
    pub(crate) fn carries(&self, direct: bool) -> bool {
        if self.is_closed() {
            return false;
        }
        direct || (self.warm.load(Ordering::SeqCst) && !self.in_doubt.load(Ordering::SeqCst))
    }

    /// Queue `bytes` on it; gives them back when it cannot take them.
    pub(crate) fn send(&self, bytes: Vec<u8>) -> Result<(), Vec<u8>> {
        if self.is_closed() {
            return Err(bytes);
        }
        self.tx
            .try_send(Message::Binary(bytes.into()))
            .map_err(|e| match e.into_inner() {
                Message::Binary(b) => b.to_vec(),
                _ => Vec::new(),
            })
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub(crate) fn close(&self, reason: &str) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            *self.reason.lock().unwrap() = Some(reason.to_string());
        }
        self.close.cancel();
    }

    /// Whether it answered (a heartbeat or any frame) after `at`.
    pub(crate) fn answered_since(&self, at: Instant) -> bool {
        *self.last_answer.lock().unwrap() > at
    }

    /// A network change: send a heartbeat now; it is in doubt until it
    /// answers.
    pub(crate) fn doubt(&self) {
        self.in_doubt.store(true, Ordering::SeqCst);
        self.heartbeat();
    }

    fn heartbeat(&self) {
        let seq = self.heartbeat_seq.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(bytes) = (TunnelMessage::Heartbeat { seq }).to_bytes() {
            let _ = self.tx.try_send(Message::Binary(bytes.into()));
        }
    }

    /// It answered: returns whether it was in doubt.
    fn answered(&self) -> bool {
        *self.last_answer.lock().unwrap() = Instant::now();
        self.missed.store(0, Ordering::SeqCst);
        self.in_doubt.swap(false, Ordering::SeqCst)
    }
}

/// Accept links on `tcp` until `cancel`.
pub(crate) async fn accept_loop(
    router: Router,
    tcp: TcpListener,
    config: Arc<rustls::ServerConfig>,
    cancel: CancellationToken,
) {
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    loop {
        let accepted = tokio::select! {
            a = tcp.accept() => a,
            () = cancel.cancelled() => return,
        };
        let (stream, peer) = match accepted {
            Ok(a) => a,
            Err(e) => {
                debug!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = Arc::clone(&router.inner.pending_hellos).try_acquire_owned() else {
            debug!(%peer, "too many connections waiting to say hello; dropped");
            continue;
        };
        let router = router.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let address = peer.to_string();
            let result = timeout(HELLO_WAIT, accept_one(&router, &acceptor, stream)).await;
            drop(permit);
            match result {
                Ok(Ok((ws, runtime))) => {
                    serve(router, ws, runtime, address, false).await;
                }
                Ok(Err((runtime, reason))) => router.refused(runtime.as_deref(), &address, &reason),
                Err(_) => router.refused(None, &address, "no hello in time"),
            }
        });
    }
}

type Refusal = (Option<String>, String);

async fn accept_one(
    router: &Router,
    acceptor: &tokio_rustls::TlsAcceptor,
    stream: TcpStream,
) -> Result<
    (
        WebSocketStream<tokio_rustls::server::TlsStream<TcpStream>>,
        String,
    ),
    Refusal,
> {
    let tls = acceptor
        .accept(stream)
        .await
        .map_err(|e| (None, format!("tls: {e}")))?;
    let binding = tls::export_binding(tls.get_ref().1).map_err(|e| (None, e))?;
    #[allow(clippy::result_large_err)]
    fn check_path(req: &Request, resp: Response) -> Result<Response, ErrorResponse> {
        if req.uri().path() == LINK_PATH {
            Ok(resp)
        } else {
            let mut refused = ErrorResponse::new(Some("not found".into()));
            *refused.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::NOT_FOUND;
            Err(refused)
        }
    }
    let mut ws = tokio_tungstenite::accept_hdr_async(tls, check_path)
        .await
        .map_err(|e| (None, format!("websocket: {e}")))?;
    let runtime = exchange_hellos(router, &mut ws, &binding, None).await?;
    Ok((ws, runtime))
}

/// Dial `target`, expecting `expect` there when given. Returns the
/// runtime that said hello.
///
/// # Errors
///
/// It could not connect, or a hello was refused.
pub(crate) async fn dial(
    router: Router,
    target: SocketAddr,
    expect: Option<&str>,
) -> Result<String, String> {
    let result = timeout(HELLO_WAIT, dial_one(&router, target, expect)).await;
    let address = target.to_string();
    match result {
        Ok(Ok((ws, runtime))) => {
            let served = runtime.clone();
            tokio::spawn(serve(router, ws, served, address, true));
            Ok(runtime)
        }
        Ok(Err((runtime, reason))) => {
            if !reason.starts_with("connect:") {
                router.refused(runtime.as_deref().or(expect), &address, &reason);
            }
            Err(reason)
        }
        Err(_) => {
            router.refused(expect, &address, "no hello in time");
            Err("no hello in time".into())
        }
    }
}

async fn dial_one(
    router: &Router,
    target: SocketAddr,
    expect: Option<&str>,
) -> Result<
    (
        WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>,
        String,
    ),
    Refusal,
> {
    let stream = timeout(CONNECT_WAIT, TcpStream::connect(target))
        .await
        .map_err(|_| (None, "connect: timed out".to_string()))?
        .map_err(|e| (None, format!("connect: {e}")))?;
    let _ = stream.set_nodelay(true);
    let config = tls::client_config().map_err(|e| (None, e))?;
    let tls_stream = tokio_rustls::TlsConnector::from(config)
        .connect(tls::server_name(), stream)
        .await
        .map_err(|e| (None, format!("tls: {e}")))?;
    let binding = tls::export_binding(tls_stream.get_ref().1).map_err(|e| (None, e))?;
    let url = format!("wss://{}{LINK_PATH}", tls::LINK_SERVER_NAME);
    let (mut ws, _) = tokio_tungstenite::client_async(url, tls_stream)
        .await
        .map_err(|e| (None, format!("websocket: {e}")))?;
    let runtime = exchange_hellos(router, &mut ws, &binding, expect).await?;
    Ok((ws, runtime))
}

/// Say this computer's hello and check the other's.
async fn exchange_hellos<S>(
    router: &Router,
    ws: &mut WebSocketStream<S>,
    binding: &[u8; 32],
    expect: Option<&str>,
) -> Result<String, Refusal>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let own = sign_hello(&router.inner.signing_key, &router.inner.runtime_id, binding);
    ws.send(Message::Text(hello_frame(&own).into()))
        .await
        .map_err(|e| (None, format!("websocket: {e}")))?;
    let frame = loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => break t.as_bytes().to_vec(),
            Some(Ok(Message::Binary(b))) => break b.to_vec(),
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
            Some(Ok(Message::Close(_))) | None => {
                return Err((expect.map(str::to_owned), "closed before its hello".into()))
            }
            Some(Err(e)) => return Err((expect.map(str::to_owned), format!("websocket: {e}"))),
        }
    };
    let Some(compact) = hello_of(&frame) else {
        let _ = ws.close(None).await;
        return Err((
            expect.map(str::to_owned),
            "the first frame was not a hello".into(),
        ));
    };
    let admitted = router.admitted().await;
    let now = chrono::Utc::now().timestamp();
    let checked = verify_hello(
        &compact,
        binding,
        now,
        &router.inner.runtime_id,
        |runtime| admitted.contains(runtime),
    );
    match checked {
        Ok(runtime) => {
            if expect.is_some_and(|e| e != runtime) {
                // Another computer answers at that address now; it is
                // still one this computer links with.
                debug!(expected = ?expect, %runtime, "another computer answered");
            }
            // D3: nothing crosses before both hellos are accepted. Each
            // side says it accepted the other's; a side whose hello was
            // refused hears a close instead.
            ws.send(Message::Text(hello::ready_frame().into()))
                .await
                .map_err(|e| (Some(runtime.clone()), format!("websocket: {e}")))?;
            loop {
                match ws.next().await {
                    Some(Ok(Message::Text(t))) if hello::is_ready(t.as_bytes()) => break,
                    Some(Ok(Message::Binary(b))) if hello::is_ready(&b) => break,
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                    Some(Ok(_)) => {
                        let _ = ws.close(None).await;
                        return Err((Some(runtime), "a frame before link_ready".into()));
                    }
                    Some(Err(_)) | None => {
                        return Err((Some(runtime), "it refused this computer's hello".into()))
                    }
                }
            }
            Ok(runtime)
        }
        Err(refusal) => {
            let named = match &refusal {
                hello::HelloRefusal::NotAdmitted(r) => Some(r.clone()),
                _ => expect.map(str::to_owned),
            };
            let _ = ws.close(None).await;
            Err((named, refusal.reason().to_string()))
        }
    }
}

/// Carry frames on an accepted link until it closes.
async fn serve<S>(
    router: Router,
    ws: WebSocketStream<S>,
    runtime: String,
    address: String,
    dialed_by_us: bool,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<Message>(LINK_QUEUE);
    let now = Instant::now();
    let link = Arc::new(Link {
        id: router.next_link_id(),
        runtime: runtime.clone(),
        address,
        dialed_by_us,
        up_at: now,
        since: chrono::Utc::now().to_rfc3339(),
        warm: AtomicBool::new(false),
        in_doubt: AtomicBool::new(false),
        last_answer: Mutex::new(now),
        heartbeat_seq: AtomicU64::new(0),
        missed: AtomicU64::new(0),
        tx,
        close: CancellationToken::new(),
        closed: AtomicBool::new(false),
        reason: Mutex::new(None),
    });
    if !router.register(Arc::clone(&link)) {
        return;
    }
    let (mut write, mut read) = ws.split();
    let writer_close = link.close.clone();
    let writer = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                m = rx.recv() => m,
                () = writer_close.cancelled() => None,
            };
            let Some(msg) = msg else { break };
            if write.send(msg).await.is_err() {
                break;
            }
        }
        let _ = write.close().await;
    });
    let beating = Arc::clone(&link);
    let heart = tokio::spawn(async move {
        let mut tick = tokio::time::interval(HEARTBEAT_EVERY);
        loop {
            tick.tick().await;
            if beating.missed.fetch_add(1, Ordering::SeqCst) >= u64::from(MAX_MISSED) {
                beating.close("missed heartbeats");
                break;
            }
            beating.heartbeat();
        }
    });
    let mut caps = Caps::default();
    let own = router.inner.runtime_id.clone();
    let reason = loop {
        let next = tokio::select! {
            n = read.next() => n,
            () = link.close.cancelled() => break None,
        };
        let bytes: Vec<u8> = match next {
            Some(Ok(Message::Binary(b))) => b.to_vec(),
            Some(Ok(Message::Text(t))) => t.as_bytes().to_vec(),
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
            Some(Ok(Message::Close(_))) | None => break Some("closed by the other computer"),
            Some(Err(_)) => break Some("connection lost"),
        };
        match check_frame(&bytes, &runtime, &own) {
            Frame::Heartbeat(seq) => {
                if let Ok(ack) = (TunnelMessage::HeartbeatAck { seq }).to_bytes() {
                    let _ = link.tx.try_send(Message::Binary(ack.into()));
                }
                if link.answered() {
                    router.link_answered(&runtime);
                }
            }
            Frame::Ack => {
                if link.answered() {
                    router.link_answered(&runtime);
                }
            }
            Frame::Sealed(ty) => {
                if link.answered() {
                    router.link_answered(&runtime);
                }
                if !caps.allow(&ty) {
                    debug!(%runtime, %ty, "over the per-minute cap on this link; frame dropped");
                    continue;
                }
                router.receive_from_link(&runtime, &bytes).await;
            }
            Frame::Refused(why) => {
                router.refused(Some(&runtime), &link.address, why);
                break Some(why);
            }
        }
    };
    let reason = reason
        .map(str::to_owned)
        .or_else(|| link.reason.lock().unwrap().clone())
        .unwrap_or_else(|| "closed".into());
    link.close(&reason);
    heart.abort();
    let _ = writer.await;
    router.unregister(&link, &reason);
}

enum Frame {
    Heartbeat(u64),
    Ack,
    Sealed(String),
    Refused(&'static str),
}

/// D3: what a frame on a link from `source` to `own` may be.
fn check_frame(bytes: &[u8], source: &str, own: &str) -> Frame {
    #[derive(serde::Deserialize)]
    struct Head {
        #[serde(rename = "type")]
        ty: String,
        #[serde(rename = "sourceRuntimeId")]
        source: Option<String>,
        #[serde(rename = "recipientRuntimeId")]
        recipient: Option<String>,
        seq: Option<u64>,
        sealed: Option<serde::de::IgnoredAny>,
    }
    let Ok(head) = serde_json::from_slice::<Head>(bytes) else {
        return Frame::Refused("a frame that is not a tunnel frame");
    };
    match head.ty.as_str() {
        "heartbeat" => return Frame::Heartbeat(head.seq.unwrap_or(0)),
        "heartbeat_ack" => return Frame::Ack,
        _ => {}
    }
    if !is_sealed_type(&head.ty) || head.sealed.is_none() {
        return Frame::Refused("a frame that is not sealed");
    }
    if head.source.as_deref() != Some(source) {
        return Frame::Refused("a frame from another computer than the one that said hello");
    }
    if head.recipient.as_deref() != Some(own) {
        return Frame::Refused("a frame for another computer");
    }
    Frame::Sealed(head.ty)
}

/// Frames per minute per type, as PekoHub caps them per runtime (D8).
#[derive(Default)]
struct Caps {
    window: Option<Instant>,
    counts: HashMap<String, u32>,
}

impl Caps {
    fn limit(ty: &str) -> u32 {
        match ty {
            "tunnel_channel_event" => 600,
            "tunnel_channel_invite" | "tunnel_channel_history_request" => 120,
            "tunnel_attachment_request" | "tunnel_attachment_chunk" => 400,
            "tunnel_group_invitation" | "tunnel_person_dm_invite" => 60,
            _ => 240,
        }
    }

    fn allow(&mut self, ty: &str) -> bool {
        let now = Instant::now();
        if self
            .window
            .is_none_or(|w| now.duration_since(w) >= Duration::from_mins(1))
        {
            self.window = Some(now);
            self.counts.clear();
        }
        let count = self.counts.entry(ty.to_string()).or_insert(0);
        *count += 1;
        *count <= Self::limit(ty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_takes_only_sealed_frames_between_its_two_computers() {
        let ok = br#"{"type":"tunnel_channel_event","sourceRuntimeId":"A","recipientRuntimeId":"B","sealed":"x"}"#;
        assert!(
            matches!(check_frame(ok, "A", "B"), Frame::Sealed(t) if t == "tunnel_channel_event")
        );
        assert!(matches!(
            check_frame(br#"{"type":"heartbeat","seq":4}"#, "A", "B"),
            Frame::Heartbeat(4)
        ));
        assert!(matches!(
            check_frame(br#"{"type":"heartbeat_ack","seq":4}"#, "A", "B"),
            Frame::Ack
        ));
        for refused in [
            // Unsealed, though of a sealed type.
            &br#"{"type":"tunnel_channel_event","sourceRuntimeId":"A","recipientRuntimeId":"B"}"#[..],
            // Not a sealed type.
            br#"{"type":"proxied_request","requestId":"r","principal":"p","payload":[]}"#,
            br#"{"type":"runtime_keys","requestId":"r","keys":[]}"#,
            // From another computer than the one that said hello.
            br#"{"type":"tunnel_channel_event","sourceRuntimeId":"C","recipientRuntimeId":"B","sealed":"x"}"#,
            // For another computer.
            br#"{"type":"tunnel_channel_event","sourceRuntimeId":"A","recipientRuntimeId":"C","sealed":"x"}"#,
            b"not json",
        ] {
            assert!(matches!(check_frame(refused, "A", "B"), Frame::Refused(_)));
        }
    }

    #[test]
    fn caps_count_per_type_per_minute() {
        let mut caps = Caps::default();
        for _ in 0..60 {
            assert!(caps.allow("tunnel_group_invitation"));
        }
        assert!(!caps.allow("tunnel_group_invitation"));
        assert!(caps.allow("tunnel_channel_event"));
    }
}
