//! The HTTP door (ADR-121): a service reaches the engine over HTTP with
//! an API key.
//!
//! A fourth transport beside the Unix datagram socket, the Windows named
//! pipe and loopback UDP — not new handlers. `POST /v1/call` carries one
//! `RequestPacket` as JSON; the door runs it through the same
//! [`crate::ipc::handlers::RequestDispatcher`] as the socket, with an
//! [`HttpSink`] in place of the datagram sink, and answers with the one
//! packet the request produced (`application/json`), or with every packet
//! as `text/event-stream` when the request produced more than one or the
//! client asked with `Accept: text/event-stream`.
//!
//! Off unless `[network] http` is set (D2). What it refuses (D4):
//!
//! - a start on an address other than this computer's own without
//!   `[network] http_hosts`, or with API keys disabled;
//! - a request whose `Host` is not the bound address (or `localhost` on
//!   a loopback bind) or one of `http_hosts` — DNS rebinding;
//! - any request carrying `Origin`, and `OPTIONS`; no CORS header is ever
//!   sent: this door is for services, not pages in a browser;
//! - a `POST` that is not `application/json` (the file upload route
//!   takes `application/octet-stream`);
//! - a request without a valid `Authorization: Bearer <key>`. The key is
//!   never read from a URL.
//!
//! Every call is recorded in the audit log under the key's id (D3).
//!
//! Files cross beside the packets (D5, [`files`]): `POST /v1/files`
//! takes one as `application/octet-stream` and answers `upload:<id>`,
//! which a send names in `attachments`; `GET /v1/files` gives back the
//! bytes of a post's attachment. Both need an `owner` key.

mod files;

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN};
use hyper::{Method, Request, Response, StatusCode};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::packet::RequestPacket;
use super::response_sink::ResponseSink;
use super::server::PeerAddr;
use super::uploads::Uploads;
use crate::common::types::config::NetworkConfig;
use peko_auth::caller::CallerContext;
use peko_auth::config::AuthConfig;
use peko_protocol::ipc::MAX_PACKET_SIZE;

/// The response body every route answers with.
pub(crate) type DoorBody = BoxBody<Bytes, Infallible>;

/// How long a client has to send its request headers.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How often uploads past their hour are deleted.
const UPLOAD_SWEEP: Duration = Duration::from_mins(5);

/// Packets buffered between a handler and a slow client before the
/// handler waits, as it waits on a full socket buffer.
const SINK_DEPTH: usize = 64;

/// Where the door listens and which `Host` names it answers to, checked
/// against the rules of ADR-121 D4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoorConfig {
    pub addr: SocketAddr,
    pub hosts: Vec<String>,
}

impl DoorConfig {
    /// Read `[network]`. `Ok(None)` when the door is off; an error when
    /// the engine must refuse to start.
    pub fn from_config(network: &NetworkConfig, auth: &AuthConfig) -> anyhow::Result<Option<Self>> {
        let Some(http) = network.http.as_deref().map(str::trim) else {
            return Ok(None);
        };
        let addr: SocketAddr = http.parse().map_err(|e| {
            anyhow::anyhow!("[network] http = {http:?} is not an address and port ({e})")
        })?;
        let hosts: Vec<String> = network
            .http_hosts
            .iter()
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect();
        if !addr.ip().is_loopback() && hosts.is_empty() {
            anyhow::bail!(
                "[network] http = {http:?} listens beyond this computer: set [network] \
                 http_hosts to the names services call the engine by (e.g. [\"peko:{}\"])",
                addr.port()
            );
        }
        if !auth.enable_api_key() {
            anyhow::bail!(
                "[network] http needs API keys: set enable_api_key = true in \
                 runtime/auth_config.toml, then make a key with `peko auth api-key create`"
            );
        }
        Ok(Some(Self { addr, hosts }))
    }

    /// The `Host` values a request may carry once bound to `bound`.
    fn allowed_hosts(&self, bound: SocketAddr) -> Vec<String> {
        let mut allowed = self.hosts.clone();
        allowed.push(bound.to_string());
        if bound.ip().is_loopback() {
            allowed.push(format!("localhost:{}", bound.port()));
        }
        allowed
    }
}

