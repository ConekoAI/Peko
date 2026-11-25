//! The router (ADR-125 D6): seals what this runtime sends and chooses,
//! per frame, the path it takes.
//!
//! It owns the queue every [`TunnelHandle`] sends into and outlives any
//! one PekoHub connection, so links carry frames while PekoHub is
//! unreachable. A frame sealed to a computer goes on the link to it when
//! one carries frames, through PekoHub otherwise, unless `direct` (or
//! the computer's word that it takes no relayed frames) holds it (D7).
//! What arrives, by either path, is opened by the same `Sealer`, put back
//! in its sender's order (`seq`), and handed to the same dispatcher.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use futures::StreamExt;
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::addresses::{self, DirectAddresses};
use super::conn::{self, Link};
use super::file::LinksFile;
use super::order::{is_ordered_type, Reorder};
use super::tls::LinkIdentity;
use super::{audit, NetworkListening, NetworkPeer, NetworkStatus, Path, ADDRESSES_TYPE};
use crate::common::types::config::{Connection, DirectWith, NetworkConfig, DEFAULT_DIRECT_LISTEN};
use crate::security::device_list::DeviceLists;
use crate::tunnel::backoff::ExponentialBackoff;
use crate::tunnel::client::TunnelHandle;
use crate::tunnel::protocol::TunnelMessage;
use crate::tunnel::seal::{is_sealed_type, Inbound, Outgoing, SealPolicy, Sealer};

/// How many frames for PekoHub wait while it is unreachable.
pub const HUB_QUEUE: usize = 1024;
/// How long a new link answers heartbeats before it carries frames (D6).
pub const WARM_FOR: Duration = Duration::from_secs(10);
/// How long a link may take to answer after a network change (D6).
pub const DOUBT_FOR: Duration = Duration::from_secs(3);
/// How long an unanswered request is kept to be asked again.
const INFLIGHT_FOR: Duration = Duration::from_mins(2);
/// How often the router looks for links to make and links to drop.
const MAINTAIN_EVERY: Duration = Duration::from_secs(5);
/// Network changes closer together than this are one change.
const SETTLE_FOR: Duration = Duration::from_millis(500);

/// Who handles what arrives (the dispatcher).
pub type Handler = Arc<
    dyn Fn(TunnelMessage, TunnelHandle) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
>;

/// Requests answered by another computer, and the type of the answer.
/// A request still unanswered when the path to its computer changes is
/// asked again on the new path (D6).
const ANSWERS: &[(&str, &str)] = &[
    ("tunnel_channel_history_request", "tunnel_channel_history"),
    ("tunnel_attachment_request", "tunnel_attachment_chunk"),
    ("tunnel_peko_models_request", "tunnel_peko_models"),
    ("tunnel_peko_approvals_request", "tunnel_peko_approvals"),
    (
        "tunnel_peko_approval_decide",
        "tunnel_peko_approval_decided",
    ),
    ("tunnel_peko_browser_request", "tunnel_peko_browser"),
];

fn is_answer_type(ty: &str) -> bool {
    ANSWERS.iter().any(|(_, answer)| *answer == ty)
}

/// `(request_id, recipient)` of a request [`ANSWERS`] lists.
fn request_route(msg: &TunnelMessage) -> Option<(&str, &str)> {
    match msg {
        TunnelMessage::TunnelChannelHistoryRequest {
            request_id,
            recipient_runtime_id,
            ..
        }
        | TunnelMessage::TunnelAttachmentRequest {
            request_id,
            recipient_runtime_id,
            ..
        }
        | TunnelMessage::TunnelPekoModelsRequest {
            request_id,
            recipient_runtime_id,
            ..
        }
        | TunnelMessage::TunnelPekoApprovalsRequest {
            request_id,
            recipient_runtime_id,
            ..
        }
        | TunnelMessage::TunnelPekoApprovalDecide {
            request_id,
            recipient_runtime_id,
            ..
        }
        | TunnelMessage::TunnelPekoBrowserRequest {
            request_id,
            recipient_runtime_id,
            ..
        } => Some((request_id, recipient_runtime_id)),
        _ => None,
    }
}

/// Where a frame came in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Via {
    Hub,
    Link(String),
}

/// What a router needs at start.
pub struct RouterConfig {
    pub runtime_id: String,
    pub signing_key: Arc<SigningKey>,
    /// `None` only in tests that speak to a fake hub in the clear.
    pub sealer: Option<Arc<Sealer>>,
    pub devices: Option<Arc<DeviceLists>>,
    /// The owner's account (for D4).
    pub policy: Option<Arc<dyn SealPolicy>>,
    pub network: NetworkConfig,
    /// `runtime/links.json`; `None`: in memory only.
    pub links_path: Option<PathBuf>,
}

struct InFlight {
    recipient: String,
    msg: TunnelMessage,
    at: Instant,
    asked_again: bool,
}

/// What the router knows of one computer besides its link.
#[derive(Default)]
struct PeerState {
    dialing: bool,
    next_try: Option<Instant>,
    backoff: Option<ExponentialBackoff>,
    last_error: Option<String>,
    told_this_session: bool,
}

struct Listener {
    address: Option<String>,
    error: Option<String>,
    cancel: Option<CancellationToken>,
}

