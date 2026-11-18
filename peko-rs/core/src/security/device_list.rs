//! Each account's devices, as one signed list (ADR-111).
//!
//! A list is a chain of links. Each link is a compact JWS, made by a device
//! already on the list, that adds or removes one device. The first link
//! (genesis) adds the first device and is signed by it. An `add` also
//! carries the new device's own JWS over the same payload, so nobody
//! enrolls a key they do not hold (D1). PekoHub keeps the chain and serves
//! it. It can withhold links, but it cannot make one (D2).
//!
//! [`DeviceList`] is one verified chain. [`DeviceLists`] is what this
//! computer holds: one chain per account, pinned by its genesis the first
//! time it is seen. Later it only grows, and a shorter, diverging or reset
//! chain is not taken. [`DeviceLists::device_of`] is the one question every
//! trust check asks (D5).
//!
//! On disk: `<runtime>/device_lists.json`, `0600`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::tunnel::tunnel_channel_signature::{jws_sign, jws_verify};
use crate::tunnel::{did_key_to_verifying_key, verifying_key_to_did_key};

/// Version tag of a link's payload.
pub const DEVICE_LIST_VERSION: &str = "peko-device-list/1";
/// `via` of an `add` written when moving from the per-computer files
/// (D7): the old files already hold the owner's decision, so the new
/// device's own signature is not required.
pub const UPGRADE_VIA: &str = "upgrade";
/// Most devices one account may have at once.
pub const MAX_DEVICES: usize = 64;
/// Most links one chain may have.
pub const MAX_LINKS: usize = 4096;
/// Longest device name, in characters.
const MAX_NAME_CHARS: usize = 64;

/// What kind of device it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Computer,
    Phone,
}

/// A device as the list records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Its signing key as a `did:key`: a computer's runtime id, or a
    /// phone's device key.
    pub id: String,
    pub kind: DeviceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Its X25519 agreement key, base64url of 32 bytes (ADR-110 D2).
    pub agreement_key: String,
    /// A phone's id at PekoHub (`dev_…`), which the Hub routes its stream
    /// by (ADR-108 D4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub_device_id: Option<String>,
}

/// What a link does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    Add { device: Device },
    Remove { device_id: String },
}

/// The signed payload of a link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkPayload {
    pub v: String,
    /// `user:hub:<id>`.
    pub account: String,
    pub seq: u64,
    /// Hash of the previous link ([`Link::hash`]); absent at `seq` 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
    #[serde(flatten)]
    pub change: Change,
    /// [`UPGRADE_VIA`] for an `add` written by the upgrade pass (D7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    pub iat: i64,
}

/// One link as it travels and is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// The device that signed it (`did:key`).
    pub by: String,
    /// Compact JWS by `by` over the [`LinkPayload`].
    pub signed: String,
    /// For an `add`: the new device's compact JWS over the same payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joiner: Option<String>,
}

impl Link {
    /// What the next link names as `prev`: base64url of SHA-256 over the
    /// signed JWS, which holds the payload and its signature.
    #[must_use]
    pub fn hash(&self) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(self.signed.as_bytes()))
    }
}

/// Where a chain ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub seq: u64,
    pub hash: String,
}

/// Why a chain, or a link of it, is not taken.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ListError {
    #[error("link {seq}: {reason}")]
    Invalid { seq: u64, reason: String },
    #[error("an empty device list")]
    Empty,
    #[error("this device list does not extend the one held for {account}")]
    Fork { account: String },
    #[error("{account}'s devices were reset: the list starts from another first device")]
    Reset { account: String },
}

fn invalid(seq: u64, reason: impl Into<String>) -> ListError {
    ListError::Invalid {
        seq,
        reason: reason.into(),
    }
}

/// A device on the list, with where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub device: Device,
    /// The link that added it.
    pub added_seq: u64,
    /// The device that added it (itself, for the first one).
    pub added_by: String,
    /// Whether the new device signed the link that added it (D1), as a
    /// device that joined by a code does.
    pub cosigned: bool,
    /// When it was added (the link's `iat`, unix seconds).
    pub added_at: i64,
}

/// One account's devices: a chain whose every link was checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceList {
    account: String,
    links: Vec<Link>,
    hashes: Vec<String>,
    members: BTreeMap<String, Member>,
}

impl DeviceList {
    /// Check `links` as `account`'s chain from its first link.
    ///
    /// # Errors
    ///
    /// The chain is empty, or a link is not what the chain allows there.
    pub fn verify(account: &str, links: Vec<Link>) -> Result<Self, ListError> {
        if links.is_empty() {
            return Err(ListError::Empty);
        }
        let mut list = Self {
            account: account.to_string(),
            links: Vec::new(),
            hashes: Vec::new(),
            members: BTreeMap::new(),
        };
        for link in links {
            list.apply(link)?;
        }
        Ok(list)
    }

    /// This chain followed by `more`, checked.
    ///
    /// # Errors
    ///
    /// A link of `more` is not what the chain allows there.
    pub fn extended(&self, more: Vec<Link>) -> Result<Self, ListError> {
        let mut next = self.clone();
        for link in more {
            next.apply(link)?;
        }
        Ok(next)
    }