/// What the door needs from the engine. `AppState` is the one
/// implementor in production; tests use a fake.
#[async_trait]
pub(crate) trait DoorHost: Send + Sync + 'static {
    /// The caller an API key stands for, if it is a valid, enabled key.
    async fn caller_for_key(&self, key: &str) -> Option<CallerContext>;

    /// Whether the caller's per-key rate limit admits one more request.
    async fn admit(&self, caller: &CallerContext) -> bool;

    /// Run one request through the handlers. Returns when the last
    /// packet has been written to `sink`.
    async fn call(
        &self,
        request: RequestPacket,
        caller: CallerContext,
        sink: HttpSink,
        peer: PeerAddr,
    ) -> anyhow::Result<()>;

    /// Record what the caller did, under its key id.
    async fn record(&self, caller: &CallerContext, event: &str, details: serde_json::Value);

    /// Every peko's workspace: `GET /v1/files?path=` serves only files
    /// in their attachments.
    async fn workspaces(&self) -> Vec<std::path::PathBuf>;

    /// A channel's file store (ADR-094), if the channel is here.
    async fn channel_files_dir(&self, channel: &str) -> Option<std::path::PathBuf>;
}

/// A [`ResponseSink`] that hands each packet to the HTTP response. When
/// the client has gone away the receiver is dropped and every write
/// fails, as a write to a socket client that went away does.
pub(crate) struct HttpSink {
    tx: mpsc::Sender<Bytes>,
}

#[async_trait]
impl ResponseSink for HttpSink {
    async fn send_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        self.tx
            .send(Bytes::copy_from_slice(bytes))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the HTTP client went away"))
    }
}

/// Bind and serve the door if `[network] http` turns it on. Returns the
/// bound address (for logs and tests). Refusals of D4 are errors.
pub(crate) async fn start(
    host: Arc<dyn DoorHost>,
    config: DoorConfig,
    uploads: Arc<Uploads>,
    shutdown: tokio::sync::broadcast::Receiver<()>,
) -> anyhow::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind(config.addr)
        .await
        .map_err(|e| anyhow::anyhow!("[network] http: could not listen on {}: {e}", config.addr))?;
    let bound = listener.local_addr()?;
    let door = Arc::new(Door {
        host,
        allowed_hosts: config.allowed_hosts(bound),
        next_request_id: AtomicU64::new(1),
        uploads,
    });
    info!("HTTP door listening on {bound} (ADR-121)");
    let handle = tokio::spawn(serve(listener, door, shutdown));
    Ok((bound, handle))
}

pub(crate) struct Door {
    host: Arc<dyn DoorHost>,
    allowed_hosts: Vec<String>,
    /// For requests that arrive without a `request_id`.
    next_request_id: AtomicU64,
    /// Files sent in, waiting for a send (D5).
    uploads: Arc<Uploads>,
}

