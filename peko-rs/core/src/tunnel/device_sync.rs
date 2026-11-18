//! Keeping each account's device list, and joining devices to it (ADR-111).
//!
//! [`DeviceSync`] asks PekoHub for the owner's list when the tunnel is
//! ready and takes every push after that. It also:
//! - writes the first link of a list nobody has written yet, and moves the
//!   per-computer files of ADR-108/109 onto it once (D7);
//! - appends what the owner adds and removes;
//! - runs both sides of a join by a code (D3);
//! - takes contacts' lists for the sealer;
//! - tells the phone gate which phones are on the list.
//!
//! What a list decides, `crate::security::device_list::DeviceLists::device_of`
//! answers; this module only keeps the lists current.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use tokio::sync::{oneshot, RwLock};
use tracing::{debug, info, warn};

use super::devices::{DeviceGate, DeviceRecord};
use super::protocol::TunnelMessage;
use super::seal::Sealer;
use super::TunnelHandle;
use crate::principal::manager::PrincipalManager;
use crate::security::computers::ComputerStatus;
use crate::security::device_list::{
    self, Accepted, Change, Device, DeviceKind, DeviceLists, Link, LinkPayload, ListError, Member,
    DEVICE_LIST_VERSION, UPGRADE_VIA,
};
use crate::security::join::{
    Cpace, JoinCode, JoinKey, JoinStep, Role, CODE_LIFETIME_SECS, MAX_STRIKES,
};

/// How long PekoHub has to take an appended link.
const APPEND_WAIT: Duration = if cfg!(test) {
    Duration::from_secs(3)
} else {
    Duration::from_secs(20)
};
/// How long a joining device waits for the whole exchange.
const JOIN_WAIT: Duration = if cfg!(test) {
    Duration::from_secs(3)
} else {
    Duration::from_mins(2)
};
/// How long the upgrade pass waits for a computer's key statement.
const KEY_WAIT: Duration = Duration::from_secs(5);

/// A code this device showed, and where its exchange stands (holder).
struct Offer {
    code: JoinCode,
    expires: Instant,
    strikes: u32,
    /// The joiner this exchange is with, once its share arrived.
    peer: Option<String>,
    key: Option<JoinKey>,
    device: Option<Device>,
    payload: Option<Vec<u8>>,
}

/// This device joining by a code (joiner).
struct Joining {
    code: JoinCode,
    account: String,
    cpace: Option<Cpace>,
    holder: Option<String>,
    key: Option<JoinKey>,
    device_json: String,
    /// The holder appended the link; waiting for the list that has it.
    appended: bool,
    done: Option<oneshot::Sender<Result<(), String>>>,
    started: Instant,
}

#[derive(Default)]
struct SyncState {
    appends: HashMap<String, oneshot::Sender<Result<u64, String>>>,
    offers: HashMap<String, Offer>,
    joining: Option<Joining>,
    /// Conversations a computer not on the list posted in, so it is asked
    /// to resend once it is added (ADR-109 D3's receipt).
    held: HashMap<String, BTreeSet<String>>,
}

/// What taking a list means for the tunnel: messages to send again now,
/// and computers to ask for what they sent while they were not on it.
#[derive(Debug, Default)]
pub struct ListOutcome {
    pub released: Vec<TunnelMessage>,
    pub resend: Vec<(String, Vec<String>)>,
}

/// A code shown to add a device.
#[derive(Debug, Clone)]
pub struct ShownCode {
    /// `ABCD-EFGH-JKMN`.
    pub code: String,
    /// What the QR code holds.
    pub qr: String,
    /// RFC 3339.
    pub expires_at: String,
}

/// Where this computer stands with its account's list, for the Devices
/// page.
#[derive(Debug, Clone)]
pub struct DevicesStatus {
    /// `user:hub:<id>`, when registered.
    pub account: Option<String>,
    pub me: String,
    /// Whether this computer is on the list.
    pub on_list: bool,
    /// The devices on the list (or on the one PekoHub shows, while this
    /// computer is not on it).
    pub devices: Vec<Member>,
    /// The account's code (ADR-112), once this computer holds its list.
    pub code: Option<String>,
}