    fn apply(&mut self, link: Link) -> Result<(), ListError> {
        let seq = self.links.len() as u64;
        if self.links.len() >= MAX_LINKS {
            return Err(invalid(
                seq,
                format!("a chain has at most {MAX_LINKS} links"),
            ));
        }
        let signer = did_key_to_verifying_key(&link.by)
            .map_err(|e| invalid(seq, format!("signer is not a did:key: {e}")))?;
        let (segment, bytes) = jws_verify(&signer, &link.signed)
            .map_err(|e| invalid(seq, format!("signature: {e}")))?;
        let payload: LinkPayload =
            serde_json::from_slice(&bytes).map_err(|e| invalid(seq, format!("payload: {e}")))?;
        if payload.v != DEVICE_LIST_VERSION {
            return Err(invalid(
                seq,
                format!("version must be {DEVICE_LIST_VERSION:?}"),
            ));
        }
        if payload.account != self.account {
            return Err(invalid(seq, "names another account"));
        }
        if payload.seq != seq {
            return Err(invalid(seq, format!("says it is link {}", payload.seq)));
        }
        if payload.prev.as_ref() != self.hashes.last() {
            return Err(invalid(seq, "does not follow the link before it"));
        }
        match payload.via.as_deref() {
            None => {}
            Some(UPGRADE_VIA) if seq > 0 && matches!(payload.change, Change::Add { .. }) => {}
            Some(other) => return Err(invalid(seq, format!("unexpected via {other:?}"))),
        }

        if seq == 0 {
            let Change::Add { device } = &payload.change else {
                return Err(invalid(seq, "the first link must add a device"));
            };
            if device.id != link.by {
                return Err(invalid(seq, "the first device must sign its own link"));
            }
        } else if !self.members.contains_key(&link.by) {
            return Err(invalid(seq, "signed by a device not on the list"));
        }

        match &payload.change {
            Change::Add { device } => {
                check_device(seq, device)?;
                if self.members.contains_key(&device.id) {
                    return Err(invalid(seq, "adds a device already on the list"));
                }
                if self.members.len() >= MAX_DEVICES {
                    return Err(invalid(
                        seq,
                        format!("an account has at most {MAX_DEVICES} devices"),
                    ));
                }
                let needs_joiner = seq > 0 && payload.via.is_none();
                let cosigned = needs_joiner;
                if needs_joiner {
                    let joiner = link
                        .joiner
                        .as_deref()
                        .ok_or_else(|| invalid(seq, "the new device did not sign it"))?;
                    let key = did_key_to_verifying_key(&device.id)
                        .map_err(|e| invalid(seq, format!("device id: {e}")))?;
                    let (joiner_segment, _) = jws_verify(&key, joiner)
                        .map_err(|e| invalid(seq, format!("new device's signature: {e}")))?;
                    if joiner_segment != segment {
                        return Err(invalid(seq, "the new device signed another payload"));
                    }
                }
                self.members.insert(
                    device.id.clone(),
                    Member {
                        device: device.clone(),
                        added_seq: seq,
                        added_by: link.by.clone(),
                        cosigned,
                        added_at: payload.iat,
                    },
                );
            }
            Change::Remove { device_id } => {
                if !self.members.contains_key(device_id) {
                    return Err(invalid(seq, "removes a device not on the list"));
                }
                if self.members.len() == 1 {
                    return Err(invalid(
                        seq,
                        "the last device cannot be removed; a new first device starts a new list",
                    ));
                }
                self.members.remove(device_id);
            }
        }
        self.hashes.push(link.hash());
        self.links.push(link);
        Ok(())
    }

    /// The account it belongs to (`user:hub:<id>`).
    #[must_use]
    pub fn account(&self) -> &str {
        &self.account
    }

    /// What pins the account: the first link's hash.
    #[must_use]
    pub fn genesis(&self) -> &str {
        &self.hashes[0]
    }

    /// Where the chain ends.
    #[must_use]
    pub fn head(&self) -> Head {
        Head {
            seq: (self.links.len() - 1) as u64,
            hash: self.hashes[self.hashes.len() - 1].clone(),
        }
    }

    /// The links, oldest first.
    #[must_use]
    pub fn links(&self) -> &[Link] {
        &self.links
    }

    /// Whether `device` is on the list, and as what.
    #[must_use]
    pub fn device_of(&self, device: &str) -> Option<DeviceKind> {
        self.members.get(device).map(|m| m.device.kind)
    }

    /// The devices on the list, by id.
    pub fn members(&self) -> impl Iterator<Item = &Member> {
        self.members.values()
    }

    /// The device `id`, when it is on the list.
    #[must_use]
    pub fn member(&self, id: &str) -> Option<&Member> {
        self.members.get(id)
    }

    /// The phone PekoHub knows as `hub_device_id`, when it is on the list.
    #[must_use]
    pub fn phone(&self, hub_device_id: &str) -> Option<&Member> {
        self.members.values().find(|m| {
            m.device.kind == DeviceKind::Phone
                && m.device.hub_device_id.as_deref() == Some(hub_device_id)
        })
    }

    /// The computers on the list.
    pub fn computers(&self) -> impl Iterator<Item = &Device> {
        self.members
            .values()
            .map(|m| &m.device)
            .filter(|d| d.kind == DeviceKind::Computer)
    }

    /// The device that signed the first link.
    #[must_use]
    pub fn first_device(&self) -> &str {
        &self.links[0].by
    }

    /// The payload of the link that would come next.
    #[must_use]
    pub fn next_payload(&self, change: Change) -> LinkPayload {
        LinkPayload {
            v: DEVICE_LIST_VERSION.into(),
            account: self.account.clone(),
            seq: self.links.len() as u64,
            prev: self.hashes.last().cloned(),
            change,
            via: None,
            iat: chrono::Utc::now().timestamp(),
        }
    }

    /// Whether `other` starts with every link of this chain.
    fn is_prefix_of(&self, other: &Self) -> bool {
        other.hashes.len() >= self.hashes.len()
            && other.hashes[..self.hashes.len()] == self.hashes[..]
    }
}

fn check_device(seq: u64, device: &Device) -> Result<(), ListError> {
    did_key_to_verifying_key(&device.id)
        .map_err(|e| invalid(seq, format!("device id is not a did:key: {e}")))?;
    URL_SAFE_NO_PAD
        .decode(&device.agreement_key)
        .ok()
        .filter(|b| b.len() == 32)
        .ok_or_else(|| invalid(seq, "agreement key is not 32 bytes of base64url"))?;
    if let Some(name) = &device.name {
        if name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
            return Err(invalid(
                seq,
                format!("a device name is at most {MAX_NAME_CHARS} characters, without control characters"),
            ));
        }
    }
    Ok(())
}