async fn serve(
    listener: TcpListener,
    door: Arc<Door>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let mut sweep = tokio::time::interval(UPLOAD_SWEEP);
    loop {
        tokio::select! {
            _ = sweep.tick() => door.uploads.sweep(),
            accepted = listener.accept() => match accepted {
                Ok((stream, remote)) => {
                    let door = Arc::clone(&door);
                    tokio::spawn(async move {
                        let service = hyper::service::service_fn(move |req| {
                            let door = Arc::clone(&door);
                            async move { Ok::<_, Infallible>(door.answer(req, remote).await) }
                        });
                        let conn = hyper::server::conn::http1::Builder::new()
                            .timer(hyper_util::rt::TokioTimer::new())
                            .header_read_timeout(HEADER_READ_TIMEOUT)
                            .serve_connection(hyper_util::rt::TokioIo::new(stream), service);
                        if let Err(e) = conn.await {
                            debug!("HTTP door connection from {remote} ended: {e}");
                        }
                    });
                }
                Err(e) => {
                    error!("HTTP door accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = shutdown.recv() => {
                info!("HTTP door stopping");
                break;
            }
        }
    }
}

/// A door-level refusal: status plus a one-line reason as
/// `{"error": "..."}`. Answers from the engine itself are packets with
/// status 200, an `error` packet included.
pub(crate) fn refuse(status: StatusCode, reason: &str) -> Response<DoorBody> {
    json_response(status, serde_json::json!({ "error": reason }).to_string())
}

pub(crate) fn json_response(status: StatusCode, body: impl Into<Bytes>) -> Response<DoorBody> {
    let mut response = Response::new(Full::new(body.into()).boxed());
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

/// `type/subtype` of a header, lowercased, parameters dropped.
pub(crate) fn media_type(value: Option<&HeaderValue>) -> Option<String> {
    let value = value?.to_str().ok()?;
    Some(value.split(';').next()?.trim().to_ascii_lowercase())
}

impl Door {
    async fn answer(&self, req: Request<Incoming>, remote: SocketAddr) -> Response<DoorBody> {
        match self.admit(&req).await {
            Ok(caller) => self.route(req, caller, remote).await,
            Err(refusal) => refusal,
        }
    }

    /// The guards of ADR-121 D4, then the key (D3) and its rate limit.
    async fn admit<B>(&self, req: &Request<B>) -> Result<CallerContext, Response<DoorBody>> {
        if req.headers().contains_key(ORIGIN) {
            return Err(refuse(
                StatusCode::FORBIDDEN,
                "requests from web pages are refused (Origin header)",
            ));
        }
        if req.method() == Method::OPTIONS {
            return Err(refuse(StatusCode::METHOD_NOT_ALLOWED, "OPTIONS is refused"));
        }
        let host = req
            .headers()
            .get(HOST)
            .and_then(|h| h.to_str().ok())
            .map(|h| h.trim().to_ascii_lowercase());
        match host {
            Some(host) if self.allowed_hosts.contains(&host) => {}
            _ => {
                return Err(refuse(
                    StatusCode::FORBIDDEN,
                    "Host is not one this engine answers to ([network] http_hosts)",
                ))
            }
        }
        let key = req
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                v.strip_prefix("Bearer ")
                    .or_else(|| v.strip_prefix("bearer "))
            })
            .map(str::trim);
        let Some(key) = key.filter(|k| !k.is_empty()) else {
            return Err(refuse(
                StatusCode::UNAUTHORIZED,
                "an API key is required: Authorization: Bearer <key>",
            ));
        };
        let Some(caller) = self.host.caller_for_key(key).await else {
            return Err(refuse(StatusCode::UNAUTHORIZED, "the API key is not valid"));
        };
        if !self.host.admit(&caller).await {
            return Err(refuse(
                StatusCode::TOO_MANY_REQUESTS,
                "rate limit exceeded; try again later",
            ));
        }
        Ok(caller)
    }

    async fn route(
        &self,
        req: Request<Incoming>,
        caller: CallerContext,
        remote: SocketAddr,
    ) -> Response<DoorBody> {
        let path = req.uri().path().to_string();
        match (req.method().clone(), path.as_str()) {
            (Method::POST, "/v1/call") => self.call(req, caller, remote).await,
            (_, "/v1/call") => refuse(StatusCode::METHOD_NOT_ALLOWED, "use POST"),
            (Method::POST, "/v1/files") => files::upload(self, req, caller).await,
            (Method::GET, "/v1/files") => files::download(self, req, caller).await,
            (_, "/v1/files") => refuse(StatusCode::METHOD_NOT_ALLOWED, "use POST or GET"),
            _ => refuse(StatusCode::NOT_FOUND, "no such route"),
        }
    }

    /// `POST /v1/call`.
    async fn call(
        &self,
        req: Request<Incoming>,
        caller: CallerContext,
        remote: SocketAddr,
    ) -> Response<DoorBody> {
        if media_type(req.headers().get(CONTENT_TYPE)).as_deref() != Some("application/json") {
            return refuse(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "the body must be application/json",
            );
        }
        let wants_stream = req
            .headers()
            .get_all(ACCEPT)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| v.to_ascii_lowercase().contains("text/event-stream"));
        let body = match http_body_util::Limited::new(req.into_body(), MAX_PACKET_SIZE)
            .collect()
            .await
        {
            Ok(collected) => collected.to_bytes(),
            Err(_) => {
                return refuse(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    &format!("a request is at most {MAX_PACKET_SIZE} bytes"),
                )
            }
        };
        let request = match self.parse_request(&body) {
            Ok(request) => request,
            Err(reason) => return refuse(StatusCode::BAD_REQUEST, &reason),
        };
        let kind = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
            .unwrap_or_default();
        self.host
            .record(
                &caller,
                "http.call",
                serde_json::json!({
                    "key_id": caller.api_key_id(),
                    "request": kind,
                    "request_id": request.request_id(),
                    "remote": remote.to_string(),
                }),
            )
            .await;

        let (tx, rx) = mpsc::channel(SINK_DEPTH);
        let host = Arc::clone(&self.host);
        let request_id = request.request_id();
        let task = tokio::spawn(async move {
            let result = host
                .call(request, caller, HttpSink { tx }, PeerAddr::Ip(remote))
                .await;
            if let Err(e) = &result {
                warn!("HTTP door request {request_id} ({kind}) failed: {e:#}");
            }
            result
        });
        answer_packets(rx, task, wants_stream).await
    }

    /// The body as a `RequestPacket`, with a `request_id` filled in when
    /// the client left it out.
    fn parse_request(&self, body: &[u8]) -> Result<RequestPacket, String> {
        let mut value: serde_json::Value =
            serde_json::from_slice(body).map_err(|e| format!("the body is not JSON: {e}"))?;
        let Some(object) = value.as_object_mut() else {
            return Err("the body must be one request object".to_string());
        };
        if !object.contains_key("request_id") {
            let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
            object.insert("request_id".to_string(), id.into());
        }
        serde_json::from_value(value).map_err(|e| format!("not a request: {e}"))
    }
}