/// See the module docs.
pub struct DeviceSync {
    me: String,
    signing: Arc<SigningKey>,
    agreement: String,
    name: String,
    manager: Arc<PrincipalManager>,
    gate: Arc<DeviceGate>,
    slot: Arc<RwLock<Option<TunnelHandle>>>,
    sealer: Mutex<Option<Arc<Sealer>>>,
    hub_url: Mutex<Option<String>>,
    state: Mutex<SyncState>,
    /// One append at a time: each names the head it follows.
    append_lock: tokio::sync::Mutex<()>,
    /// This sync, for the tasks it spawns.
    weak: std::sync::Weak<Self>,
}

impl std::fmt::Debug for DeviceSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceSync")
            .field("me", &self.me)
            .finish_non_exhaustive()
    }
}

fn err_text(e: &ListError) -> String {
    e.to_string()
}

impl DeviceSync {
    /// The sync for this computer (`signing`'s runtime, called `name`).
    #[must_use]
    pub fn new(
        signing: Arc<SigningKey>,
        name: String,
        manager: Arc<PrincipalManager>,
        gate: Arc<DeviceGate>,
        slot: Arc<RwLock<Option<TunnelHandle>>>,
    ) -> Arc<Self> {
        let me = device_list::did_of(&signing);
        let agreement = super::seal::AgreementKey::derive(&signing)
            .public()
            .to_string();
        Arc::new_cyclic(|weak| Self {
            me,
            signing,
            agreement,
            name,
            manager,
            gate,
            slot,
            sealer: Mutex::new(None),
            hub_url: Mutex::new(None),
            state: Mutex::new(SyncState::default()),
            append_lock: tokio::sync::Mutex::new(()),
            weak: weak.clone(),
        })
    }

    /// Give the phone gate the phones on the list this computer holds
    /// (at start, before the tunnel connects).
    pub async fn load_phones(&self) {
        let own = self.own().await;
        self.refresh_phones_for(own.as_deref());
    }

    /// The sealer of the tunnel now running, and the Hub it talks to.
    pub fn attach(&self, sealer: Arc<Sealer>, hub_url: String) {
        *self.sealer.lock().unwrap() = Some(sealer);
        self.set_hub(hub_url);
    }

    /// The Hub a shown code names.
    pub fn set_hub(&self, hub_url: String) {
        *self.hub_url.lock().unwrap() = Some(hub_url);
    }

    fn lists(&self) -> Arc<DeviceLists> {
        self.manager.device_lists()
    }

    /// This computer's runtime id.
    #[must_use]
    pub fn me(&self) -> &str {
        &self.me
    }

    async fn own(&self) -> Option<String> {
        self.manager
            .local_account()
            .await
            .map(|a| format!("user:hub:{a}"))
    }

    async fn send(&self, msg: TunnelMessage) -> bool {
        let handle = self.slot.read().await.clone();
        match handle {
            Some(handle) => handle.send(msg).is_ok(),
            None => false,
        }
    }

    /// The computers the owner approved here before ADR-111 (D7).
    fn approved_before(&self, own: &str) -> HashSet<String> {
        let bare = own.strip_prefix("user:hub:").unwrap_or(own);
        self.manager
            .own_computers()
            .list(bare)
            .into_iter()
            .filter(|c| c.status == ComputerStatus::Approved)
            .map(|c| c.runtime)
            .collect()
    }

    /// The tunnel is ready: ask for the owner's list.
    pub async fn on_ready(&self) {
        let Some(own) = self.own().await else {
            return;
        };
        self.refresh_phones_for(Some(&own));
        self.send(TunnelMessage::DeviceListRequest {
            request_id: format!("devices_{}", uuid::Uuid::new_v4().simple()),
            account: own,
            after: None,
        })
        .await;
    }