/// Sign `payload` as `key`: the JWS that goes in [`Link::signed`], or in
/// [`Link::joiner`] when `key` is the device being added.
#[must_use]
pub fn sign_payload(key: &SigningKey, payload: &LinkPayload) -> String {
    sign_payload_bytes(key, &payload_bytes(payload))
}

/// The bytes a payload is signed as. Two devices that sign one link sign
/// these exact bytes (the join sends them as a string, D3).
#[must_use]
pub fn payload_bytes(payload: &LinkPayload) -> Vec<u8> {
    serde_json::to_vec(payload).expect("a link payload serializes")
}

/// Sign payload bytes as they were sent.
#[must_use]
pub fn sign_payload_bytes(key: &SigningKey, bytes: &[u8]) -> String {
    jws_sign(key, bytes)
}

/// The `did:key` of `key`.
#[must_use]
pub fn did_of(key: &SigningKey) -> String {
    verifying_key_to_did_key(&key.verifying_key())
}

/// The first link of `account`'s list: `key`'s device adding itself.
#[must_use]
pub fn genesis(
    key: &SigningKey,
    account: &str,
    kind: DeviceKind,
    name: Option<&str>,
    agreement_key: &str,
) -> Link {
    let id = did_of(key);
    let payload = LinkPayload {
        v: DEVICE_LIST_VERSION.into(),
        account: account.to_string(),
        seq: 0,
        prev: None,
        change: Change::Add {
            device: Device {
                id: id.clone(),
                kind,
                name: name.map(str::to_owned),
                agreement_key: agreement_key.to_string(),
                hub_device_id: None,
            },
        },
        via: None,
        iat: chrono::Utc::now().timestamp(),
    };
    Link {
        by: id,
        signed: sign_payload(key, &payload),
        joiner: None,
    }
}

/// What taking a chain changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accepted {
    /// The first list seen for the account: pinned by its genesis.
    Pinned,
    /// It grew. The ids of the devices it added and removed.
    Extended {
        added: Vec<String>,
        removed: Vec<String>,
    },
    /// Nothing this computer did not already hold.
    Unchanged,
    /// This computer's own account's list, which it is not on (or cannot
    /// anchor) yet: kept to show, never used to decide (D3).
    NotOnIt,
}

/// A contact's list that starts from another first device (D4), waiting
/// for the owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReset {
    pub account: String,
    pub links: Vec<Link>,
    pub reported_at: chrono::DateTime<chrono::Utc>,
}

/// Approval-request ids of a contact's reset start with this.
pub const RESET_ID_PREFIX: &str = "apr_devreset";