pub(crate) struct Inner {
    pub(crate) runtime_id: String,
    pub(crate) signing_key: Arc<SigningKey>,
    sealer: Option<Arc<Sealer>>,
    devices: Option<Arc<DeviceLists>>,
    policy: Option<Arc<dyn SealPolicy>>,
    settings: RwLock<NetworkConfig>,
    tx: mpsc::Sender<TunnelMessage>,
    rx: Mutex<Option<mpsc::Receiver<TunnelMessage>>>,
    hub_tx: mpsc::Sender<Vec<u8>>,
    hub_up: AtomicBool,
    /// Wakes the PekoHub connection's reconnect wait (D6).
    pub(crate) hub_kick: Arc<Notify>,
    handler: RwLock<Option<Handler>>,
    pub(crate) links: Mutex<HashMap<String, Arc<Link>>>,
    peers: Mutex<HashMap<String, PeerState>>,
    /// `direct_peers` address → the runtime that answered there.
    peer_addresses: Mutex<HashMap<String, (String, Option<Instant>)>>,
    paths: Mutex<HashMap<String, Path>>,
    inflight: Mutex<HashMap<String, InFlight>>,
    reorder: Mutex<Reorder<TunnelMessage>>,
    reorder_kick: Notify,
    maintain_kick: Notify,
    file: Mutex<LinksFile>,
    links_path: Option<PathBuf>,
    listener: Mutex<Listener>,
    pub(crate) identity: Option<LinkIdentity>,
    pub(crate) pending_hellos: Arc<tokio::sync::Semaphore>,
    link_ids: AtomicU64,
    started: AtomicBool,
    cancel: CancellationToken,
    /// Frames from PekoHub dropped unopened in `direct` (D7), for tests
    /// and the status.
    dropped_relayed: AtomicU64,
    /// [`WARM_FOR`], in milliseconds (shorter in tests).
    warm_ms: AtomicU64,
}

/// See the module docs. Cheap to clone.
#[derive(Clone)]
pub struct Router {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field("runtime_id", &self.inner.runtime_id)
            .finish_non_exhaustive()
    }
}

impl Router {
    /// A router, and the queue of frames for PekoHub that the PekoHub
    /// connection's write loop drains.
    #[must_use]
    pub fn new(config: RouterConfig) -> (Self, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(crate::tunnel::client::TUNNEL_OUTBOUND_BUFFER_SIZE);
        let (hub_tx, hub_rx) = mpsc::channel(HUB_QUEUE);
        let identity = match LinkIdentity::generate() {
            Ok(identity) => Some(identity),
            Err(e) => {
                warn!("direct links are off: {e}");
                None
            }
        };
        let file = LinksFile::load(config.links_path.as_deref());
        let inner = Inner {
            runtime_id: config.runtime_id,
            signing_key: config.signing_key,
            sealer: config.sealer,
            devices: config.devices,
            policy: config.policy,
            settings: RwLock::new(config.network),
            tx,
            rx: Mutex::new(Some(rx)),
            hub_tx,
            hub_up: AtomicBool::new(false),
            hub_kick: Arc::new(Notify::new()),
            handler: RwLock::new(None),
            links: Mutex::new(HashMap::new()),
            peers: Mutex::new(HashMap::new()),
            peer_addresses: Mutex::new(HashMap::new()),
            paths: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            reorder: Mutex::new(Reorder::default()),
            reorder_kick: Notify::new(),
            maintain_kick: Notify::new(),
            file: Mutex::new(file),
            links_path: config.links_path,
            listener: Mutex::new(Listener {
                address: None,
                error: None,
                cancel: None,
            }),
            identity,
            pending_hellos: Arc::new(tokio::sync::Semaphore::new(conn::MAX_PENDING_HELLOS)),
            link_ids: AtomicU64::new(1),
            started: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            dropped_relayed: AtomicU64::new(0),
            warm_ms: AtomicU64::new(WARM_FOR.as_millis() as u64),
        };
        (
            Self {
                inner: Arc::new(inner),
            },
            hub_rx,
        )
    }

    /// The handle every part of the runtime sends through.
    #[must_use]
    pub fn handle(&self) -> TunnelHandle {
        TunnelHandle::from_sender(self.inner.tx.clone())
    }

    /// Who handles what arrives.
    pub fn set_handler(&self, handler: Handler) {
        *self.inner.handler.write().unwrap() = Some(handler);
    }

    pub(crate) fn handler(&self) -> Option<Handler> {
        self.inner.handler.read().unwrap().clone()
    }

    /// What wakes the PekoHub connection's reconnect wait.
    #[must_use]
    pub fn hub_kick(&self) -> Arc<Notify> {
        Arc::clone(&self.inner.hub_kick)
    }

    /// Start sending, listening and dialing. Idempotent; needs a tokio
    /// runtime.
    pub fn start(&self) {
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let rx = self.inner.rx.lock().unwrap().take();
        if let Some(rx) = rx {
            tokio::spawn(self.clone().pump(rx));
        }
        tokio::spawn(self.clone().release_held());
        tokio::spawn(self.clone().maintain());
        tokio::spawn(self.clone().watch_network());
        if self.connection().links() {
            self.start_listener();
        }
    }