    /// Whether a computer may speak for this computer's own account
    /// (ADR-111 D5, replacing ADR-109's approvals): it must be on the list.
    /// A post from one that is not is remembered, so the computer is asked
    /// to resend it once it is added.
    pub async fn admits(&self, source: &str, channel: Option<&str>) -> bool {
        let Some(own) = self.own().await else {
            return true;
        };
        if source == self.me || self.lists().device_of(&own, source) == Some(DeviceKind::Computer) {
            return true;
        }
        info!(
            source,
            "a computer not on this account's device list speaks for it; not acted on"
        );
        if let Some(channel) = channel {
            self.state
                .lock()
                .unwrap()
                .held
                .entry(source.to_string())
                .or_default()
                .insert(channel.to_string());
        }
        false
    }

    /// A list PekoHub sent (an answer, or a push after an append).
    pub async fn on_list(&self, account: String, from: u64, links: Vec<Link>) -> ListOutcome {
        let own = self.own().await;
        let lists = self.lists();
        let mine = own.as_deref() == Some(account.as_str());
        let result = match (from, links.is_empty()) {
            (1.., true) => Ok(Accepted::Unchanged),
            (0, true) => {
                if mine && lists.list(&account).is_none() {
                    // Nobody wrote this account's list yet: this computer
                    // is its first device (D7).
                    self.spawn_first_link(account.clone());
                }
                Ok(Accepted::Unchanged)
            }
            (0, false) if mine => {
                let approved = self.approved_before(&account);
                lists.accept_own(&account, links, &self.me, &approved)
            }
            (0, false) => lists.accept(&account, links),
            (_, false) => match lists.accept_after(&account, links) {
                Err(ListError::Empty) => {
                    // Not held yet: ask for all of it.
                    self.send(TunnelMessage::DeviceListRequest {
                        request_id: format!("devices_{}", uuid::Uuid::new_v4().simple()),
                        account: account.clone(),
                        after: None,
                    })
                    .await;
                    Ok(Accepted::Unchanged)
                }
                other => other,
            },
        };
        let mut outcome = ListOutcome::default();
        match &result {
            Ok(Accepted::Extended { added, .. }) if mine => {
                let mut state = self.state.lock().unwrap();
                for id in added {
                    if let Some(channels) = state.held.remove(id) {
                        outcome
                            .resend
                            .push((id.clone(), channels.into_iter().collect()));
                    }
                }
            }
            Ok(_) => {}
            Err(e) => warn!(%account, "device list not taken: {}", err_text(e)),
        }
        if mine {
            self.refresh_phones_for(Some(&account));
            if matches!(result, Ok(Accepted::Pinned | Accepted::Extended { .. })) {
                self.finish_join_if_listed(&account);
                if !lists.upgraded(&account) {
                    self.spawn_move_legacy(account.clone());
                }
            }
        }
        let sealer = self.sealer.lock().unwrap().clone();
        if let Some(sealer) = sealer {
            if lists.list(&account).is_some() {
                outcome.released = sealer.release_account(&account);
            } else {
                let dropped = sealer.release_account(&account).len();
                if dropped > 0 {
                    warn!(%account, dropped, "no device list for this account; not sent");
                }
            }
        }
        outcome
    }

    /// PekoHub's answer to an append.
    pub fn on_appended(&self, request_id: &str, ok: bool, error: Option<String>, seq: Option<u64>) {
        let waiter = self.state.lock().unwrap().appends.remove(request_id);
        if let Some(waiter) = waiter {
            let _ = waiter.send(if ok {
                Ok(seq.unwrap_or_default())
            } else {
                Err(error.unwrap_or_else(|| "PekoHub did not take the change".into()))
            });
        }
    }

    fn refresh_phones_for(&self, own: Option<&str>) {
        let Some(own) = own else { return };
        let phones: Vec<DeviceRecord> = self
            .lists()
            .list(own)
            .map(|l| {
                l.members()
                    .filter(|m| m.device.kind == DeviceKind::Phone)
                    .filter_map(DeviceRecord::from_member)
                    .collect()
            })
            .unwrap_or_default();
        for gone in self.gate.set_phones(phones) {
            info!(device = %gone.device_id, "a phone left the device list");
        }
    }

    // ── Appending ───────────────────────────────────────────────────────