impl PendingReset {
    /// The id the owner decides it by.
    #[must_use]
    pub fn request_id(&self) -> String {
        let hash = Sha256::digest(self.account.as_bytes());
        format!("{RESET_ID_PREFIX}{}", hex::encode(&hash[..8]))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ListsFile {
    #[serde(default)]
    accounts: BTreeMap<String, Vec<Link>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    resets: Vec<PendingReset>,
    /// Accounts whose per-computer files were moved onto the list (D7).
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    upgraded: std::collections::BTreeSet<String>,
}

type ChangeHook = std::sync::Arc<dyn Fn(&str, &Accepted) + Send + Sync>;

/// The device lists this computer holds, one per account (D5).
pub struct DeviceLists {
    path: Option<PathBuf>,
    lists: Mutex<HashMap<String, DeviceList>>,
    resets: Mutex<Vec<PendingReset>>,
    upgraded: Mutex<std::collections::BTreeSet<String>>,
    /// This computer's own account's latest list while it is not on it.
    unanchored: Mutex<Option<DeviceList>>,
    on_change: Mutex<Option<ChangeHook>>,
}

impl std::fmt::Debug for DeviceLists {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceLists")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl DeviceLists {
    /// The lists kept at `path` (`None`: in memory only). A stored chain
    /// that no longer checks out is dropped, with a warning.
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Self {
        let file: ListsFile = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let mut lists = HashMap::new();
        for (account, links) in file.accounts {
            match DeviceList::verify(&account, links) {
                Ok(list) => {
                    lists.insert(account, list);
                }
                Err(e) => tracing::warn!(%account, "dropping a stored device list: {e}"),
            }
        }
        Self {
            path,
            lists: Mutex::new(lists),
            resets: Mutex::new(file.resets),
            upgraded: Mutex::new(file.upgraded),
            unanchored: Mutex::new(None),
            on_change: Mutex::new(None),
        }
    }

    /// Call `hook` after every change a list makes; the latest hook wins.
    pub fn on_change(&self, hook: impl Fn(&str, &Accepted) + Send + Sync + 'static) {
        *self.on_change.lock().unwrap() = Some(std::sync::Arc::new(hook));
    }

    fn changed(&self, account: &str, outcome: &Accepted) {
        if matches!(outcome, Accepted::Unchanged) {
            return;
        }
        let hook = self.on_change.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(account, outcome);
        }
    }

    fn save(&self, lists: &HashMap<String, DeviceList>) {
        let Some(path) = &self.path else { return };
        let file = ListsFile {
            accounts: lists
                .iter()
                .map(|(account, list)| (account.clone(), list.links.clone()))
                .collect(),
            resets: self.resets.lock().unwrap().clone(),
            upgraded: self.upgraded.lock().unwrap().clone(),
        };
        let bytes = serde_json::to_vec_pretty(&file).expect("device lists serialize");
        if let Err(e) = crate::security::computers::write_private(path, &bytes) {
            tracing::warn!(path = %path.display(), "could not save device lists: {e}");
        }
    }

    /// Take `links` as a contact's (`account`'s) whole chain. The first one
    /// seen pins the account (first-contact trust, as ADR-110 D4). After
    /// that a chain is taken only when it extends the one held; one this
    /// computer already holds more of changes nothing. A chain from another
    /// first device is kept for the owner to decide (D4).
    ///
    /// # Errors
    ///
    /// The chain does not check out, diverges from the one held
    /// ([`ListError::Fork`]), or starts from another first device
    /// ([`ListError::Reset`]).
    pub fn accept(&self, account: &str, links: Vec<Link>) -> Result<Accepted, ListError> {
        let offered = DeviceList::verify(account, links)?;
        let outcome = self.take(account, offered.clone());
        if let Err(ListError::Reset { .. }) = &outcome {
            let mut resets = self.resets.lock().unwrap();
            resets.retain(|r| r.account != account);
            resets.push(PendingReset {
                account: account.to_string(),
                links: offered.links,
                reported_at: chrono::Utc::now(),
            });
            drop(resets);
            self.save(&self.lists.lock().unwrap());
        }
        outcome
    }

    /// Take `links` as this computer's own account's chain (D3, D7). Until
    /// a list is held, one is pinned only when this computer (`me`) is on
    /// it and can anchor it: it made the first link, it joined by a code
    /// (its own signature is on the link that added it), or the first
    /// device or the one that added it is one the owner already approved
    /// here (`approved`, from the per-computer files of ADR-108/109). Else
    /// it is kept to show (`Accepted::NotOnIt`) and decides nothing.
    ///
    /// # Errors
    ///
    /// As [`DeviceLists::accept`], except that a reset of one's own list is
    /// never taken.
    pub fn accept_own(
        &self,
        account: &str,
        links: Vec<Link>,
        me: &str,
        approved: &std::collections::HashSet<String>,
    ) -> Result<Accepted, ListError> {
        let offered = DeviceList::verify(account, links)?;
        if self.lists.lock().unwrap().contains_key(account) {
            return self.take(account, offered);
        }
        let anchored = offered.member(me).is_some_and(|m| {
            offered.first_device() == me
                || m.cosigned
                || approved.contains(offered.first_device())
                || approved.contains(&m.added_by)
        });
        if !anchored {
            *self.unanchored.lock().unwrap() = Some(offered);
            self.changed(account, &Accepted::NotOnIt);
            return Ok(Accepted::NotOnIt);
        }
        *self.unanchored.lock().unwrap() = None;
        self.take(account, offered)
    }

    /// Take `more`, the links after the chain held for `account` (PekoHub's
    /// answer to a request for what follows a head).
    ///
    /// # Errors
    ///
    /// No chain is held for `account` ([`ListError::Empty`]), or a link
    /// does not follow.
    pub fn accept_after(&self, account: &str, more: Vec<Link>) -> Result<Accepted, ListError> {
        if more.is_empty() {
            return Ok(Accepted::Unchanged);
        }
        let held = self
            .lists
            .lock()
            .unwrap()
            .get(account)
            .cloned()
            .ok_or(ListError::Empty)?;
        let offered = held.extended(more)?;
        self.take(account, offered)
    }

    fn take(&self, account: &str, offered: DeviceList) -> Result<Accepted, ListError> {
        let mut lists = self.lists.lock().unwrap();
        let outcome = match lists.get(account) {
            None => Accepted::Pinned,
            Some(held) if held.genesis() != offered.genesis() => {
                return Err(ListError::Reset {
                    account: account.to_string(),
                })
            }
            Some(held) if offered.is_prefix_of(held) => return Ok(Accepted::Unchanged),
            Some(held) if held.is_prefix_of(&offered) => Accepted::Extended {
                added: offered
                    .members
                    .keys()
                    .filter(|id| !held.members.contains_key(*id))
                    .cloned()
                    .collect(),
                removed: held
                    .members
                    .keys()
                    .filter(|id| !offered.members.contains_key(*id))
                    .cloned()
                    .collect(),
            },
            Some(_) => {
                return Err(ListError::Fork {
                    account: account.to_string(),
                })
            }
        };
        lists.insert(account.to_string(), offered);
        self.save(&lists);
        drop(lists);
        self.changed(account, &outcome);
        Ok(outcome)
    }

    /// Whether `device` is on `account`'s list, and as what. `None` when it
    /// is not, or when no list is held for the account.
    #[must_use]
    pub fn device_of(&self, account: &str, device: &str) -> Option<DeviceKind> {
        self.lists
            .lock()
            .unwrap()
            .get(account)
            .and_then(|l| l.device_of(device))
    }

    /// Where `account`'s chain ends, when one is held.
    #[must_use]
    pub fn head(&self, account: &str) -> Option<Head> {
        self.lists
            .lock()
            .unwrap()
            .get(account)
            .map(DeviceList::head)
    }

    /// `account`'s list, when one is held.
    #[must_use]
    pub fn list(&self, account: &str) -> Option<DeviceList> {
        self.lists.lock().unwrap().get(account).cloned()
    }

    /// This computer's own account's latest list while it is not on it.
    #[must_use]
    pub fn unanchored(&self) -> Option<DeviceList> {
        self.unanchored.lock().unwrap().clone()
    }

    /// Whether this computer already moved its per-computer files onto
    /// `account`'s list (D7).
    #[must_use]
    pub fn upgraded(&self, account: &str) -> bool {
        self.upgraded.lock().unwrap().contains(account)
    }

    /// Record that the per-computer files are on `account`'s list now, so
    /// a device the owner removes later is not added back.
    pub fn mark_upgraded(&self, account: &str) {
        self.upgraded.lock().unwrap().insert(account.to_string());
        self.save(&self.lists.lock().unwrap());
    }

    /// Contacts' lists that were reset, waiting for the owner.
    #[must_use]
    pub fn pending_resets(&self) -> Vec<PendingReset> {
        self.resets.lock().unwrap().clone()
    }

    /// The owner's decision on a reset (by its request id): approving pins
    /// the new list; denying keeps the old one and forgets the new.
    /// Returns the account, or `None` when no such reset waits.
    pub fn decide_reset(&self, id: &str, approve: bool) -> Option<String> {
        let reset = {
            let mut resets = self.resets.lock().unwrap();
            let at = resets.iter().position(|r| r.request_id() == id)?;
            resets.remove(at)
        };
        let mut lists = self.lists.lock().unwrap();
        if approve {
            match DeviceList::verify(&reset.account, reset.links) {
                Ok(list) => {
                    lists.insert(reset.account.clone(), list);
                }
                Err(e) => {
                    tracing::warn!(account = %reset.account, "a reset list no longer checks out: {e}")
                }
            }
        }
        self.save(&lists);
        drop(lists);
        if approve {
            self.changed(&reset.account, &Accepted::Pinned);
        }
        Some(reset.account)
    }
}

/// `account`'s list of these devices, the first adding the rest (each
/// signing its own link, as a join does). For tests elsewhere.
#[cfg(any(test, feature = "test-utils"))]
#[must_use]
pub fn test_chain(account: &str, devices: &[(&SigningKey, Device)]) -> Vec<Link> {
    let (first, first_device) = &devices[0];
    let mut list = DeviceList::verify(
        account,
        vec![genesis(
            first,
            account,
            first_device.kind,
            first_device.name.as_deref(),
            &first_device.agreement_key,
        )],
    )
    .expect("a first link checks out");
    for (key, device) in &devices[1..] {
        let payload = list.next_payload(Change::Add {
            device: device.clone(),
        });
        list = list
            .extended(vec![Link {
                by: did_of(first),
                signed: sign_payload(first, &payload),
                joiner: Some(sign_payload(key, &payload)),
            }])
            .expect("an add checks out");
    }
    list.links
}

/// A computer as the list records it, from its runtime key. For tests.
#[cfg(any(test, feature = "test-utils"))]
#[must_use]
pub fn test_computer(key: &SigningKey) -> Device {
    Device {
        id: did_of(key),
        kind: DeviceKind::Computer,
        name: None,
        agreement_key: crate::tunnel::seal::AgreementKey::derive(key)
            .public()
            .to_string(),
        hub_device_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "user:hub:3f2c9a7e-1111-4222-8333-944455556666";

    fn key() -> SigningKey {
        SigningKey::from_bytes(&rand::random::<[u8; 32]>())
    }

    fn agreement() -> String {
        URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
    }

    fn device(key: &SigningKey, kind: DeviceKind, name: &str) -> Device {
        Device {
            id: did_of(key),
            kind,
            name: Some(name.into()),
            agreement_key: agreement(),
            hub_device_id: None,
        }
    }

    /// `signer` adds `new`'s device, which signs too.
    fn add(list: &DeviceList, signer: &SigningKey, new: &SigningKey, kind: DeviceKind) -> Link {
        let payload = list.next_payload(Change::Add {
            device: device(new, kind, "new"),
        });
        Link {
            by: did_of(signer),
            signed: sign_payload(signer, &payload),
            joiner: Some(sign_payload(new, &payload)),
        }
    }

    fn remove(list: &DeviceList, signer: &SigningKey, gone: &SigningKey) -> Link {
        let payload = list.next_payload(Change::Remove {
            device_id: did_of(gone),
        });
        Link {
            by: did_of(signer),
            signed: sign_payload(signer, &payload),
            joiner: None,
        }
    }

    fn first(desktop: &SigningKey) -> DeviceList {
        DeviceList::verify(
            ACCOUNT,
            vec![genesis(
                desktop,
                ACCOUNT,
                DeviceKind::Computer,
                Some("Desk"),
                &agreement(),
            )],
        )
        .unwrap()
    }

    fn reason(err: ListError) -> String {
        match err {
            ListError::Invalid { reason, .. } => reason,
            other => panic!("expected an invalid link, got {other:?}"),
        }
    }

    #[test]
    fn a_device_is_on_the_list_from_its_add_until_its_remove() {
        let (desktop, laptop, phone) = (key(), key(), key());
        let list = first(&desktop);
        assert_eq!(
            list.device_of(&did_of(&desktop)),
            Some(DeviceKind::Computer)
        );
        assert_eq!(list.device_of(&did_of(&laptop)), None);

        let list = list
            .extended(vec![add(&list, &desktop, &laptop, DeviceKind::Computer)])
            .unwrap();
        // The laptop, now on the list, adds the phone.
        let list = list
            .extended(vec![add(&list, &laptop, &phone, DeviceKind::Phone)])
            .unwrap();
        assert_eq!(list.device_of(&did_of(&laptop)), Some(DeviceKind::Computer));
        assert_eq!(list.device_of(&did_of(&phone)), Some(DeviceKind::Phone));
        let phone_member = list
            .members()
            .find(|m| m.device.id == did_of(&phone))
            .unwrap();
        assert_eq!(
            (phone_member.added_seq, phone_member.added_by.as_str()),
            (2, did_of(&laptop).as_str())
        );

        // The phone removes the laptop; the laptop can sign nothing after.
        let list = list.extended(vec![remove(&list, &phone, &laptop)]).unwrap();
        assert_eq!(list.device_of(&did_of(&laptop)), None);
        assert_eq!(list.head().seq, 3);
        let err = list
            .extended(vec![add(&list, &laptop, &key(), DeviceKind::Computer)])
            .unwrap_err();
        assert_eq!(reason(err), "signed by a device not on the list");

        // The whole chain checks out again from its links.
        let again = DeviceList::verify(ACCOUNT, list.links().to_vec()).unwrap();
        assert_eq!(again, list);
    }

    #[test]
    fn a_link_keeps_the_payload_shape_of_the_adr() {
        let desktop = key();
        let list = first(&desktop);
        let payload = list.next_payload(Change::Remove {
            device_id: "did:key:x".into(),
        });
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["op"], "remove");
        assert_eq!(json["device_id"], "did:key:x");
        assert_eq!(json["seq"], 1);
        assert_eq!(json["prev"], list.genesis());
        let payload = list.next_payload(Change::Add {
            device: device(&key(), DeviceKind::Phone, "Mira's iPhone"),
        });
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["op"], "add");
        assert_eq!(json["device"]["kind"], "phone");
        assert_eq!(
            serde_json::from_value::<LinkPayload>(json).unwrap(),
            payload
        );
    }