    /// Stop everything this router started.
    pub fn stop(&self) {
        self.inner.cancel.cancel();
        self.stop_listener();
        for link in self.take_links() {
            link.close("stopped");
        }
    }

    /// The setting in force.
    #[must_use]
    pub fn connection(&self) -> Connection {
        self.inner.settings.read().unwrap().connection
    }

    fn direct_with(&self) -> DirectWith {
        self.inner.settings.read().unwrap().direct_with
    }

    /// The `[network]` settings in force.
    #[must_use]
    pub fn settings(&self) -> NetworkConfig {
        self.inner.settings.read().unwrap().clone()
    }

    /// How long a new link answers heartbeats before it carries frames.
    #[cfg(test)]
    pub(crate) fn set_warm_for(&self, warm: Duration) {
        self.inner
            .warm_ms
            .store(warm.as_millis() as u64, Ordering::Relaxed);
    }

    /// Frames from PekoHub dropped unopened since start (D7).
    #[must_use]
    pub fn dropped_relayed(&self) -> u64 {
        self.inner.dropped_relayed.load(Ordering::Relaxed)
    }

    // -- Outbound ---------------------------------------------------------

    async fn pump(self, mut rx: mpsc::Receiver<TunnelMessage>) {
        loop {
            let msg = tokio::select! {
                msg = rx.recv() => msg,
                () = self.inner.cancel.cancelled() => None,
            };
            let Some(msg) = msg else { break };
            self.track(&msg);
            let frames = match &self.inner.sealer {
                Some(sealer) => sealer.outbound_routed(msg).await,
                None => match msg.to_bytes() {
                    Ok(bytes) => vec![plain(bytes)],
                    Err(e) => {
                        warn!("could not serialize a tunnel message: {e}");
                        continue;
                    }
                },
            };
            for frame in frames {
                self.route(frame);
            }
        }
    }

    /// Send one frame by the path D6 chooses.
    fn route(&self, frame: Outgoing) {
        let Some(to) = frame.to.clone().filter(|_| frame.sealed) else {
            self.to_hub(frame.bytes);
            return;
        };
        if is_ordered_type(&frame.ty) {
            audit::routed(
                &frame.ty,
                request_id_of(&frame.bytes).as_deref(),
                &to,
                self.path_to(&to),
            );
        }
        if let Some(link) = self.carrying(&to) {
            match link.send(frame.bytes) {
                Ok(()) => return,
                Err(bytes) => {
                    debug!(runtime = %to, "link queue full or closed; frame takes another path");
                    return self.route_off_link(&to, &frame.ty, bytes);
                }
            }
        }
        self.route_off_link(&to, &frame.ty, frame.bytes);
    }

    fn route_off_link(&self, to: &str, ty: &str, bytes: Vec<u8>) {
        if ty != ADDRESSES_TYPE && !self.relays_to(to) {
            debug!(runtime = %to, %ty, "no link and no relay to this computer; the frame waits");
            return;
        }
        self.to_hub(bytes);
    }

    /// Whether frames to `to` may go through PekoHub: not in `direct`,
    /// nor to a computer that said it takes none (D7).
    fn relays_to(&self, to: &str) -> bool {
        self.connection() != Connection::Direct
            && self
                .inner
                .file
                .lock()
                .unwrap()
                .peers
                .get(to)
                .is_none_or(|p| p.relay)
    }

    fn to_hub(&self, bytes: Vec<u8>) {
        if let Err(e) = self.inner.hub_tx.try_send(bytes) {
            warn!("PekoHub's queue is full; a frame was not sent: {e}");
        }
    }

    /// Remember a request until it is answered (D6).
    fn track(&self, msg: &TunnelMessage) {
        let Some((request_id, recipient)) = request_route(msg) else {
            return;
        };
        let mut inflight = self.inner.inflight.lock().unwrap();
        inflight.retain(|_, f| f.at.elapsed() < INFLIGHT_FOR);
        inflight
            .entry(request_id.to_string())
            .or_insert_with(|| InFlight {
                recipient: recipient.to_string(),
                msg: msg.clone(),
                at: Instant::now(),
                asked_again: false,
            });
    }

    /// The link to `runtime` that carries frames now.
    fn carrying(&self, runtime: &str) -> Option<Arc<Link>> {
        let direct = self.connection() == Connection::Direct;
        self.inner
            .links
            .lock()
            .unwrap()
            .get(runtime)
            .filter(|l| l.carries(direct))
            .cloned()
    }

    /// The path frames to `runtime` take now.
    #[must_use]
    pub fn path_to(&self, runtime: &str) -> Path {
        if self.carrying(runtime).is_some() {
            return Path::Direct;
        }
        if !self.relays_to(runtime) {
            return Path::Waiting;
        }
        if self.inner.hub_up.load(Ordering::SeqCst) {
            Path::Relay
        } else {
            Path::Offline
        }
    }

    /// Look again at the path to `runtime`; act when it changed.
    pub(crate) fn recheck(&self, runtime: &str) {
        let now = self.path_to(runtime);
        audit::set_path(runtime, now);
        let before = self
            .inner
            .paths
            .lock()
            .unwrap()
            .insert(runtime.to_string(), now);
        if before.is_some_and(|b| b != now) {
            info!(runtime = %runtime, from = ?before, to = ?now, "path changed");
            self.path_changed(runtime, now);
        }
    }