    /// Send `link` and wait for PekoHub to take it, then take it here.
    async fn append_link(&self, own: &str, link: Link, genesis: bool) -> Result<u64, String> {
        let request_id = format!("append_{}", uuid::Uuid::new_v4().simple());
        let (tx, rx) = oneshot::channel();
        self.state
            .lock()
            .unwrap()
            .appends
            .insert(request_id.clone(), tx);
        if !self
            .send(TunnelMessage::DeviceListAppend {
                request_id: request_id.clone(),
                account: own.to_string(),
                link: link.clone(),
            })
            .await
        {
            self.state.lock().unwrap().appends.remove(&request_id);
            return Err("not connected to PekoHub".into());
        }
        let seq = match tokio::time::timeout(APPEND_WAIT, rx).await {
            Ok(Ok(answer)) => answer?,
            _ => {
                self.state.lock().unwrap().appends.remove(&request_id);
                return Err("PekoHub did not answer".into());
            }
        };
        let lists = self.lists();
        let taken = if genesis {
            let approved = self.approved_before(own);
            lists.accept_own(own, vec![link], &self.me, &approved)
        } else {
            lists.accept_after(own, vec![link])
        };
        match taken {
            // A push of the same link may have arrived first.
            Ok(_) | Err(ListError::Invalid { .. }) => {}
            Err(e) => return Err(err_text(&e)),
        }
        self.refresh_phones_for(Some(own));
        Ok(seq)
    }

    /// Append `change` to the owner's list, as this computer.
    async fn append(&self, change: Change, via: Option<&str>) -> Result<u64, String> {
        let own = self
            .own()
            .await
            .ok_or("[not_registered] Sign this computer in to PekoHub first")?;
        let _guard = self.append_lock.lock().await;
        let list = self
            .lists()
            .list(&own)
            .filter(|l| l.member(&self.me).is_some())
            .ok_or("This computer is not on your account's devices")?;
        let mut payload = list.next_payload(change);
        payload.via = via.map(str::to_owned);
        let bytes = device_list::payload_bytes(&payload);
        let link = Link {
            by: self.me.clone(),
            signed: device_list::sign_payload_bytes(&self.signing, &bytes),
            joiner: None,
        };
        self.append_link(&own, link, false).await
    }

    fn spawn_first_link(&self, own: String) {
        let Some(this) = self.self_arc() else { return };
        tokio::spawn(async move {
            let _guard = this.append_lock.lock().await;
            if this.lists().list(&own).is_some() {
                return;
            }
            let link = device_list::genesis(
                &this.signing,
                &own,
                DeviceKind::Computer,
                Some(&this.name),
                &this.agreement,
            );
            match this.append_link(&own, link, true).await {
                Ok(_) => info!(%own, "started this account's device list"),
                Err(e) => warn!(%own, "could not start the device list: {e}"),
            }
        });
    }

    /// D7: move the computers approved and the phones paired here before
    /// ADR-111 onto the list, once.
    fn spawn_move_legacy(&self, own: String) {
        let Some(this) = self.self_arc() else { return };
        tokio::spawn(async move { this.move_legacy(&own).await });
    }

    async fn move_legacy(&self, own: &str) {
        let lists = self.lists();
        if lists.upgraded(own) || lists.device_of(own, &self.me).is_none() {
            return;
        }
        let mut adds: Vec<Device> = Vec::new();
        for runtime in self.approved_before(own) {
            if lists.device_of(own, &runtime).is_some() || runtime == self.me {
                continue;
            }
            match self.key_of(&runtime).await {
                Some(agreement_key) => adds.push(Device {
                    id: runtime,
                    kind: DeviceKind::Computer,
                    name: None,
                    agreement_key,
                    hub_device_id: None,
                }),
                None => {
                    warn!(%runtime, "no key statement for an approved computer; it joins by a code instead")
                }
            }
        }
        for phone in self.gate.legacy() {
            let Some(id) = did_of_raw(&phone.signing_key) else {
                continue;
            };
            if lists.device_of(own, &id).is_some() {
                continue;
            }
            adds.push(Device {
                id,
                kind: DeviceKind::Phone,
                name: Some(phone.name.clone()).filter(|n| !n.is_empty()),
                agreement_key: phone.agreement_key.clone(),
                hub_device_id: Some(phone.device_id.clone()),
            });
        }
        for device in adds {
            let id = device.id.clone();
            if let Err(e) = self.append(Change::Add { device }, Some(UPGRADE_VIA)).await {
                warn!(%id, "could not move a device onto the list: {e}");
                return;
            }
        }
        lists.mark_upgraded(own);
    }