    #[test]
    fn nobody_enrolls_a_key_they_do_not_hold() {
        let (desktop, laptop) = (key(), key());
        let list = first(&desktop);

        // No signature by the new device.
        let mut link = add(&list, &desktop, &laptop, DeviceKind::Computer);
        link.joiner = None;
        assert_eq!(
            reason(list.extended(vec![link]).unwrap_err()),
            "the new device did not sign it"
        );

        // The new device signed some other payload.
        let mut link = add(&list, &desktop, &laptop, DeviceKind::Computer);
        let other = list.next_payload(Change::Add {
            device: device(&laptop, DeviceKind::Phone, "other"),
        });
        link.joiner = Some(sign_payload(&laptop, &other));
        assert_eq!(
            reason(list.extended(vec![link]).unwrap_err()),
            "the new device signed another payload"
        );

        // Someone else signed in the new device's place.
        let mut link = add(&list, &desktop, &laptop, DeviceKind::Computer);
        let payload = list.next_payload(Change::Add {
            device: device(&laptop, DeviceKind::Computer, "new"),
        });
        link.signed = sign_payload(&desktop, &payload);
        link.joiner = Some(sign_payload(&key(), &payload));
        assert!(
            reason(list.extended(vec![link]).unwrap_err()).starts_with("new device's signature")
        );

        // The upgrade pass may add without it (D7), but only as an add after
        // the first link.
        let mut payload = list.next_payload(Change::Add {
            device: device(&laptop, DeviceKind::Computer, "Laptop"),
        });
        payload.via = Some(UPGRADE_VIA.into());
        let link = Link {
            by: did_of(&desktop),
            signed: sign_payload(&desktop, &payload),
            joiner: None,
        };
        let list = list.extended(vec![link]).unwrap();
        assert_eq!(list.device_of(&did_of(&laptop)), Some(DeviceKind::Computer));
    }