    fn recheck_all(&self) {
        let mut runtimes: HashSet<String> =
            self.inner.paths.lock().unwrap().keys().cloned().collect();
        runtimes.extend(self.inner.links.lock().unwrap().keys().cloned());
        for runtime in runtimes {
            self.recheck(&runtime);
        }
    }

    /// The path to `runtime` changed: ask its unanswered requests again
    /// on the new path, and send what waits for it (D6).
    fn path_changed(&self, runtime: &str, now: Path) {
        if !matches!(now, Path::Direct | Path::Relay) {
            return;
        }
        let again: Vec<TunnelMessage> = {
            let mut inflight = self.inner.inflight.lock().unwrap();
            inflight
                .values_mut()
                .filter(|f| f.recipient == runtime && !f.asked_again)
                .map(|f| {
                    f.asked_again = true;
                    f.msg.clone()
                })
                .collect()
        };
        for msg in again {
            if self.inner.tx.try_send(msg).is_err() {
                warn!("could not ask a request again: the queue is full");
            }
        }
        // As PekoHub's `peer_runtime_online`: the outbox resends what
        // waits for that computer, on whichever path is there now.
        self.deliver(TunnelMessage::PeerRuntimeOnline {
            runtime_id: runtime.to_string(),
        });
    }

    fn deliver(&self, msg: TunnelMessage) {
        if let Some(handler) = self.handler() {
            let handle = self.handle();
            tokio::spawn(async move { handler(msg, handle).await });
        }
    }

    // -- Inbound ----------------------------------------------------------

    /// A frame from PekoHub: the messages to handle now.
    pub async fn receive_from_hub(&self, bytes: &[u8]) -> Vec<TunnelMessage> {
        if self.connection() == Connection::Direct {
            let ty = frame_type(bytes);
            if ty
                .as_deref()
                .is_some_and(|t| is_sealed_type(t) && t != ADDRESSES_TYPE)
            {
                self.inner.dropped_relayed.fetch_add(1, Ordering::Relaxed);
                debug!(ty = ?ty, "Direct only: a sealed frame from PekoHub dropped unopened");
                return Vec::new();
            }
            if ty.as_deref() == Some("proxied_request") {
                self.refuse_proxied(bytes);
                return Vec::new();
            }
        }
        self.receive(bytes, Via::Hub).await
    }

    /// D7: a visitor's chat through PekoHub's proxy is refused.
    fn refuse_proxied(&self, bytes: &[u8]) {
        let Ok(TunnelMessage::ProxiedRequest { request_id, .. }) = TunnelMessage::from_bytes(bytes)
        else {
            return;
        };
        let payload = serde_json::json!({
            "status": 503,
            "body": "This computer takes no conversations through PekoHub.",
        });
        let _ = self
            .handle()
            .send_response(request_id, payload.to_string().into_bytes());
    }

    /// A frame from a link: handled here.
    pub(crate) async fn receive_from_link(&self, runtime: &str, bytes: &[u8]) {
        for msg in self.receive(bytes, Via::Link(runtime.to_string())).await {
            self.deliver(msg);
        }
    }

    async fn receive(&self, bytes: &[u8], via: Via) -> Vec<TunnelMessage> {
        let Some(sealer) = self.inner.sealer.clone() else {
            return match TunnelMessage::from_bytes(bytes) {
                Ok(msg) => vec![msg],
                Err(e) => {
                    warn!("failed to parse a tunnel message: {e}");
                    Vec::new()
                }
            };
        };
        let (inbound, meta) = sealer.inbound_meta(bytes).await;
        let msg = match inbound {
            Inbound::Message(msg) => *msg,
            Inbound::MessageAnd(msg, first) => {
                for ask in first {
                    let _ = self.inner.tx.send(ask).await;
                }
                *msg
            }
            Inbound::Release(msgs) => {
                for msg in msgs {
                    let _ = self.inner.tx.send(msg).await;
                }
                return Vec::new();
            }
            Inbound::Ignore => return Vec::new(),
        };
        if let TunnelMessage::TunnelDirectAddresses {
            source_runtime_id,
            statement,
            ..
        } = &msg
        {
            self.on_addresses(source_runtime_id, statement).await;
            return Vec::new();
        }
        let request_id = request_id_of(bytes);
        if let Some(request_id) = &request_id {
            if is_answer_type(&meta.ty) {
                self.inner.inflight.lock().unwrap().remove(request_id);
            }
            if is_sealed_type(&meta.ty) {
                audit::received_by(
                    request_id,
                    match via {
                        Via::Hub => Path::Relay,
                        Via::Link(_) => Path::Direct,
                    },
                );
            }
        }
        match (meta.seq, meta.source) {
            (Some(seq), Some(source)) if is_ordered_type(&meta.ty) => {
                let ready =
                    self.inner
                        .reorder
                        .lock()
                        .unwrap()
                        .push(&source, seq, msg, Instant::now());
                self.inner.reorder_kick.notify_one();
                ready
            }
            _ => vec![msg],
        }
    }