    /// A computer's agreement key, from its statement (asked for when not
    /// known yet).
    async fn key_of(&self, runtime: &str) -> Option<String> {
        let sealer = self.sealer.lock().unwrap().clone()?;
        if let Some(key) = sealer.known_key_of(runtime) {
            return Some(key);
        }
        self.send(TunnelMessage::RuntimeKeysRequest {
            request_id: format!("keys_{}", uuid::Uuid::new_v4().simple()),
            runtime_ids: vec![runtime.to_string()],
            account: None,
        })
        .await;
        let deadline = Instant::now() + KEY_WAIT;
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Some(key) = sealer.known_key_of(runtime) {
                return Some(key);
            }
        }
        None
    }

    // ── What the owner does ─────────────────────────────────────────────

    /// Where this computer stands, for the Devices page.
    pub async fn status(&self) -> DevicesStatus {
        let own = self.own().await;
        let lists = self.lists();
        let held = own.as_deref().and_then(|a| lists.list(a));
        let on_list = held.as_ref().is_some_and(|l| l.member(&self.me).is_some());
        let code = held.as_ref().map(|l| {
            use crate::security::identity_code::{code_of, Identity};
            code_of(Identity::Person(l.genesis()))
        });
        let shown = held.or_else(|| lists.unanchored());
        DevicesStatus {
            account: own,
            me: self.me.clone(),
            code,
            on_list,
            devices: shown
                .map(|l| l.members().cloned().collect())
                .unwrap_or_default(),
        }
    }

    /// Show a code that adds a device (D3).
    ///
    /// # Errors
    ///
    /// This computer is not registered, or not on its account's list.
    pub async fn show_code(&self) -> Result<ShownCode, String> {
        let own = self
            .own()
            .await
            .ok_or("[not_registered] Sign this computer in to PekoHub first")?;
        if self.lists().device_of(&own, &self.me).is_none() {
            return Err("[not_on_list] This computer is not on your account's devices yet".into());
        }
        let hub = self
            .hub_url
            .lock()
            .unwrap()
            .clone()
            .ok_or("[offline] Not connected to PekoHub")?;
        let code = JoinCode::generate();
        let expires_at =
            (chrono::Utc::now() + chrono::Duration::seconds(CODE_LIFETIME_SECS)).to_rfc3339();
        let shown = ShownCode {
            code: code.display(),
            qr: code.qr_text(&hub),
            expires_at,
        };
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        state.offers.retain(|_, o| o.expires > now);
        state.offers.insert(
            code.nameplate().to_string(),
            Offer {
                code,
                expires: now + Duration::from_secs(CODE_LIFETIME_SECS.unsigned_abs()),
                strikes: 0,
                peer: None,
                key: None,
                device: None,
                payload: None,
            },
        );
        Ok(shown)
    }

    /// Join this computer to its account's list with a code shown on a
    /// device already on it (D3).
    ///
    /// # Errors
    ///
    /// Not registered, already on the list, a code that is not one, or the
    /// exchange failed (the sentence says why).
    pub async fn join(&self, typed: &str) -> Result<(), String> {
        let own = self
            .own()
            .await
            .ok_or("[not_registered] Sign this computer in to PekoHub first")?;
        if self.lists().device_of(&own, &self.me).is_some() {
            return Err("This computer is already on your account's devices".into());
        }
        let code = JoinCode::parse(typed).map_err(|e| e.to_string())?;
        let device_json = serde_json::to_string(&Device {
            id: self.me.clone(),
            kind: DeviceKind::Computer,
            name: Some(self.name.clone()),
            agreement_key: self.agreement.clone(),
            hub_device_id: None,
        })
        .expect("a device serializes");
        let (cpace, share) = Cpace::start(Role::Joiner, &code, &own);
        let (tx, rx) = oneshot::channel();
        let nameplate = code.nameplate().to_string();
        self.state.lock().unwrap().joining = Some(Joining {
            code,
            account: own.clone(),
            cpace: Some(cpace),
            holder: None,
            key: None,
            device_json,
            appended: false,
            done: Some(tx),
            started: Instant::now(),
        });
        if !self
            .send(TunnelMessage::DeviceJoin {
                account: own,
                nameplate,
                from: None,
                to: None,
                body: JoinStep::Hello { share },
            })
            .await
        {
            self.state.lock().unwrap().joining = None;
            return Err("[offline] Not connected to PekoHub".into());
        }
        let outcome = match tokio::time::timeout(JOIN_WAIT, rx).await {
            Ok(Ok(outcome)) => outcome,
            _ => Err(
                "No device answered. Check the code, and that the device showing it is online."
                    .into(),
            ),
        };
        self.state.lock().unwrap().joining = None;
        outcome
    }

    /// Remove a device from the owner's list (D4), by its id or, for a
    /// phone, its Hub id.
    ///
    /// # Errors
    ///
    /// No such device on the list, or PekoHub did not take the change.
    pub async fn remove(&self, id: &str) -> Result<(), String> {
        let own = self
            .own()
            .await
            .ok_or("[not_registered] Sign this computer in to PekoHub first")?;
        let list = self
            .lists()
            .list(&own)
            .ok_or("This computer holds no device list for your account")?;
        let device_id = list
            .member(id)
            .or_else(|| list.phone(id))
            .map(|m| m.device.id.clone())
            .ok_or("No such device on your account")?;
        self.append(Change::Remove { device_id }, None)
            .await
            .map(|_| ())
    }

    // ── Joining, step by step ───────────────────────────────────────────

    /// One step of a join PekoHub relayed.
    pub async fn on_join(
        &self,
        account: String,
        nameplate: String,
        from: Option<String>,
        to: Option<String>,
        body: JoinStep,
    ) {
        let Some(from) = from else { return };
        if to.as_deref().is_some_and(|t| t != self.me) || from == self.me {
            return;
        }
        if self.own().await.as_deref() != Some(account.as_str()) {
            return;
        }
        let holding = self.state.lock().unwrap().offers.contains_key(&nameplate);
        if holding {
            self.as_holder(&account, &nameplate, &from, body).await;
        } else if to.is_some() {
            // A joiner hears only replies to it, never another joiner's hello.
            self.as_joiner(&account, &nameplate, &from, body).await;
        }
    }

    async fn reply(&self, account: &str, nameplate: &str, to: &str, body: JoinStep) {
        self.send(TunnelMessage::DeviceJoin {
            account: account.to_string(),
            nameplate: nameplate.to_string(),
            from: None,
            to: Some(to.to_string()),
            body,
        })
        .await;
    }

    /// A wrong step costs the code a strike; the third ends it.
    fn strike(&self, nameplate: &str) {
        let mut state = self.state.lock().unwrap();
        let Some(offer) = state.offers.get_mut(nameplate) else {
            return;
        };
        offer.strikes += 1;
        offer.peer = None;
        offer.key = None;
        offer.device = None;
        offer.payload = None;
        if offer.strikes >= MAX_STRIKES {
            state.offers.remove(nameplate);
            warn!(nameplate, "a join code ended after wrong answers");
        }
    }

    async fn as_holder(&self, account: &str, nameplate: &str, from: &str, body: JoinStep) {
        enum Next {
            Reply(JoinStep),
            Append(Link),
            Strike(String),
            Nothing,
        }
        let lists = self.lists();
        let next = {
            let mut state = self.state.lock().unwrap();
            let Some(offer) = state.offers.get_mut(nameplate) else {
                return;
            };
            if offer.expires <= Instant::now() {
                state.offers.remove(nameplate);
                Next::Reply(JoinStep::Failed {
                    message: "That code has expired. Show a new one.".into(),
                })
            } else if offer.peer.as_deref().is_some_and(|p| p != from) {
                Next::Reply(JoinStep::Failed {
                    message: "Another device is using this code. Show a new one.".into(),
                })
            } else {
                match body {
                    JoinStep::Hello { share } if offer.peer.is_none() => {
                        let (cpace, mine) = Cpace::start(Role::Holder, &offer.code, account);
                        match cpace.finish(&share) {
                            Ok(key) => {
                                offer.peer = Some(from.to_string());
                                offer.key = Some(key);
                                Next::Reply(JoinStep::Hello { share: mine })
                            }
                            Err(e) => Next::Strike(e.to_string()),
                        }
                    }
                    JoinStep::Device { device, mac } if offer.peer.is_some() => {
                        let key = offer.key.clone().expect("a peer has a key");
                        match key.check("device", device.as_bytes(), &mac) {
                            Err(e) => Next::Strike(e.to_string()),
                            Ok(()) => match self.holder_payload(account, from, &device, &lists) {
                                Ok((device, bytes)) => {
                                    let mac = key.mac("payload", &bytes);
                                    let payload = String::from_utf8(bytes.clone())
                                        .expect("a payload is JSON text");
                                    offer.device = Some(device);
                                    offer.payload = Some(bytes);
                                    Next::Reply(JoinStep::Payload { payload, mac })
                                }
                                Err(message) => Next::Reply(JoinStep::Failed { message }),
                            },
                        }
                    }
                    JoinStep::Signed { joiner } if offer.payload.is_some() => {
                        let bytes = offer.payload.clone().expect("checked");
                        Next::Append(Link {
                            by: self.me.clone(),
                            signed: device_list::sign_payload_bytes(&self.signing, &bytes),
                            joiner: Some(joiner),
                        })
                    }
                    _ => Next::Nothing,
                }
            }
        };
        match next {
            Next::Reply(step) => self.reply(account, nameplate, from, step).await,
            Next::Strike(message) => {
                self.strike(nameplate);
                self.reply(account, nameplate, from, JoinStep::Failed { message })
                    .await;
            }
            Next::Append(link) => {
                let _guard = self.append_lock.lock().await;
                let outcome = self.append_link(account, link, false).await;
                drop(_guard);
                let step = match outcome {
                    Ok(seq) => {
                        let device = self
                            .state
                            .lock()
                            .unwrap()
                            .offers
                            .remove(nameplate)
                            .and_then(|o| o.device);
                        info!(
                            device = device.as_ref().map(|d| d.id.as_str()).unwrap_or(""),
                            "a device joined this account"
                        );
                        JoinStep::Done { seq }
                    }
                    Err(message) => {
                        self.state.lock().unwrap().offers.remove(nameplate);
                        JoinStep::Failed { message }
                    }
                };
                self.reply(account, nameplate, from, step).await;
            }
            Next::Nothing => debug!(nameplate, "a join step out of order; ignored"),
        }
    }

    /// The payload of the link that adds the joiner, after checking that it
    /// describes the device PekoHub says sent it.
    fn holder_payload(
        &self,
        account: &str,
        from: &str,
        device_json: &str,
        lists: &DeviceLists,
    ) -> Result<(Device, Vec<u8>), String> {
        let device: Device = serde_json::from_str(device_json)
            .map_err(|_| "The device did not describe itself.".to_string())?;
        let names_sender = match device.kind {
            DeviceKind::Computer => device.id == from,
            DeviceKind::Phone => device
                .hub_device_id
                .as_deref()
                .is_some_and(|id| from == format!("device:{id}")),
        };
        if !names_sender {
            return Err("The device is not the one that asked.".into());
        }
        let list = lists
            .list(account)
            .ok_or("This device holds no list for the account.")?;
        if list.member(&device.id).is_some() {
            return Err("That device is already on your account.".into());
        }
        let payload = list.next_payload(Change::Add {
            device: device.clone(),
        });
        Ok((device, device_list::payload_bytes(&payload)))
    }

    async fn as_joiner(&self, account: &str, nameplate: &str, from: &str, body: JoinStep) {
        let reply = {
            let mut state = self.state.lock().unwrap();
            let Some(joining) = state.joining.as_mut() else {
                return;
            };
            if joining.code.nameplate() != nameplate || joining.account != account {
                return;
            }
            if joining.holder.as_deref().is_some_and(|h| h != from) {
                return;
            }
            match body {
                JoinStep::Hello { share } if joining.holder.is_none() => {
                    let Some(cpace) = joining.cpace.take() else {
                        return;
                    };
                    match cpace.finish(&share) {
                        Ok(key) => {
                            let mac = key.mac("device", joining.device_json.as_bytes());
                            joining.holder = Some(from.to_string());
                            joining.key = Some(key);
                            Some(JoinStep::Device {
                                device: joining.device_json.clone(),
                                mac,
                            })
                        }
                        Err(e) => {
                            Self::finish(joining, Err(e.to_string()));
                            None
                        }
                    }
                }
                JoinStep::Payload { payload, mac } => {
                    let Some(key) = joining.key.clone() else {
                        return;
                    };
                    match key
                        .check("payload", payload.as_bytes(), &mac)
                        .map_err(|e| e.to_string())
                        .and_then(|()| self.check_payload(joining, &payload))
                    {
                        Ok(()) => Some(JoinStep::Signed {
                            joiner: device_list::sign_payload_bytes(
                                &self.signing,
                                payload.as_bytes(),
                            ),
                        }),
                        Err(message) => {
                            Self::finish(joining, Err(message.clone()));
                            Some(JoinStep::Failed { message })
                        }
                    }
                }
                JoinStep::Done { .. } => {
                    joining.appended = true;
                    None
                }
                JoinStep::Failed { message } => {
                    Self::finish(joining, Err(message));
                    None
                }
                _ => None,
            }
        };
        if let Some(step) = reply {
            self.reply(account, nameplate, from, step).await;
            return;
        }
        let appended = self
            .state
            .lock()
            .unwrap()
            .joining
            .as_ref()
            .is_some_and(|j| j.appended);
        if appended {
            // The list that has this computer on it.
            self.send(TunnelMessage::DeviceListRequest {
                request_id: format!("devices_{}", uuid::Uuid::new_v4().simple()),
                account: account.to_string(),
                after: None,
            })
            .await;
        }
    }

    /// The payload the holder sent adds exactly this device to this account.
    fn check_payload(&self, joining: &Joining, payload: &str) -> Result<(), String> {
        let parsed: LinkPayload = serde_json::from_str(payload)
            .map_err(|_| "The other device sent something that is not a list entry.".to_string())?;
        let mine: Device =
            serde_json::from_str(&joining.device_json).expect("our own description parses");
        let adds_me = matches!(&parsed.change, Change::Add { device } if *device == mine);
        if parsed.v != DEVICE_LIST_VERSION
            || parsed.account != joining.account
            || !adds_me
            || parsed.via.is_some()
        {
            return Err("The other device offered a different change.".into());
        }
        Ok(())
    }

    fn finish(joining: &mut Joining, outcome: Result<(), String>) {
        if let Some(done) = joining.done.take() {
            let _ = done.send(outcome);
        }
    }

    fn finish_join_if_listed(&self, own: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(joining) = state.joining.as_mut() {
            if joining.account == own && self.lists().device_of(own, &self.me).is_some() {
                info!(
                    secs = joining.started.elapsed().as_secs(),
                    "this computer joined its account's devices"
                );
                Self::finish(joining, Ok(()));
            }
        }
    }

    // ── Plumbing ────────────────────────────────────────────────────────

    fn self_arc(&self) -> Option<Arc<Self>> {
        self.weak.upgrade()
    }
}

/// The `did:key` of a raw ed25519 public key (base64url).
fn did_of_raw(key: &str) -> Option<String> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let bytes: [u8; 32] = URL_SAFE_NO_PAD.decode(key).ok()?.try_into().ok()?;
    let key = ed25519_dalek::VerifyingKey::from_bytes(&bytes).ok()?;
    Some(super::verifying_key_to_did_key(&key))
}