    #[test]
    fn only_a_device_on_the_list_signs_and_only_in_order() {
        let (desktop, stranger, laptop) = (key(), key(), key());
        let list = first(&desktop);

        // A device not on the list cannot add one.
        let err = list
            .extended(vec![add(&list, &stranger, &laptop, DeviceKind::Computer)])
            .unwrap_err();
        assert_eq!(reason(err), "signed by a device not on the list");

        // A link that skips ahead, or follows another link.
        let mut payload = list.next_payload(Change::Add {
            device: device(&laptop, DeviceKind::Computer, "Laptop"),
        });
        payload.seq = 2;
        let skip = Link {
            by: did_of(&desktop),
            signed: sign_payload(&desktop, &payload),
            joiner: Some(sign_payload(&laptop, &payload)),
        };
        assert_eq!(
            reason(list.extended(vec![skip]).unwrap_err()),
            "says it is link 2"
        );
        let mut payload = list.next_payload(Change::Add {
            device: device(&laptop, DeviceKind::Computer, "Laptop"),
        });
        payload.prev = Some("not-the-genesis".into());
        let detached = Link {
            by: did_of(&desktop),
            signed: sign_payload(&desktop, &payload),
            joiner: Some(sign_payload(&laptop, &payload)),
        };
        assert_eq!(
            reason(list.extended(vec![detached]).unwrap_err()),
            "does not follow the link before it"
        );

        // Another account's chain is not this one's.
        assert_eq!(
            reason(DeviceList::verify("user:hub:someone-else", list.links().to_vec()).unwrap_err()),
            "names another account"
        );

        // A changed signature does not verify.
        let mut tampered = list.links().to_vec();
        tampered[0].signed.push('A');
        assert!(DeviceList::verify(ACCOUNT, tampered).is_err());
    }

    #[test]
    fn the_first_link_adds_its_own_signer() {
        let (desktop, other) = (key(), key());
        let payload = LinkPayload {
            v: DEVICE_LIST_VERSION.into(),
            account: ACCOUNT.into(),
            seq: 0,
            prev: None,
            change: Change::Add {
                device: device(&other, DeviceKind::Computer, "Other"),
            },
            via: None,
            iat: 0,
        };
        let link = Link {
            by: did_of(&desktop),
            signed: sign_payload(&desktop, &payload),
            joiner: None,
        };
        assert_eq!(
            reason(DeviceList::verify(ACCOUNT, vec![link]).unwrap_err()),
            "the first device must sign its own link"
        );
        assert_eq!(
            DeviceList::verify(ACCOUNT, Vec::new()).unwrap_err(),
            ListError::Empty
        );
    }

    #[test]
    fn the_last_device_stays_and_a_device_is_added_once() {
        let (desktop, laptop) = (key(), key());
        let list = first(&desktop);
        assert_eq!(
            reason(
                list.extended(vec![remove(&list, &desktop, &desktop)])
                    .unwrap_err()
            ),
            "the last device cannot be removed; a new first device starts a new list"
        );
        let list = list
            .extended(vec![add(&list, &desktop, &laptop, DeviceKind::Computer)])
            .unwrap();
        assert_eq!(
            reason(
                list.extended(vec![add(&list, &desktop, &laptop, DeviceKind::Computer)])
                    .unwrap_err()
            ),
            "adds a device already on the list"
        );
        // A device may remove itself while another remains.
        let list = list
            .extended(vec![remove(&list, &laptop, &laptop)])
            .unwrap();
        assert_eq!(list.device_of(&did_of(&laptop)), None);
        // And come back by joining again.
        let list = list
            .extended(vec![add(&list, &desktop, &laptop, DeviceKind::Computer)])
            .unwrap();
        assert_eq!(list.device_of(&did_of(&laptop)), Some(DeviceKind::Computer));
    }