    /// Hand on frames whose hold ran out.
    async fn release_held(self) {
        loop {
            let deadline = self.inner.reorder.lock().unwrap().next_deadline();
            let wait = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = wait => {
                    let due = self.inner.reorder.lock().unwrap().due(Instant::now());
                    for msg in due {
                        self.deliver(msg);
                    }
                }
                () = self.inner.reorder_kick.notified() => {}
                () = self.inner.cancel.cancelled() => return,
            }
        }
    }

    // -- PekoHub ----------------------------------------------------------

    /// The PekoHub connection is up (`TunnelReady`) or gone.
    pub fn hub_state(&self, up: bool) {
        let was = self.inner.hub_up.swap(up, Ordering::SeqCst);
        if was == up {
            return;
        }
        self.recheck_all();
        if up && self.connection().links() {
            // D5: tell addresses when connecting to PekoHub.
            let this = self.clone();
            tokio::spawn(async move { this.tell_all(true).await });
        }
    }

    /// Whether the PekoHub connection is up.
    #[must_use]
    pub fn hub_up(&self) -> bool {
        self.inner.hub_up.load(Ordering::SeqCst)
    }

    // -- Addresses (D5) ---------------------------------------------------

    /// The computers this one links with (D4): those on the owner's own
    /// device list, and with `people` every computer it seals to.
    pub async fn admitted(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        if let (Some(policy), Some(devices)) = (&self.inner.policy, &self.inner.devices) {
            if let Some(account) = policy.own_account().await {
                if let Some(list) = devices.list(&format!("user:hub:{account}")) {
                    out.extend(list.computers().map(|d| d.id.clone()));
                }
            }
        }
        if self.direct_with() == DirectWith::People {
            if let Some(sealer) = &self.inner.sealer {
                out.extend(sealer.known_runtimes());
            }
        }
        out.remove(&self.inner.runtime_id);
        out
    }

    /// What this computer tells (D5): `direct_addresses` when set, else
    /// its interfaces' private addresses with the port it listens on.
    #[must_use]
    pub fn own_addresses(&self) -> Vec<String> {
        let settings = self.settings();
        if !settings.direct_addresses.is_empty() {
            return settings.direct_addresses;
        }
        let Some(listening) = self.inner.listener.lock().unwrap().address.clone() else {
            return Vec::new();
        };
        let Some(port) = listening
            .parse::<std::net::SocketAddr>()
            .ok()
            .map(|a| a.port())
        else {
            return Vec::new();
        };
        let bound_ip = listening
            .parse::<std::net::SocketAddr>()
            .ok()
            .map(|a| a.ip())
            .filter(|ip| !ip.is_unspecified());
        match bound_ip {
            Some(ip) => addresses::own_addresses([ip], port),
            None => addresses::own_addresses(addresses::interface_addresses(), port),
        }
    }

    /// Tell every admitted computer this one's addresses (only those not
    /// told yet in this session, unless `all`).
    pub async fn tell_all(&self, all: bool) {
        let admitted = self.admitted().await;
        for runtime in admitted {
            let first = {
                let mut peers = self.inner.peers.lock().unwrap();
                let peer = peers.entry(runtime.clone()).or_default();
                !std::mem::replace(&mut peer.told_this_session, true)
            };
            if all || first {
                self.tell(&runtime);
            }
        }
    }

    /// Send `runtime` this computer's addresses and whether it takes
    /// relayed frames.
    fn tell(&self, runtime: &str) {
        let links = self.connection().links();
        let addresses = if links {
            self.own_addresses()
        } else {
            Vec::new()
        };
        let relay = self.connection() != Connection::Direct;
        let statement = addresses::sign_addresses(
            &self.inner.signing_key,
            &self.inner.runtime_id,
            &addresses,
            relay,
        );
        let msg = TunnelMessage::TunnelDirectAddresses {
            request_id: format!("addr_{}", uuid::Uuid::new_v4().simple()),
            source_runtime_id: self.inner.runtime_id.clone(),
            recipient_runtime_id: runtime.to_string(),
            statement,
        };
        if self.inner.tx.try_send(msg).is_err() {
            warn!("could not tell addresses: the queue is full");
        }
    }

    async fn on_addresses(&self, source: &str, compact: &str) {
        let statement: DirectAddresses = match addresses::verify_addresses(compact, source) {
            Ok(s) => s,
            Err(e) => {
                warn!(runtime = %source, "ignoring addresses that do not verify: {e}");
                return;
            }
        };
        if !self.admitted().await.contains(source) {
            debug!(runtime = %source, "ignoring addresses from a computer this one does not link with");
            return;
        }
        let changed = {
            let mut file = self.inner.file.lock().unwrap();
            let changed = file.told(
                source,
                statement.addresses.clone(),
                statement.relay,
                statement.iat,
            );
            if changed {
                file.save(self.inner.links_path.as_ref());
            }
            changed
        };
        debug!(runtime = %source, addresses = ?statement.addresses, relay = statement.relay, "addresses told");
        // Answer once, so a computer that just started learns ours too.
        let first = {
            let mut peers = self.inner.peers.lock().unwrap();
            let peer = peers.entry(source.to_string()).or_default();
            peer.next_try = None;
            !std::mem::replace(&mut peer.told_this_session, true)
        };
        if first && self.connection().links() {
            self.tell(source);
        }
        if changed {
            self.recheck(source);
        }
        self.inner.maintain_kick.notify_one();
    }

    // -- Links ------------------------------------------------------------

    pub(crate) fn next_link_id(&self) -> u64 {
        self.inner.link_ids.fetch_add(1, Ordering::SeqCst)
    }

    /// A link said a hello this computer accepted: keep it, or the one
    /// already there (D3: one per pair, the one dialed by the lower id).
    pub(crate) fn register(&self, link: Arc<Link>) -> bool {
        let runtime = link.runtime.clone();
        let preferred_dialer_is_us = self.inner.runtime_id < runtime;
        let replaced = {
            let mut links = self.inner.links.lock().unwrap();
            if let Some(existing) = links.get(&runtime) {
                let existing_preferred = existing.dialed_by_us == preferred_dialer_is_us;
                let new_preferred = link.dialed_by_us == preferred_dialer_is_us;
                if existing_preferred && !new_preferred && !existing.is_closed() {
                    drop(links);
                    link.close("a link to this computer is already up");
                    return false;
                }
            }
            links.insert(runtime.clone(), Arc::clone(&link))
        };
        if let Some(old) = replaced {
            old.close("replaced by a newer link");
        }
        {
            let mut file = self.inner.file.lock().unwrap();
            file.worked(&runtime, &link.address);
            file.save(self.inner.links_path.as_ref());
        }
        {
            let mut peers = self.inner.peers.lock().unwrap();
            let peer = peers.entry(runtime.clone()).or_default();
            peer.last_error = None;
            peer.backoff = None;
            peer.next_try = None;
        }
        audit::link_up(&runtime, &link.address, link.dialed_by_us);
        // D6: a new link carries frames once it has answered heartbeats
        // for a while (at once in `direct`, where there is no other path).
        let this = self.clone();
        let warming = Arc::clone(&link);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(
                this.inner.warm_ms.load(Ordering::Relaxed),
            ))
            .await;
            if warming.answered_since(warming.up_at) && !warming.is_closed() {
                warming.warm.store(true, Ordering::SeqCst);
                this.recheck(&warming.runtime);
            }
        });
        self.recheck(&runtime);
        // Tell our addresses on the link, so a link made from `direct_peers`
        // learns them too.
        if self.connection().links() {
            self.tell(&runtime);
        }
        true
    }

    /// A link closed.
    pub(crate) fn unregister(&self, link: &Link, reason: &str) {
        let removed = {
            let mut links = self.inner.links.lock().unwrap();
            match links.get(&link.runtime) {
                Some(current) if current.id == link.id => links.remove(&link.runtime).is_some(),
                _ => false,
            }
        };
        audit::link_down(&link.runtime, &link.address, reason);
        if removed {
            self.recheck(&link.runtime);
            self.inner.maintain_kick.notify_one();
        }
    }

    /// A link that was in doubt answered (D6).
    pub(crate) fn link_answered(&self, runtime: &str) {
        self.recheck(runtime);
    }

    fn take_links(&self) -> Vec<Arc<Link>> {
        self.inner
            .links
            .lock()
            .unwrap()
            .drain()
            .map(|(_, l)| l)
            .collect()
    }

    fn note_error(&self, runtime: Option<&str>, address: &str, error: &str) {
        if let Some(runtime) = runtime {
            let mut peers = self.inner.peers.lock().unwrap();
            peers.entry(runtime.to_string()).or_default().last_error =
                Some(format!("{address}: {error}"));
        }
    }

    pub(crate) fn refused(&self, runtime: Option<&str>, address: &str, reason: &str) {
        audit::link_refused(runtime, address, reason);
        self.note_error(runtime, address, reason);
    }

    fn start_listener(&self) {
        let Some(identity) = &self.inner.identity else {
            self.inner.listener.lock().unwrap().error =
                Some("this engine could not make a certificate".into());
            return;
        };
        let config = match identity.server_config() {
            Ok(c) => c,
            Err(e) => {
                self.inner.listener.lock().unwrap().error = Some(e);
                return;
            }
        };
        let address = self
            .settings()
            .direct_listen
            .unwrap_or_else(|| DEFAULT_DIRECT_LISTEN.to_string());
        let cancel = self.inner.cancel.child_token();
        {
            let mut listener = self.inner.listener.lock().unwrap();
            if let Some(old) = listener.cancel.replace(cancel.clone()) {
                old.cancel();
            }
            listener.address = None;
            listener.error = None;
        }
        let this = self.clone();
        tokio::spawn(async move {
            match tokio::net::TcpListener::bind(&address).await {
                Ok(tcp) => {
                    let bound = tcp
                        .local_addr()
                        .map(|a| a.to_string())
                        .unwrap_or(address.clone());
                    info!(address = %bound, "listening for direct links");
                    this.inner.listener.lock().unwrap().address = Some(bound);
                    // What is told changed: tell it.
                    this.tell_all(true).await;
                    conn::accept_loop(this, tcp, config, cancel).await;
                }
                Err(e) => {
                    // D2: logged and shown; the engine still dials.
                    warn!(%address, "could not listen for direct links: {e}");
                    this.inner.listener.lock().unwrap().error =
                        Some(format!("could not listen on {address}: {e}"));
                }
            }
        });
    }

    fn stop_listener(&self) {
        let mut listener = self.inner.listener.lock().unwrap();
        if let Some(cancel) = listener.cancel.take() {
            cancel.cancel();
        }
        listener.address = None;
        listener.error = None;
    }

    /// Look for links to make and links to drop, every few seconds or
    /// when something changed.
    async fn maintain(self) {
        loop {
            tokio::select! {
                () = tokio::time::sleep(MAINTAIN_EVERY) => {}
                () = self.inner.maintain_kick.notified() => {}
                () = self.inner.cancel.cancelled() => return,
            }
            if !self.connection().links() {
                continue;
            }
            self.maintain_once().await;
        }
    }

    async fn maintain_once(&self) {
        let admitted = self.admitted().await;
        // D4: a computer taken off the list loses its link.
        let gone: Vec<Arc<Link>> = self
            .inner
            .links
            .lock()
            .unwrap()
            .values()
            .filter(|l| !admitted.contains(&l.runtime))
            .cloned()
            .collect();
        for link in gone {
            link.close("no longer a computer this one links with");
        }
        // D5: newly admitted computers are told.
        self.tell_all(false).await;
        let linked: HashSet<String> = self.inner.links.lock().unwrap().keys().cloned().collect();
        // Told and remembered addresses, per computer.
        let known: Vec<(String, Vec<String>)> = {
            let file = self.inner.file.lock().unwrap();
            admitted
                .iter()
                .filter(|r| !linked.contains(*r))
                .filter_map(|r| {
                    let record = file.peers.get(r)?;
                    let mut all = record.told.clone();
                    for worked in &record.worked {
                        if !all.contains(worked) {
                            all.push(worked.clone());
                        }
                    }
                    Some((r.clone(), all))
                })
                .collect()
        };
        for (runtime, told) in known {
            if !self.due(&runtime) {
                continue;
            }
            let targets = addresses::dialable(&told).await;
            if targets.is_empty() {
                continue;
            }
            self.dial_runtime(runtime, targets);
        }
        // `direct_peers`: whoever answers there, unless already linked.
        for entry in self.settings().direct_peers {
            let (learned, next_try) = self
                .inner
                .peer_addresses
                .lock()
                .unwrap()
                .get(&entry)
                .cloned()
                .map_or((None, None), |(r, t)| (Some(r), t));
            if learned.as_ref().is_some_and(|r| linked.contains(r)) {
                continue;
            }
            if next_try.is_some_and(|t| Instant::now() < t) {
                continue;
            }
            let targets = addresses::resolve_any(&entry).await;
            let this = self.clone();
            self.inner.peer_addresses.lock().unwrap().insert(
                entry.clone(),
                (
                    learned.unwrap_or_default(),
                    Some(Instant::now() + Duration::from_secs(10)),
                ),
            );
            tokio::spawn(async move {
                for target in targets {
                    match conn::dial(this.clone(), target, None).await {
                        Ok(runtime) => {
                            this.inner
                                .peer_addresses
                                .lock()
                                .unwrap()
                                .insert(entry.clone(), (runtime, None));
                            return;
                        }
                        Err(e) => debug!(address = %target, "direct peer did not link: {e}"),
                    }
                }
            });
        }
    }

    /// Whether `runtime` is due a dial (its backoff passed, none running).
    fn due(&self, runtime: &str) -> bool {
        let peers = self.inner.peers.lock().unwrap();
        peers
            .get(runtime)
            .is_none_or(|p| !p.dialing && p.next_try.is_none_or(|t| Instant::now() >= t))
    }

    fn dial_runtime(&self, runtime: String, targets: Vec<std::net::SocketAddr>) {
        self.inner
            .peers
            .lock()
            .unwrap()
            .entry(runtime.clone())
            .or_default()
            .dialing = true;
        let this = self.clone();
        tokio::spawn(async move {
            let mut error = None;
            for target in targets {
                match conn::dial(this.clone(), target, Some(&runtime)).await {
                    Ok(_) => {
                        error = None;
                        break;
                    }
                    Err(e) => error = Some(format!("{target}: {e}")),
                }
            }
            let mut peers = this.inner.peers.lock().unwrap();
            let peer = peers.entry(runtime.clone()).or_default();
            peer.dialing = false;
            if let Some(error) = error {
                debug!(runtime = %runtime, "could not link: {error}");
                peer.last_error = Some(error);
                let delay = peer
                    .backoff
                    .get_or_insert_with(ExponentialBackoff::default_tunnel)
                    .next();
                peer.next_try = Some(Instant::now() + delay);
            }
        });
    }

    /// `network_link_retry`: dial now, without waiting for the backoff.
    pub async fn retry(&self, runtime: Option<&str>) {
        {
            let mut peers = self.inner.peers.lock().unwrap();
            for (id, peer) in peers.iter_mut() {
                if runtime.is_none_or(|r| r == id) {
                    peer.next_try = None;
                    peer.backoff = None;
                }
            }
        }
        for (_, (_, next)) in self.inner.peer_addresses.lock().unwrap().iter_mut() {
            *next = None;
        }
        if self.connection().links() {
            self.maintain_once().await;
        }
    }

    // -- Network changes (D6) ----------------------------------------------

    async fn watch_network(self) {
        let mut watcher = match if_watch::tokio::IfWatcher::new() {
            Ok(w) => w,
            Err(e) => {
                warn!("cannot hear network changes; links rely on heartbeats: {e}");
                return;
            }
        };
        // The watcher first reports every address there is; that is not
        // a change.
        let started = Instant::now();
        loop {
            let event = tokio::select! {
                event = watcher.next() => event,
                () = self.inner.cancel.cancelled() => return,
            };
            let Some(event) = event else { return };
            if started.elapsed() < Duration::from_secs(2) {
                continue;
            }
            if let Err(e) = event {
                debug!("network watcher: {e}");
                continue;
            }
            // Settle: one change often comes as several events.
            let settle = tokio::time::sleep(SETTLE_FOR);
            tokio::pin!(settle);
            loop {
                tokio::select! {
                    () = &mut settle => break,
                    more = watcher.next() => if more.is_none() { return },
                }
            }
            self.network_changed().await;
        }
    }

    /// The OS said the network changed (D6).
    pub async fn network_changed(&self) {
        info!("the network changed");
        let at = Instant::now();
        let links: Vec<Arc<Link>> = self.inner.links.lock().unwrap().values().cloned().collect();
        for link in &links {
            link.doubt();
            self.recheck(&link.runtime);
            let this = self.clone();
            let link = Arc::clone(link);
            tokio::spawn(async move {
                tokio::time::sleep(DOUBT_FOR).await;
                if !link.answered_since(at) {
                    link.close("no answer after a network change");
                } else {
                    this.recheck(&link.runtime);
                }
            });
        }
        self.inner.hub_kick.notify_one();
        if self.connection().links() {
            self.tell_all(true).await;
            self.retry(None).await;
        }
    }

    // -- Settings and status (D9) ------------------------------------------

    /// Apply new `[network]` settings at once.
    pub async fn apply(&self, settings: NetworkConfig) {
        let before =
            std::mem::replace(&mut *self.inner.settings.write().unwrap(), settings.clone());
        let listener_changed = before.connection.links() != settings.connection.links()
            || before.direct_listen != settings.direct_listen;
        if listener_changed {
            self.stop_listener();
            if settings.connection.links() {
                self.start_listener();
            }
        }
        if !settings.connection.links() {
            for link in self.take_links() {
                link.close("this computer connects through PekoHub only");
            }
        }
        self.recheck_all();
        // Tell everyone what changed: addresses, and whether relayed
        // frames are taken. Leaving links behind, the last word is "no
        // addresses, relay taken", so no one keeps holding frames.
        if settings.connection.links() || before.connection.links() {
            self.tell_all(true).await;
        }
        self.inner.maintain_kick.notify_one();
    }

    /// What `network_status` answers.
    pub async fn status(&self) -> NetworkStatus {
        let settings = self.settings();
        let listening = {
            let listener = self.inner.listener.lock().unwrap();
            NetworkListening {
                address: listener.address.clone(),
                addresses: Vec::new(),
                error: listener.error.clone(),
            }
        };
        let listening = NetworkListening {
            addresses: if settings.connection.links() {
                self.own_addresses()
            } else {
                Vec::new()
            },
            ..listening
        };
        let mut runtimes: Vec<String> = self.admitted().await.into_iter().collect();
        for linked in self.inner.links.lock().unwrap().keys() {
            if !runtimes.contains(linked) {
                runtimes.push(linked.clone());
            }
        }
        runtimes.sort();
        let peers = runtimes
            .into_iter()
            .map(|runtime| {
                let link = self.inner.links.lock().unwrap().get(&runtime).cloned();
                let last_error = self
                    .inner
                    .peers
                    .lock()
                    .unwrap()
                    .get(&runtime)
                    .and_then(|p| p.last_error.clone());
                let (account, name) = self
                    .inner
                    .devices
                    .as_ref()
                    .and_then(|d| d.find_computer(&runtime))
                    .map_or((None, None), |(a, n)| (Some(a), n));
                NetworkPeer {
                    path: self.path_to(&runtime),
                    address: link.as_ref().map(|l| l.address.clone()),
                    since: link.as_ref().map(|l| l.since.clone()),
                    last_error,
                    account,
                    name,
                    runtime_id: runtime,
                }
            })
            .collect();
        NetworkStatus {
            listening,
            peers,
            ..NetworkStatus::idle(&settings)
        }
    }
}

fn plain(bytes: Vec<u8>) -> Outgoing {
    Outgoing {
        bytes,
        ty: String::new(),
        to: None,
        sealed: false,
    }
}

/// A frame's `type`, without opening it.
#[must_use]
pub fn frame_type(bytes: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Head {
        #[serde(rename = "type")]
        ty: String,
    }
    serde_json::from_slice::<Head>(bytes).ok().map(|h| h.ty)
}

fn request_id_of(bytes: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Head {
        #[serde(rename = "requestId")]
        request_id: Option<String>,
    }
    serde_json::from_slice::<Head>(bytes).ok()?.request_id
}

#[cfg(test)]
#[path = "router_tests.rs"]
mod tests;