fn is_heartbeat(packet: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(packet)
        .ok()
        .and_then(|v| {
            v.get("type")
                .and_then(|t| t.as_str())
                .map(|t| t == "heartbeat")
        })
        .unwrap_or(false)
}

/// Turn the packets a handler writes into the HTTP answer: the single
/// packet as JSON, or the stream as `text/event-stream` once a second
/// packet comes (or at once when the client asked for a stream).
async fn answer_packets(
    mut rx: mpsc::Receiver<Bytes>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    wants_stream: bool,
) -> Response<DoorBody> {
    let mut first: Vec<Bytes> = Vec::new();
    if !wants_stream {
        loop {
            match rx.recv().await {
                Some(packet) if first.is_empty() && is_heartbeat(&packet) => continue,
                Some(packet) if first.is_empty() => first.push(packet),
                Some(packet) if is_heartbeat(&packet) => continue,
                Some(packet) => {
                    // A second packet: this request streams.
                    first.push(packet);
                    break;
                }
                None => {
                    // The handler is done.
                    return match first.pop() {
                        Some(packet) => json_response(StatusCode::OK, packet),
                        None => {
                            let reason = match task.await {
                                Ok(Err(e)) => format!("the engine failed: {e:#}"),
                                _ => "the engine sent no answer".to_string(),
                            };
                            refuse(StatusCode::BAD_GATEWAY, &reason)
                        }
                    };
                }
            }
        }
    }
    drop(task); // keeps running; it ends when the stream does
    event_stream(first, rx)
}

fn sse_frame(packet: &[u8]) -> Frame<Bytes> {
    let mut event = Vec::with_capacity(packet.len() + 8);
    event.extend_from_slice(b"data: ");
    event.extend_from_slice(packet);
    event.extend_from_slice(b"\n\n");
    Frame::data(Bytes::from(event))
}

fn event_stream(first: Vec<Bytes>, rx: mpsc::Receiver<Bytes>) -> Response<DoorBody> {
    let head = futures::stream::iter(first);
    let tail = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|packet| (packet, rx))
    });
    let frames = futures::StreamExt::map(futures::StreamExt::chain(head, tail), |packet| {
        Ok::<_, Infallible>(sse_frame(&packet))
    });
    let mut response = Response::new(BodyExt::boxed(StreamBody::new(frames)));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(
        hyper::header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    response
}

// ---------------------------------------------------------------------------
// The engine side
// ---------------------------------------------------------------------------

#[async_trait]
impl DoorHost for crate::daemon::state::AppState {
    async fn caller_for_key(&self, key: &str) -> Option<CallerContext> {
        if !self.auth_config().enable_api_key() {
            return None;
        }
        let entry = self.api_key_verifier()?.verify(key).await?;
        Some(CallerContext::from_api_key(entry.id, entry.scopes))
    }

    async fn admit(&self, caller: &CallerContext) -> bool {
        match self.rate_limiter() {
            Some(limiter) => limiter.check(&caller.rate_limit_bucket, false).await,
            None => true,
        }
    }

    async fn call(
        &self,
        request: RequestPacket,
        caller: CallerContext,
        sink: HttpSink,
        peer: PeerAddr,
    ) -> anyhow::Result<()> {
        Box::pin(crate::ipc::handlers::RequestDispatcher::dispatch(
            self.clone(),
            request,
            &caller,
            &sink,
            &peer,
        ))
        .await
    }

    async fn record(&self, caller: &CallerContext, event: &str, details: serde_json::Value) {
        if let Err(e) = self
            .observability()
            .audit_with_caller(Some(&caller.subject()), event, None, details)
            .await
        {
            warn!("could not record {event} in the audit log: {e:#}");
        }
    }

    async fn workspaces(&self) -> Vec<std::path::PathBuf> {
        self.principal_manager()
            .list_all()
            .await
            .iter()
            .map(|p| p.workspace_path.clone())
            .collect()
    }

    async fn channel_files_dir(&self, channel: &str) -> Option<std::path::PathBuf> {
        let channel = peko_channel::ChannelId::parse(channel)?;
        self.channel_port().files_dir(&channel).await.ok()
    }
}

/// Start the door from the engine's `[network]` config, if it is on.
pub(crate) async fn start_for(
    state: &crate::daemon::state::AppState,
    config: Option<DoorConfig>,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    let Some(config) = config else {
        return Ok(None);
    };
    let host: Arc<dyn DoorHost> = Arc::new(state.clone());
    let uploads = Uploads::new(super::uploads::default_dir());
    let (_, handle) = start(host, config, uploads, state.subscribe_shutdown()).await?;
    Ok(Some(handle))
}

#[cfg(test)]
mod tests;