    #[test]
    fn a_device_must_be_well_formed() {
        let desktop = key();
        let list = first(&desktop);
        let laptop = key();
        let bad = |edit: &dyn Fn(&mut Device)| {
            let mut d = device(&laptop, DeviceKind::Computer, "Laptop");
            edit(&mut d);
            let payload = list.next_payload(Change::Add { device: d });
            let link = Link {
                by: did_of(&desktop),
                signed: sign_payload(&desktop, &payload),
                joiner: Some(sign_payload(&laptop, &payload)),
            };
            reason(list.extended(vec![link]).unwrap_err())
        };
        assert_eq!(
            bad(&|d| d.agreement_key = "short".into()),
            "agreement key is not 32 bytes of base64url"
        );
        assert!(bad(&|d| d.name = Some("x".repeat(65))).starts_with("a device name is at most"));
        assert!(bad(&|d| d.name = Some("bad\nname".into())).starts_with("a device name is at most"));
    }

    #[test]
    fn this_computer_pins_a_list_then_only_lets_it_grow() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device_lists.json");
        let lists = DeviceLists::new(Some(path.clone()));
        let (desktop, laptop, phone) = (key(), key(), key());

        let one = first(&desktop);
        let two = one
            .extended(vec![add(&one, &desktop, &laptop, DeviceKind::Computer)])
            .unwrap();
        let three = two
            .extended(vec![add(&two, &laptop, &phone, DeviceKind::Phone)])
            .unwrap();

        assert_eq!(
            lists.device_of(ACCOUNT, &did_of(&desktop)),
            None,
            "nothing held yet"
        );
        assert_eq!(
            lists.accept(ACCOUNT, two.links().to_vec()).unwrap(),
            Accepted::Pinned
        );
        assert_eq!(
            lists.device_of(ACCOUNT, &did_of(&laptop)),
            Some(DeviceKind::Computer)
        );

        // An older head (PekoHub serving less) changes nothing.
        assert_eq!(
            lists.accept(ACCOUNT, one.links().to_vec()).unwrap(),
            Accepted::Unchanged
        );
        assert_eq!(lists.head(ACCOUNT).unwrap(), two.head());

        // What follows the head, alone.
        assert_eq!(
            lists
                .accept_after(ACCOUNT, three.links()[2..].to_vec())
                .unwrap(),
            Accepted::Extended {
                added: vec![did_of(&phone)],
                removed: Vec::new()
            }
        );

        // Another link 2 from the same head: a fork, refused.
        let fork = two
            .extended(vec![add(&two, &desktop, &key(), DeviceKind::Computer)])
            .unwrap();
        assert_eq!(
            lists.accept(ACCOUNT, fork.links().to_vec()).unwrap_err(),
            ListError::Fork {
                account: ACCOUNT.into()
            }
        );

        // A list from another first device: a reset, refused.
        assert_eq!(
            lists
                .accept(ACCOUNT, first(&key()).links().to_vec())
                .unwrap_err(),
            ListError::Reset {
                account: ACCOUNT.into()
            }
        );

        // Kept across a restart, private.
        let again = DeviceLists::new(Some(path.clone()));
        assert_eq!(again.head(ACCOUNT).unwrap(), three.head());
        assert_eq!(
            again.device_of(ACCOUNT, &did_of(&phone)),
            Some(DeviceKind::Phone)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_stored_list_that_no_longer_checks_out_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device_lists.json");
        let desktop = key();
        let lists = DeviceLists::new(Some(path.clone()));
        lists
            .accept(ACCOUNT, first(&desktop).links().to_vec())
            .unwrap();
        let mut file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        file["accounts"][ACCOUNT][0]["by"] = serde_json::Value::String(did_of(&key()));
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        let again = DeviceLists::new(Some(path));
        assert_eq!(again.head(ACCOUNT), None);
    }

    #[test]
    fn a_computer_pins_its_own_list_only_when_it_can_anchor_it() {
        use std::collections::HashSet;
        let (desktop, laptop, rogue) = (key(), key(), key());
        let none = HashSet::new();

        // PekoHub makes a list of its own and adds this laptop without the
        // laptop's signature (an upgrade link): the laptop is on it, but
        // nothing it trusts made it.
        let fake = first(&rogue);
        let mut payload = fake.next_payload(Change::Add {
            device: device(&laptop, DeviceKind::Computer, "Laptop"),
        });
        payload.via = Some(UPGRADE_VIA.into());
        let fake = fake
            .extended(vec![Link {
                by: did_of(&rogue),
                signed: sign_payload(&rogue, &payload),
                joiner: None,
            }])
            .unwrap();
        let lists = DeviceLists::new(None);
        assert_eq!(
            lists
                .accept_own(ACCOUNT, fake.links().to_vec(), &did_of(&laptop), &none)
                .unwrap(),
            Accepted::NotOnIt
        );
        assert_eq!(
            lists.device_of(ACCOUNT, &did_of(&rogue)),
            None,
            "decides nothing"
        );
        assert!(lists.unanchored().is_some());

        // The real list, where the laptop joined by a code: anchored by its
        // own signature.
        let real = first(&desktop);
        let real = real
            .extended(vec![add(&real, &desktop, &laptop, DeviceKind::Computer)])
            .unwrap();
        assert_eq!(
            lists
                .accept_own(ACCOUNT, real.links().to_vec(), &did_of(&laptop), &none)
                .unwrap(),
            Accepted::Pinned
        );
        assert!(lists.unanchored().is_none());
        assert_eq!(
            lists.device_of(ACCOUNT, &did_of(&desktop)),
            Some(DeviceKind::Computer)
        );
        // Once pinned, another first device is a reset, never taken.
        assert!(matches!(
            lists.accept_own(ACCOUNT, fake.links().to_vec(), &did_of(&laptop), &none),
            Err(ListError::Reset { .. })
        ));
        assert!(
            lists.pending_resets().is_empty(),
            "one's own list is not offered"
        );

        // Moving from the old files: the computer that wrote the first link
        // was approved here, so its upgrade link anchors the list.
        let upgraded = first(&desktop);
        let mut payload = upgraded.next_payload(Change::Add {
            device: device(&laptop, DeviceKind::Computer, "Laptop"),
        });
        payload.via = Some(UPGRADE_VIA.into());
        let upgraded = upgraded
            .extended(vec![Link {
                by: did_of(&desktop),
                signed: sign_payload(&desktop, &payload),
                joiner: None,
            }])
            .unwrap();
        let fresh = DeviceLists::new(None);
        let approved: HashSet<String> = [did_of(&desktop)].into();
        assert_eq!(
            fresh
                .accept_own(
                    ACCOUNT,
                    upgraded.links().to_vec(),
                    &did_of(&laptop),
                    &approved
                )
                .unwrap(),
            Accepted::Pinned
        );
        // The first device itself always anchors its own list.
        let alone = DeviceLists::new(None);
        assert_eq!(
            alone
                .accept_own(
                    ACCOUNT,
                    first(&desktop).links().to_vec(),
                    &did_of(&desktop),
                    &none
                )
                .unwrap(),
            Accepted::Pinned
        );
    }

    #[test]
    fn a_contacts_reset_waits_for_the_owner() {
        let lists = DeviceLists::new(None);
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let log = std::sync::Arc::clone(&seen);
        lists.on_change(move |account, outcome| {
            log.lock()
                .unwrap()
                .push((account.to_string(), outcome.clone()));
        });
        let (old, new) = (key(), key());
        lists.accept(ACCOUNT, first(&old).links().to_vec()).unwrap();
        assert!(matches!(
            lists.accept(ACCOUNT, first(&new).links().to_vec()),
            Err(ListError::Reset { .. })
        ));
        let pending = lists.pending_resets();
        assert_eq!(pending.len(), 1);
        let id = pending[0].request_id();
        assert!(id.starts_with(RESET_ID_PREFIX));
        assert_eq!(lists.device_of(ACCOUNT, &did_of(&new)), None);
        assert_eq!(lists.decide_reset(&id, true).as_deref(), Some(ACCOUNT));
        assert_eq!(
            lists.device_of(ACCOUNT, &did_of(&new)),
            Some(DeviceKind::Computer)
        );
        assert_eq!(lists.device_of(ACCOUNT, &did_of(&old)), None);
        assert!(lists.pending_resets().is_empty());
        assert_eq!(lists.decide_reset(&id, true), None);
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.len(),
            2,
            "pinned, then pinned again by the decision: {seen:?}"
        );
    }

    #[test]
    fn a_phone_is_found_by_its_hub_id() {
        let (desktop, phone) = (key(), key());
        let list = first(&desktop);
        let mut d = device(&phone, DeviceKind::Phone, "Mira's iPhone");
        d.hub_device_id = Some("dev_5f0c1c1e9a7b4f52".into());
        let payload = list.next_payload(Change::Add { device: d });
        let list = list
            .extended(vec![Link {
                by: did_of(&desktop),
                signed: sign_payload(&desktop, &payload),
                joiner: Some(sign_payload(&phone, &payload)),
            }])
            .unwrap();
        assert_eq!(
            list.phone("dev_5f0c1c1e9a7b4f52").unwrap().device.id,
            did_of(&phone)
        );
        assert!(list.phone("dev_other").is_none());
        assert_eq!(list.computers().count(), 1);
    }

    /// A chain this runtime signs (a computer adding a phone, then removing
    /// a second computer), for peko-mobile's checks of its own
    /// (`tests/fixtures/device-list.json` there):
    /// `PEKO_DEVICE_LIST_FIXTURE=<path> cargo test -p peko --lib
    /// security::device_list::tests::write_mobile_fixture -- --ignored`.
    #[test]
    #[ignore = "writes a fixture for peko-mobile"]
    fn write_mobile_fixture() {
        let Some(path) = std::env::var_os("PEKO_DEVICE_LIST_FIXTURE") else {
            return;
        };
        let desk = SigningKey::from_bytes(&[1u8; 32]);
        let laptop = SigningKey::from_bytes(&[2u8; 32]);
        let phone = SigningKey::from_bytes(&[3u8; 32]);
        let mut phone_device = device(&phone, DeviceKind::Phone, "Mira's iPhone");
        phone_device.hub_device_id = Some("dev_5f0c1c1e9a7b4f52".into());
        let links = test_chain(
            ACCOUNT,
            &[
                (&desk, device(&desk, DeviceKind::Computer, "Desk")),
                (&laptop, device(&laptop, DeviceKind::Computer, "Laptop")),
                (&phone, phone_device),
            ],
        );
        let list = DeviceList::verify(ACCOUNT, links).unwrap();
        let list = list.extended(vec![remove(&list, &desk, &laptop)]).unwrap();
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "account": ACCOUNT,
                "links": list.links(),
                "head": list.head(),
                "devices": list.members().map(|m| &m.device.id).collect::<Vec<_>>(),
                "phone": did_of(&phone),
                "desk": did_of(&desk),
            }))
            .unwrap(),
        )
        .unwrap();
    }
}
