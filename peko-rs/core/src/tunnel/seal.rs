//! What crosses PekoHub is sealed to the computer it is for (ADR-110).
//!
//! Every envelope one computer sends another through the Hub keeps a
//! readable header (the fields the Hub routes and checks on, D1) and
//! carries everything else in `sealed`: HPKE base mode (RFC 9180,
//! DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305) to the
//! recipient's agreement key, with the canonical header as associated
//! data (D3). The body still carries today's signatures, so a receiver
//! opens it and then verifies exactly as before.
//!
//! Each runtime's agreement key is derived from its signing key under its
//! own label and published as a key statement signed by the signing key
//! (D2), so the Hub can withhold a key but never make one. Which
//! computers of an account a message is sealed to is the account's device
//! list's to say (ADR-111 D5): the owner's own list, and another person's
//! pinned the first time it is seen.
//!
//! The [`Sealer`] sits in the tunnel client's read and write loops, so
//! every handler above it keeps working on the opened message. Nothing
//! of a sealed type goes out or comes in unsealed (D5).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::SigningKey;
use hpke::{
    aead::ChaCha20Poly1305, kdf::HkdfSha256, kem::X25519HkdfSha256, Deserializable,
    Kem as KemTrait, OpModeR, OpModeS, Serializable,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::{debug, warn};

use super::did_key::did_key_to_verifying_key;
use super::link::order::{is_ordered_type, Seq, Sequencer, SEQ_FIELD};
use super::protocol::{RuntimeKeyEntry, TunnelMessage};
use super::tunnel_channel_signature::{jws_sign, jws_verify};
use crate::security::device_list::{DeviceLists, Head};

type Kem = X25519HkdfSha256;

/// HPKE `info` of a sealed envelope.
pub const SEAL_INFO: &str = "peko-tunnel-seal/1";
/// Version tag of a runtime's key statement.
pub const KEY_STATEMENT_VERSION: &str = "peko-runtime-key/1";
/// Prefix of the input the agreement key is derived from, so it is never
/// the signing key (D2).
const AGREEMENT_KEY_LABEL: &[u8] = b"peko-agreement-key/1";
/// The field of a sealed body that carries the sender's key statement.
const SENDER_KEY_FIELD: &str = "senderKey";
/// The field of a sealed body that carries where the sender's account's
/// device list ends (ADR-111 D2), so a receiver that holds less asks.
const SENDER_DEVICES_FIELD: &str = "senderDevices";
/// How long a message waits for its recipient's key before it is dropped
/// (a post stays in the outbox and is sent again; ADR-072).
const PARK_FOR: Duration = Duration::from_secs(30);
/// How long a key request is left to answer before it is asked again.
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(10);
/// How long a contact's device list is used before it is asked for again
/// (ADR-111 D5). The owner's own list is kept current by PekoHub's pushes.
const ACCOUNT_LIST_FOR: Duration = Duration::from_mins(1);
/// A newer head seen in an envelope is asked about at most this often.
const HEAD_ASK_EVERY: Duration = Duration::from_secs(10);

/// The envelope types that are sealed (D1, D7).
pub const SEALED_TYPES: &[&str] = &[
    "tunnel_channel_event",
    "tunnel_channel_invite",
    "tunnel_channel_history_request",
    "tunnel_channel_history",
    "tunnel_attachment_request",
    "tunnel_attachment_chunk",
    "tunnel_account_dms_request",
    "tunnel_account_dms",
    "tunnel_person_dm_invite",
    "tunnel_dm_route",
    "tunnel_peko_models_request",
    "tunnel_peko_models",
    "tunnel_peko_approvals_request",
    "tunnel_peko_approvals",
    "tunnel_peko_approval_decide",
    "tunnel_peko_approval_decided",
    "tunnel_peko_browser_request",
    "tunnel_peko_browser",
    "tunnel_direct_addresses",
    "tunnel_read_mark",
    "tunnel_group_invitation",
    "tunnel_group_join",
    "tunnel_group_join_request",
    "tunnel_device_request",
    "tunnel_device_response",
    "tunnel_device_event",
];

/// The fields a sealed envelope keeps readable, in the order they enter
/// the associated data. Everything the Hub routes or checks on, and
/// nothing else (D1).
pub const HEADER_FIELDS: &[&str] = &[
    "type",
    "requestId",
    "sourceRuntimeId",
    "recipientRuntimeId",
    "recipientAccount",
    "sourcePrincipalDid",
    "sourceHubId",
    "channelId",
    "deviceId",
    "runtimeId",
    "size",
];

/// Whether envelopes of `ty` are sealed.
#[must_use]
pub fn is_sealed_type(ty: &str) -> bool {
    SEALED_TYPES.contains(&ty)
}

fn is_device_type(ty: &str) -> bool {
    ty.starts_with("tunnel_device_")
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// A runtime's X25519 agreement key (D2). It never signs; the signing key
/// never decrypts.
pub struct AgreementKey {
    secret: <Kem as KemTrait>::PrivateKey,
    public: String,
}

impl std::fmt::Debug for AgreementKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgreementKey")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

impl AgreementKey {
    /// The runtime's agreement key: HPKE `DeriveKeyPair` over the signing
    /// key's seed under its own label. The same signing key always gives
    /// the same agreement key, so it needs no storage of its own and
    /// moves wherever the runtime key does.
    #[must_use]
    pub fn derive(signing: &SigningKey) -> Self {
        let mut ikm = AGREEMENT_KEY_LABEL.to_vec();
        ikm.extend_from_slice(signing.as_bytes());
        let (secret, public) = Kem::derive_keypair(&ikm);
        Self {
            secret,
            public: URL_SAFE_NO_PAD.encode(public.to_bytes()),
        }
    }

    /// The public key, base64url of its 32 bytes.
    #[must_use]
    pub fn public(&self) -> &str {
        &self.public
    }

    /// Open what [`seal_to`] sealed to this key with `info` and `aad`.
    ///
    /// # Errors
    ///
    /// It was not sealed to this key, or was changed.
    pub fn open(&self, sealed: &str, info: &str, aad: &[u8]) -> Result<Vec<u8>, String> {
        let bytes = URL_SAFE_NO_PAD
            .decode(sealed)
            .map_err(|_| "sealed is not base64url".to_string())?;
        if bytes.len() < 32 + 16 {
            return Err("too short to be a sealed message".into());
        }
        let encapped = <Kem as KemTrait>::EncappedKey::from_bytes(&bytes[..32])
            .map_err(|_| "bad encapsulated key".to_string())?;
        hpke::single_shot_open::<ChaCha20Poly1305, HkdfSha256, Kem>(
            &OpModeR::Base,
            &self.secret,
            &encapped,
            info.as_bytes(),
            &bytes[32..],
            aad,
        )
        .map_err(|_| "does not open with this computer's key".to_string())
    }
}

/// The operating system's random numbers, in the form `hpke` takes them.
/// A failure to read them panics, as `rand::rngs::OsRng` does.
pub(crate) fn os_rng() -> impl hpke::rand_core::CryptoRng {
    use rand_core_hpke::TryRngCore as _;
    rand_core_hpke::OsRng.unwrap_err()
}

/// Seal `plaintext` to the X25519 key `public` (base64url) with `info`
/// and `aad`. Returns base64url of the encapsulated key and the
/// ciphertext.
///
/// # Errors
///
/// `public` is not an X25519 public key.
pub fn seal_to(public: &str, info: &str, plaintext: &[u8], aad: &[u8]) -> Result<String, String> {
    let key_bytes = URL_SAFE_NO_PAD
        .decode(public)
        .map_err(|_| "agreement key is not base64url".to_string())?;
    let recipient = <Kem as KemTrait>::PublicKey::from_bytes(&key_bytes)
        .map_err(|_| "agreement key is not an X25519 key".to_string())?;
    let (encapped, ciphertext) = hpke::single_shot_seal::<ChaCha20Poly1305, HkdfSha256, Kem, _>(
        &OpModeS::Base,
        &recipient,
        info.as_bytes(),
        plaintext,
        aad,
        &mut os_rng(),
    )
    .map_err(|e| format!("seal failed: {e}"))?;
    let mut out = encapped.to_bytes().to_vec();
    out.extend_from_slice(&ciphertext);
    Ok(URL_SAFE_NO_PAD.encode(out))
}

fn unverified_payload<T: serde::de::DeserializeOwned>(compact: &str) -> Result<T, String> {
    let payload = compact
        .split('.')
        .nth(1)
        .ok_or_else(|| "not a compact JWS".to_string())?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| "JWS payload is not base64url".to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| format!("JWS payload: {e}"))
}

fn verified_by<T: serde::de::DeserializeOwned>(did: &str, compact: &str) -> Result<T, String> {
    let key = did_key_to_verifying_key(did).map_err(|e| format!("{did} is not a did:key: {e}"))?;
    let (_, bytes) = jws_verify(&key, compact).map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| format!("JWS payload: {e}"))
}

/// A runtime's word for its agreement key (D2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyStatement {
    pub v: String,
    pub runtime_id: String,
    pub agreement_key: String,
    pub iat: i64,
}

/// Sign this runtime's key statement.
#[must_use]
pub fn sign_key_statement(signing: &SigningKey, runtime_id: &str, key: &AgreementKey) -> String {
    let payload = KeyStatement {
        v: KEY_STATEMENT_VERSION.into(),
        runtime_id: runtime_id.into(),
        agreement_key: key.public().into(),
        iat: chrono::Utc::now().timestamp(),
    };
    jws_sign(
        signing,
        &serde_json::to_vec(&payload).expect("payload serializes"),
    )
}

/// Check a key statement against the `did:key` it names. A statement
/// cannot be made by anyone but that runtime, whoever relays it.
///
/// # Errors
///
/// It is not a statement, or that runtime did not sign it.
pub fn verify_key_statement(compact: &str) -> Result<KeyStatement, String> {
    let claimed: KeyStatement = unverified_payload(compact)?;
    let statement: KeyStatement = verified_by(&claimed.runtime_id, compact)?;
    if statement.v != KEY_STATEMENT_VERSION {
        return Err(format!(
            "key statement version must be {KEY_STATEMENT_VERSION:?}"
        ));
    }
    URL_SAFE_NO_PAD
        .decode(&statement.agreement_key)
        .ok()
        .filter(|b| b.len() == 32)
        .ok_or_else(|| "agreement key is not 32 bytes of base64url".to_string())?;
    Ok(statement)
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// The associated data of a frame: its header fields that are present, in
/// [`HEADER_FIELDS`] order, one `name=<json>` per line.
#[must_use]
pub fn header_aad(frame: &Map<String, Value>) -> Vec<u8> {
    let mut out = String::new();
    for name in HEADER_FIELDS {
        if let Some(value) = frame.get(*name) {
            out.push_str(name);
            out.push('=');
            out.push_str(&value.to_string());
            out.push('\n');
        }
    }
    out.into_bytes()
}

/// Where the sender's account's device list ends (ADR-111 D2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderDevices {
    /// `user:hub:<id>`.
    pub account: String,
    pub seq: u64,
    pub hash: String,
}

/// What [`open_frame`] found in a sealed body besides the message.
#[derive(Debug, Default)]
pub struct Opened {
    /// The sender's key statement.
    pub sender: Option<String>,
    /// Where the sender's account's device list ends.
    pub devices: Option<SenderDevices>,
    /// The frame's place in its sender's count to this computer
    /// (ADR-125 D6), on channel events and invites.
    pub seq: Option<Seq>,
}

/// Seal `frame` to `recipient` (an X25519 key): the header stays, the
/// rest (with `sender_statement` and `devices`, when given) goes into
/// `sealed`.
///
/// # Errors
///
/// `recipient` is not an X25519 key.
pub fn seal_frame(
    frame: Map<String, Value>,
    recipient: &str,
    sender_statement: Option<&str>,
    devices: Option<&SenderDevices>,
) -> Result<Map<String, Value>, String> {
    let mut header = Map::new();
    let mut body = Map::new();
    for (name, value) in frame {
        if HEADER_FIELDS.contains(&name.as_str()) {
            header.insert(name, value);
        } else {
            body.insert(name, value);
        }
    }
    if let Some(statement) = sender_statement {
        body.insert(SENDER_KEY_FIELD.into(), Value::String(statement.into()));
    }
    if let Some(devices) = devices {
        body.insert(
            SENDER_DEVICES_FIELD.into(),
            serde_json::to_value(devices).expect("a head serializes"),
        );
    }
    let plaintext = serde_json::to_vec(&body).expect("body serializes");
    let sealed = seal_to(recipient, SEAL_INFO, &plaintext, &header_aad(&header))?;
    header.insert("sealed".into(), Value::String(sealed));
    Ok(header)
}

/// Open a sealed frame with `key`. Returns the whole frame, and the
/// sender's key statement and device-list head when the body carried them.
///
/// # Errors
///
/// It is unsealed, carries a field outside the header, does not open with
/// `key`, or its body repeats a header field.
pub fn open_frame(
    mut frame: Map<String, Value>,
    key: &AgreementKey,
) -> Result<(Map<String, Value>, Opened), String> {
    let sealed = match frame.remove("sealed") {
        Some(Value::String(s)) => s,
        _ => return Err("not sealed".into()),
    };
    if let Some(extra) = frame
        .keys()
        .find(|name| !HEADER_FIELDS.contains(&name.as_str()))
    {
        return Err(format!("readable field {extra:?} outside the header"));
    }
    let plaintext = key.open(&sealed, SEAL_INFO, &header_aad(&frame))?;
    let body: Map<String, Value> =
        serde_json::from_slice(&plaintext).map_err(|e| format!("sealed body: {e}"))?;
    let mut found = Opened::default();
    for (name, value) in body {
        if name == SENDER_KEY_FIELD {
            found.sender = value.as_str().map(str::to_owned);
            continue;
        }
        if name == SENDER_DEVICES_FIELD {
            found.devices = serde_json::from_value(value).ok();
            continue;
        }
        if name == SEQ_FIELD {
            found.seq = serde_json::from_value(value).ok();
            continue;
        }
        if frame.contains_key(&name) {
            return Err(format!("sealed body repeats header field {name:?}"));
        }
        frame.insert(name, value);
    }
    Ok((frame, found))
}

// ---------------------------------------------------------------------------
// The sealer
// ---------------------------------------------------------------------------

/// What the sealer needs to know about this computer (D4, D7).
#[async_trait::async_trait]
pub trait SealPolicy: Send + Sync {
    /// The Hub account of this computer's local user (the bare id).
    async fn own_account(&self) -> Option<String>;
    /// A phone's agreement key, by its Hub id (ADR-108 D2, ADR-111 D5).
    fn device_key(&self, device_id: &str) -> Option<String>;
}

/// Who a message waits for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Want {
    /// A runtime's key statement.
    Runtime(String),
    /// An account's device list (`user:hub:<id>`, ADR-111 D5).
    Account(String),
}

struct Parked {
    msg: TunnelMessage,
    at: Instant,
}

#[derive(Default)]
struct Pending {
    waiting: HashMap<Want, Vec<Parked>>,
    /// Requests in flight, by request id.
    asked: HashMap<String, (Want, Instant)>,
}

/// `<runtime>/sealing.json`: agreement keys learned from verified key
/// statements. Which computers are whose is the device lists' to say
/// (ADR-111 D5).
#[derive(Debug, Default, Serialize, Deserialize)]
struct SealingFile {
    /// Runtime id → agreement key (base64url), from verified statements.
    #[serde(default)]
    keys: BTreeMap<String, String>,
}

/// What [`Sealer::inbound`] made of a frame.
#[derive(Debug)]
pub enum Inbound {
    /// A message to handle, opened if it was sealed.
    Message(Box<TunnelMessage>),
    /// A message to handle, and requests to send first (a device list the
    /// envelope showed is newer than the one held).
    MessageAnd(Box<TunnelMessage>, Vec<TunnelMessage>),
    /// Keys arrived: send these again, they can be sealed now.
    Release(Vec<TunnelMessage>),
    /// Nothing to handle (refused, or not understood).
    Ignore,
}

/// One frame [`Sealer::outbound_routed`] made, and where it goes.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub bytes: Vec<u8>,
    /// Its `type`.
    pub ty: String,
    /// The computer it is sealed to (`recipientRuntimeId`); `None` for a
    /// frame to PekoHub itself or to a phone.
    pub to: Option<String>,
    /// Whether it is sealed.
    pub sealed: bool,
}

/// What [`Sealer::inbound_meta`] read of a frame besides the message.
#[derive(Debug, Default, Clone)]
pub struct InboundMeta {
    /// Its `type`.
    pub ty: String,
    /// Its header's `sourceRuntimeId`.
    pub source: Option<String>,
    /// Its `seq`, when it carried one (ADR-125 D6).
    pub seq: Option<Seq>,
}

/// Seals what this runtime sends and opens what it receives (ADR-110).
pub struct Sealer {
    runtime_id: String,
    key: AgreementKey,
    statement: String,
    policy: Arc<dyn SealPolicy>,
    devices: Arc<DeviceLists>,
    path: Option<PathBuf>,
    file: Mutex<SealingFile>,
    pending: Mutex<Pending>,
    /// When each contact's device list was last fetched.
    fetched: Mutex<HashMap<String, Instant>>,
    /// When a newer head of each account was last asked about.
    heads_asked: Mutex<HashMap<String, Instant>>,
    /// ADR-125 D6: numbers channel events and invites per computer.
    sequencer: Sequencer,
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sealer")
            .field("runtime_id", &self.runtime_id)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Sealer {
    /// A sealer for `runtime_id`, keeping the keys it learns at `path`
    /// (`None`: in memory only), sealing to the computers `devices` lists.
    #[must_use]
    pub fn new(
        runtime_id: String,
        signing: Arc<SigningKey>,
        policy: Arc<dyn SealPolicy>,
        devices: Arc<DeviceLists>,
        path: Option<PathBuf>,
    ) -> Self {
        let key = AgreementKey::derive(&signing);
        let statement = sign_key_statement(&signing, &runtime_id, &key);
        let mut file: SealingFile = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        file.keys
            .insert(runtime_id.clone(), key.public().to_string());
        Self {
            runtime_id,
            key,
            statement,
            policy,
            devices,
            path,
            file: Mutex::new(file),
            pending: Mutex::new(Pending::default()),
            fetched: Mutex::new(HashMap::new()),
            heads_asked: Mutex::new(HashMap::new()),
            sequencer: Sequencer::new(),
        }
    }

    /// This runtime's key statement, for `runtime_hello`.
    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }

    /// This runtime's agreement key.
    #[must_use]
    pub fn agreement_key(&self) -> &AgreementKey {
        &self.key
    }

    fn known_key(&self, runtime: &str) -> Option<String> {
        self.file.lock().unwrap().keys.get(runtime).cloned()
    }

    /// `runtime`'s agreement key, when its statement was learned.
    #[must_use]
    pub fn known_key_of(&self, runtime: &str) -> Option<String> {
        self.known_key(runtime)
    }

    /// Every other runtime whose key statement this computer holds: the
    /// computers it seals to (ADR-125 D4).
    #[must_use]
    pub fn known_runtimes(&self) -> Vec<String> {
        self.file
            .lock()
            .unwrap()
            .keys
            .keys()
            .filter(|r| **r != self.runtime_id)
            .cloned()
            .collect()
    }

    fn learn(&self, statement: &str) -> Option<KeyStatement> {
        match verify_key_statement(statement) {
            Ok(s) => {
                let mut file = self.file.lock().unwrap();
                if file.keys.get(&s.runtime_id) != Some(&s.agreement_key) {
                    file.keys
                        .insert(s.runtime_id.clone(), s.agreement_key.clone());
                    self.persist(&file);
                }
                Some(s)
            }
            Err(e) => {
                warn!("ignoring a key statement that does not verify: {e}");
                None
            }
        }
    }

    fn persist(&self, file: &SealingFile) {
        let Some(path) = &self.path else { return };
        let bytes = serde_json::to_vec_pretty(file).expect("sealing file serializes");
        if let Err(e) = crate::security::computers::write_private(path, &bytes) {
            warn!(path = %path.display(), "could not save sealing keys: {e}");
        }
    }

    async fn own_devices(&self) -> Option<SenderDevices> {
        let account = format!("user:hub:{}", self.policy.own_account().await?);
        let Head { seq, hash } = self.devices.head(&account)?;
        Some(SenderDevices { account, seq, hash })
    }

    /// Seal `msg` for the wire. Returns the frames to write now: the
    /// sealed message (one per computer for an account), and any request
    /// it is waiting on. A message of a sealed type is never written
    /// unsealed (D5).
    pub async fn outbound(&self, msg: TunnelMessage) -> Vec<Vec<u8>> {
        self.outbound_routed(msg)
            .await
            .into_iter()
            .map(|f| f.bytes)
            .collect()
    }

    /// [`Self::outbound`], saying of each frame which computer it is
    /// sealed to, so a router can choose its path (ADR-125 D6).
    pub async fn outbound_routed(&self, msg: TunnelMessage) -> Vec<Outgoing> {
        let value = match serde_json::to_value(&msg) {
            Ok(Value::Object(map)) => map,
            Ok(_) | Err(_) => return Vec::new(),
        };
        let ty = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !is_sealed_type(&ty) {
            return serde_json::to_vec(&value)
                .map(|bytes| {
                    vec![Outgoing {
                        bytes,
                        ty,
                        to: None,
                        sealed: false,
                    }]
                })
                .unwrap_or_default();
        }
        self.sweep();
        let devices = self.own_devices().await;
        if is_device_type(&ty) {
            let device = value.get("deviceId").and_then(Value::as_str).unwrap_or("");
            let Some(key) = self.policy.device_key(device) else {
                warn!(device, %ty, "no phone with that id on this account's list; not sent");
                return Vec::new();
            };
            return self.sealed_bytes(value, &key, false, devices.as_ref());
        }
        if let Some(recipient) = value.get("recipientRuntimeId").and_then(Value::as_str) {
            let recipient = recipient.to_string();
            if let Some(key) = self.known_key(&recipient) {
                return self.sealed_bytes(value, &key, true, devices.as_ref());
            }
            return self.park(Want::Runtime(recipient), msg);
        }
        let account = value
            .get("recipientAccount")
            .or_else(|| value.get("sourcePrincipalDid"))
            .and_then(Value::as_str)
            .filter(|a| a.starts_with("user:hub:"))
            .map(str::to_owned);
        let Some(account) = account else {
            warn!(%ty, "a sealed envelope with no recipient; not sent");
            return Vec::new();
        };
        let own = self
            .policy
            .own_account()
            .await
            .map(|a| format!("user:hub:{a}"));
        let fresh = own.as_deref() == Some(account.as_str())
            || self
                .fetched
                .lock()
                .unwrap()
                .get(&account)
                .is_some_and(|at| at.elapsed() < ACCOUNT_LIST_FOR);
        let Some(list) = self.devices.list(&account).filter(|_| fresh) else {
            return self.park(Want::Account(account), msg);
        };
        let mut frames = Vec::new();
        for computer in list.computers().filter(|d| d.id != self.runtime_id) {
            // The computer's own statement when this one learned it; the
            // key its list entry carries otherwise (both are its word).
            let key = self
                .known_key(&computer.id)
                .unwrap_or_else(|| computer.agreement_key.clone());
            let mut copy = value.clone();
            copy.insert(
                "recipientRuntimeId".into(),
                Value::String(computer.id.clone()),
            );
            frames.extend(self.sealed_bytes(copy, &key, true, devices.as_ref()));
        }
        if frames.is_empty() {
            debug!(%account, %ty, "no other computer on the account's list; not sent");
        }
        frames
    }

    fn sealed_bytes(
        &self,
        mut frame: Map<String, Value>,
        key: &str,
        with_sender: bool,
        devices: Option<&SenderDevices>,
    ) -> Vec<Outgoing> {
        let sender = with_sender.then_some(self.statement.as_str());
        let ty = frame
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let to = frame
            .get("recipientRuntimeId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let (true, Some(to), Some(request_id)) = (
            is_ordered_type(&ty),
            to.as_deref(),
            frame.get("requestId").and_then(Value::as_str),
        ) {
            let seq = self.sequencer.stamp(to, request_id);
            frame.insert(
                SEQ_FIELD.into(),
                serde_json::to_value(seq).expect("a seq serializes"),
            );
        }
        match seal_frame(frame, key, sender, devices) {
            Ok(sealed) => serde_json::to_vec(&sealed)
                .map(|bytes| {
                    vec![Outgoing {
                        bytes,
                        ty,
                        to,
                        sealed: true,
                    }]
                })
                .unwrap_or_default(),
            Err(e) => {
                warn!("could not seal an envelope: {e}; not sent");
                Vec::new()
            }
        }
    }

    fn park(&self, want: Want, msg: TunnelMessage) -> Vec<Outgoing> {
        let mut pending = self.pending.lock().unwrap();
        pending
            .waiting
            .entry(want.clone())
            .or_default()
            .push(Parked {
                msg,
                at: Instant::now(),
            });
        let asked = pending
            .asked
            .values()
            .any(|(w, at)| w == &want && at.elapsed() < ASK_AGAIN_AFTER);
        if asked {
            return Vec::new();
        }
        let request = match &want {
            Want::Runtime(runtime) => {
                let request_id = format!("keys_{}", uuid::Uuid::new_v4().simple());
                pending
                    .asked
                    .insert(request_id.clone(), (want.clone(), Instant::now()));
                TunnelMessage::RuntimeKeysRequest {
                    request_id,
                    runtime_ids: vec![runtime.clone()],
                    account: None,
                }
            }
            Want::Account(account) => {
                let request_id = format!("devices_{}", uuid::Uuid::new_v4().simple());
                pending
                    .asked
                    .insert(request_id.clone(), (want.clone(), Instant::now()));
                TunnelMessage::DeviceListRequest {
                    request_id,
                    account: account.clone(),
                    after: None,
                }
            }
        };
        let ty = match &request {
            TunnelMessage::RuntimeKeysRequest { .. } => "runtime_keys_request",
            _ => "device_list_request",
        };
        request
            .to_bytes()
            .map(|bytes| {
                vec![Outgoing {
                    bytes,
                    ty: ty.into(),
                    to: None,
                    sealed: false,
                }]
            })
            .unwrap_or_default()
    }

    fn sweep(&self) {
        let mut pending = self.pending.lock().unwrap();
        for (want, parked) in &mut pending.waiting {
            let before = parked.len();
            parked.retain(|p| p.at.elapsed() < PARK_FOR);
            if parked.len() < before {
                warn!(
                    ?want,
                    dropped = before - parked.len(),
                    "nothing arrived to seal these envelopes to; not sent"
                );
            }
        }
        pending.waiting.retain(|_, v| !v.is_empty());
        pending.asked.retain(|_, (_, at)| at.elapsed() < PARK_FOR);
    }

    /// `account`'s device list was just taken (ADR-111): the envelopes
    /// waiting for it, to send again now.
    pub fn release_account(&self, account: &str) -> Vec<TunnelMessage> {
        self.fetched
            .lock()
            .unwrap()
            .insert(account.to_string(), Instant::now());
        let mut pending = self.pending.lock().unwrap();
        pending
            .asked
            .retain(|_, (w, _)| w != &Want::Account(account.to_string()));
        pending
            .waiting
            .remove(&Want::Account(account.to_string()))
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.at.elapsed() < PARK_FOR)
            .map(|p| p.msg)
            .collect()
    }

    /// A request for what follows the held list of `account`, when an
    /// envelope showed a head this computer does not hold.
    fn behind(&self, devices: &SenderDevices) -> Option<TunnelMessage> {
        let held = self.devices.head(&devices.account);
        if held.as_ref().is_some_and(|h| h.seq >= devices.seq) {
            return None;
        }
        let mut asked = self.heads_asked.lock().unwrap();
        if asked
            .get(&devices.account)
            .is_some_and(|at| at.elapsed() < HEAD_ASK_EVERY)
        {
            return None;
        }
        asked.insert(devices.account.clone(), Instant::now());
        Some(TunnelMessage::DeviceListRequest {
            request_id: format!("devices_{}", uuid::Uuid::new_v4().simple()),
            account: devices.account.clone(),
            after: held.map(|h| h.seq),
        })
    }

    /// Read a frame from the wire: open it if it is sealed, take in keys
    /// the Hub sent, and refuse anything of a sealed type that is not
    /// sealed to this computer (D5).
    pub async fn inbound(&self, bytes: &[u8]) -> Inbound {
        self.inbound_meta(bytes).await.0
    }

    /// [`Self::inbound`], with the frame's type, source and `seq`
    /// (ADR-125 D6).
    pub async fn inbound_meta(&self, bytes: &[u8]) -> (Inbound, InboundMeta) {
        let Ok(Value::Object(frame)) = serde_json::from_slice::<Value>(bytes) else {
            warn!("ignoring a tunnel frame that is not a JSON object");
            return (Inbound::Ignore, InboundMeta::default());
        };
        let ty = frame
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut meta = InboundMeta {
            ty: ty.clone(),
            source: frame
                .get("sourceRuntimeId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            seq: None,
        };
        let inbound = self.inbound_frame(frame, ty, &mut meta).await;
        (inbound, meta)
    }

    async fn inbound_frame(
        &self,
        frame: Map<String, Value>,
        ty: String,
        meta: &mut InboundMeta,
    ) -> Inbound {
        if ty == "runtime_keys" {
            return match serde_json::from_value::<TunnelMessage>(Value::Object(frame)) {
                Ok(TunnelMessage::RuntimeKeys {
                    request_id, keys, ..
                }) => Inbound::Release(self.keys_arrived(&request_id, keys)),
                _ => Inbound::Ignore,
            };
        }
        if !is_sealed_type(&ty) {
            return match serde_json::from_value(Value::Object(frame)) {
                Ok(msg) => Inbound::Message(Box::new(msg)),
                Err(e) => {
                    warn!(%ty, "failed to parse tunnel message: {e}");
                    Inbound::Ignore
                }
            };
        }
        let addressed = if is_device_type(&ty) {
            frame.get("runtimeId")
        } else {
            frame.get("recipientRuntimeId")
        }
        .and_then(Value::as_str);
        if addressed != Some(self.runtime_id.as_str()) {
            warn!(%ty, "refusing an envelope not addressed to this computer");
            return Inbound::Ignore;
        }
        let source = frame
            .get("sourceRuntimeId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let (opened, found) = match open_frame(frame, &self.key) {
            Ok(opened) => opened,
            Err(e) => {
                warn!(%ty, "refusing an envelope: {e}");
                return Inbound::Ignore;
            }
        };
        meta.seq = found.seq.clone();
        if let (Some(statement), Some(source)) = (found.sender, source) {
            if let Some(s) = self.learn(&statement) {
                if s.runtime_id != source {
                    warn!(%ty, "an envelope carried another runtime's key statement");
                }
            }
        }
        let ask = found.devices.as_ref().and_then(|d| self.behind(d));
        match serde_json::from_value(Value::Object(opened)) {
            Ok(msg) => match ask {
                Some(ask) => Inbound::MessageAnd(Box::new(msg), vec![ask]),
                None => Inbound::Message(Box::new(msg)),
            },
            Err(e) => {
                warn!(%ty, "failed to parse an opened envelope: {e}");
                Inbound::Ignore
            }
        }
    }

    fn keys_arrived(&self, request_id: &str, keys: Vec<RuntimeKeyEntry>) -> Vec<TunnelMessage> {
        for entry in keys {
            match self.learn(&entry.statement) {
                Some(s) if s.runtime_id == entry.runtime_id => {}
                Some(_) => {
                    warn!(runtime = %entry.runtime_id, "the Hub served another runtime's key statement")
                }
                None => {}
            }
        }
        let mut pending = self.pending.lock().unwrap();
        let Some((want, _)) = pending.asked.remove(request_id) else {
            return Vec::new();
        };
        let released: Vec<TunnelMessage> = pending
            .waiting
            .remove(&want)
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.msg)
            .collect();
        if let Want::Runtime(runtime) = &want {
            if self.known_key(runtime).is_none() {
                warn!(%runtime, dropped = released.len(), "PekoHub has no key for this computer; not sent");
                return Vec::new();
            }
        }
        released
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::device_list::{self, Change, Device, DeviceKind, Link};
    use crate::tunnel::verifying_key_to_did_key;

    fn runtime() -> (Arc<SigningKey>, String) {
        let key = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
        let did = verifying_key_to_did_key(&key.verifying_key());
        (Arc::new(key), did)
    }

    #[derive(Default)]
    struct Policy {
        own: Option<String>,
        devices: Mutex<HashMap<String, String>>,
    }

    #[async_trait::async_trait]
    impl SealPolicy for Policy {
        async fn own_account(&self) -> Option<String> {
            self.own.clone()
        }
        fn device_key(&self, device_id: &str) -> Option<String> {
            self.devices.lock().unwrap().get(device_id).cloned()
        }
    }

    fn sealer(policy: Policy) -> (Sealer, String) {
        let (s, did, _, _) = sealer_with(policy);
        (s, did)
    }

    /// A sealer, its runtime id and key, and the device lists it reads.
    fn sealer_with(policy: Policy) -> (Sealer, String, Arc<SigningKey>, Arc<DeviceLists>) {
        let (key, did) = runtime();
        let lists = Arc::new(DeviceLists::new(None));
        (
            Sealer::new(
                did.clone(),
                Arc::clone(&key),
                Arc::new(policy),
                Arc::clone(&lists),
                None,
            ),
            did,
            key,
            lists,
        )
    }

    /// `account`'s list with these computers, the first one adding the rest.
    fn chain(account: &str, computers: &[(&Arc<SigningKey>, &Sealer)]) -> Vec<Link> {
        let (first, first_sealer) = computers[0];
        let mut list = device_list::DeviceList::verify(
            account,
            vec![device_list::genesis(
                first,
                account,
                DeviceKind::Computer,
                None,
                first_sealer.agreement_key().public(),
            )],
        )
        .unwrap();
        for (key, s) in &computers[1..] {
            let payload = list.next_payload(Change::Add {
                device: Device {
                    id: device_list::did_of(key),
                    kind: DeviceKind::Computer,
                    name: None,
                    agreement_key: s.agreement_key().public().into(),
                    hub_device_id: None,
                },
            });
            list = list
                .extended(vec![Link {
                    by: device_list::did_of(first),
                    signed: device_list::sign_payload(first, &payload),
                    joiner: Some(device_list::sign_payload(key, &payload)),
                }])
                .unwrap();
        }
        list.links().to_vec()
    }

    fn event(from: &str, to: &str) -> TunnelMessage {
        TunnelMessage::TunnelPekoModelsRequest {
            request_id: "r1".into(),
            source_runtime_id: from.into(),
            recipient_runtime_id: to.into(),
            source_principal_did: "user:hub:alice".into(),
            principal_did: "did:key:zpeko".into(),
            signature: "secret-bearing-signature".into(),
        }
    }

    fn frame(bytes: &[u8]) -> Map<String, Value> {
        serde_json::from_slice(bytes).unwrap()
    }

    /// A frame this runtime seals to a phone, for peko-mobile's tests of
    /// its own opening (`tests/fixtures/sealed-response.json` there):
    /// `PEKO_SEAL_FIXTURE=<path> cargo test -p peko --lib
    /// tunnel::seal::tests::write_mobile_fixture -- --ignored`.
    #[test]
    #[ignore = "writes a fixture for peko-mobile"]
    fn write_mobile_fixture() {
        let Some(path) = std::env::var_os("PEKO_SEAL_FIXTURE") else {
            return;
        };
        let runtime = SigningKey::from_bytes(&[7u8; 32]);
        let runtime_id = verifying_key_to_did_key(&runtime.verifying_key());
        let phone_secret = [9u8; 32];
        let phone = <Kem as KemTrait>::sk_to_pk(
            &<Kem as KemTrait>::PrivateKey::from_bytes(&phone_secret).unwrap(),
        );
        let phone_public = URL_SAFE_NO_PAD.encode(phone.to_bytes());
        let device_id = "dev_fixture0000000000000000";
        let signature = crate::tunnel::devices::sign_response(
            &runtime,
            "req_fixture",
            device_id,
            &runtime_id,
            Ok(serde_json::json!({"conversations": [{"name": "the plans"}]})),
        );
        let frame = serde_json::to_value(TunnelMessage::TunnelDeviceResponse {
            request_id: "req_fixture".into(),
            device_id: device_id.into(),
            runtime_id: runtime_id.clone(),
            signature,
        })
        .unwrap();
        let Value::Object(frame) = frame else {
            unreachable!()
        };
        let sealed = seal_frame(frame, &phone_public, None, None).unwrap();
        let key = AgreementKey::derive(&runtime);
        let fixture = serde_json::json!({
            "deviceId": device_id,
            "agree": URL_SAFE_NO_PAD.encode(phone_secret),
            "runtimeId": runtime_id,
            "statement": sign_key_statement(&runtime, &runtime_id, &key),
            "frame": sealed,
        });
        std::fs::write(path, serde_json::to_string_pretty(&fixture).unwrap() + "\n").unwrap();
    }

    /// A request peko-mobile sealed to the fixture runtime (`[7; 32]`):
    /// the two implementations agree.
    #[test]
    fn opens_a_request_a_phone_sealed() {
        let frame: Map<String, Value> =
            serde_json::from_str(include_str!("fixtures/phone_sealed_request.json")).unwrap();
        let runtime = AgreementKey::derive(&SigningKey::from_bytes(&[7u8; 32]));
        let (opened, found) = open_frame(frame, &runtime).unwrap();
        assert_eq!(opened["type"], "tunnel_device_request");
        assert_eq!(opened["signature"], "a phone's signed request");
        assert!(found.sender.is_none());
    }

    #[test]
    fn the_agreement_key_is_stable_and_not_the_signing_key() {
        let (key, _) = runtime();
        let a = AgreementKey::derive(&key);
        let b = AgreementKey::derive(&key);
        assert_eq!(a.public(), b.public());
        assert_ne!(
            URL_SAFE_NO_PAD.decode(a.public()).unwrap(),
            key.verifying_key().to_bytes().to_vec()
        );
    }

    #[test]
    fn a_key_statement_verifies_only_for_its_runtime() {
        let (key, did) = runtime();
        let agreement = AgreementKey::derive(&key);
        let statement = sign_key_statement(&key, &did, &agreement);
        let s = verify_key_statement(&statement).unwrap();
        assert_eq!(s.runtime_id, did);
        assert_eq!(s.agreement_key, agreement.public());

        // Someone else signing a statement naming this runtime is refused.
        let (other, _) = runtime();
        let forged = sign_key_statement(&other, &did, &AgreementKey::derive(&other));
        assert!(verify_key_statement(&forged).is_err());
    }

    #[test]
    fn a_frame_keeps_only_its_header_readable() {
        let (key, _) = runtime();
        let recipient = AgreementKey::derive(&key);
        let mut frame = Map::new();
        frame.insert("type".into(), "tunnel_channel_event".into());
        frame.insert("recipientRuntimeId".into(), "did:key:zr".into());
        frame.insert("event".into(), serde_json::json!({"text": "the plans"}));
        frame.insert("signature".into(), "jws".into());
        let sealed = seal_frame(frame.clone(), recipient.public(), Some("stmt"), None).unwrap();
        let wire = serde_json::to_string(&sealed).unwrap();
        assert!(!wire.contains("the plans"));
        assert!(!wire.contains("jws"));
        assert_eq!(sealed.len(), 3);

        let (opened, found) = open_frame(sealed, &recipient).unwrap();
        assert_eq!(opened, frame);
        assert_eq!(found.sender.as_deref(), Some("stmt"));
    }

    #[test]
    fn a_changed_header_does_not_open() {
        let (key, _) = runtime();
        let recipient = AgreementKey::derive(&key);
        let mut frame = Map::new();
        frame.insert("type".into(), "tunnel_channel_event".into());
        frame.insert("channelId".into(), "chan_a".into());
        frame.insert("event".into(), "x".into());
        let mut sealed = seal_frame(frame, recipient.public(), None, None).unwrap();
        sealed.insert("channelId".into(), "chan_b".into());
        assert!(open_frame(sealed.clone(), &recipient).is_err());

        let mut extra = sealed;
        extra.insert("channelId".into(), "chan_a".into());
        extra.insert("event".into(), "injected".into());
        assert!(open_frame(extra, &recipient).is_err());
    }

    #[tokio::test]
    async fn a_message_waits_for_its_recipients_key_then_goes_sealed() {
        let (alice, alice_id) = sealer(Policy::default());
        let (bob, bob_id) = sealer(Policy::default());

        // No key yet: the message waits, and a key request goes out.
        let frames = alice.outbound(event(&alice_id, &bob_id)).await;
        assert_eq!(frames.len(), 1);
        let request = frame(&frames[0]);
        assert_eq!(request["type"], "runtime_keys_request");
        let request_id = request["requestId"].as_str().unwrap().to_string();

        // The Hub answers with Bob's statement: the message is released.
        let answer = serde_json::to_vec(&TunnelMessage::RuntimeKeys {
            request_id,
            account: None,
            keys: vec![RuntimeKeyEntry {
                runtime_id: bob_id.clone(),
                statement: bob.statement().to_string(),
            }],
        })
        .unwrap();
        let Inbound::Release(released) = alice.inbound(&answer).await else {
            panic!("keys should release the message");
        };
        assert_eq!(released.len(), 1);

        let frames = alice.outbound(released.into_iter().next().unwrap()).await;
        assert_eq!(frames.len(), 1);
        let wire = String::from_utf8(frames[0].clone()).unwrap();
        assert!(!wire.contains("secret-bearing-signature"));
        assert!(wire.contains("\"sealed\""));

        // Bob opens it, and learns Alice's key from it.
        let Inbound::Message(msg) = bob.inbound(&frames[0]).await else {
            panic!("Bob should open it");
        };
        assert_eq!(
            serde_json::to_value(&*msg).unwrap(),
            serde_json::to_value(event(&alice_id, &bob_id)).unwrap()
        );
        assert!(bob.known_key(&alice_id).is_some());
    }

    #[tokio::test]
    async fn an_unsealed_envelope_of_a_sealed_type_is_refused() {
        let (bob, bob_id) = sealer(Policy::default());
        let plain = serde_json::to_vec(&event("did:key:za", &bob_id)).unwrap();
        assert!(matches!(bob.inbound(&plain).await, Inbound::Ignore));
    }

    #[tokio::test]
    async fn an_envelope_sealed_for_another_computer_is_refused() {
        let (alice, alice_id) = sealer(Policy::default());
        let (bob, bob_id) = sealer(Policy::default());
        let (carol, _) = sealer(Policy::default());
        alice.learn(bob.statement());
        let frames = alice.outbound(event(&alice_id, &bob_id)).await;
        assert!(matches!(carol.inbound(&frames[0]).await, Inbound::Ignore));
    }

    #[tokio::test]
    async fn hub_frames_are_not_sealed() {
        let (alice, _) = sealer(Policy::default());
        let frames = alice.outbound(TunnelMessage::Heartbeat { seq: 3 }).await;
        assert_eq!(frame(&frames[0])["type"], "heartbeat");
    }

    fn invitation(from: &str, account: &str) -> TunnelMessage {
        TunnelMessage::TunnelGroupInvitation {
            request_id: "inv".into(),
            source_runtime_id: from.into(),
            recipient_account: account.into(),
            channel_id: "chan_g".into(),
            name: "the plans".into(),
            inviter: "user:hub:alice".into(),
            initial_members: Vec::new(),
            signature: "sig".into(),
        }
    }

    fn recipients(frames: &[Vec<u8>]) -> Vec<String> {
        let mut r: Vec<String> = frames
            .iter()
            .map(|f| frame(f)["recipientRuntimeId"].as_str().unwrap().to_string())
            .collect();
        r.sort();
        r
    }

    #[tokio::test]
    async fn another_persons_computers_are_the_ones_on_their_device_list() {
        let (alice, alice_id, _, alice_lists) = sealer_with(Policy::default());
        let (b1, b1_id, b1_key, _) = sealer_with(Policy::default());
        let (b2, b2_id, b2_key, _) = sealer_with(Policy::default());
        let bob = "user:hub:bob";

        // No list held yet: the invitation waits, and the list is asked for.
        let frames = alice.outbound(invitation(&alice_id, bob)).await;
        assert_eq!(frames.len(), 1);
        let request = frame(&frames[0]);
        assert_eq!(request["type"], "device_list_request");
        assert_eq!(request["account"], bob);

        // The list arrives and is taken (the tunnel's device sync does
        // this): the invitation goes to each computer on it, sealed.
        alice_lists
            .accept(bob, chain(bob, &[(&b1_key, &b1), (&b2_key, &b2)]))
            .unwrap();
        let released = alice.release_account(bob);
        assert_eq!(released.len(), 1);
        let frames = alice.outbound(released[0].clone()).await;
        let mut want = vec![b1_id, b2_id];
        want.sort();
        assert_eq!(recipients(&frames), want);
        for f in &frames {
            let opener = if frame(f)["recipientRuntimeId"] == b1.runtime_id.as_str() {
                &b1
            } else {
                &b2
            };
            assert!(matches!(opener.inbound(f).await, Inbound::Message(_)));
        }

        // A computer PekoHub registers to Bob that is not on his list
        // gets nothing: the list, not the Hub, says which are his.
        let again = alice.outbound(invitation(&alice_id, bob)).await;
        assert_eq!(recipients(&again).len(), 2);
    }

    #[tokio::test]
    async fn own_account_messages_go_only_to_computers_on_the_list() {
        let policy = Policy {
            own: Some("alice".into()),
            ..Policy::default()
        };
        let (me, me_id, me_key, lists) = sealer_with(policy);
        let (c2, c2_id, c2_key, _) = sealer_with(Policy::default());
        lists
            .accept(
                "user:hub:alice",
                chain("user:hub:alice", &[(&me_key, &me), (&c2_key, &c2)]),
            )
            .unwrap();
        let mark = TunnelMessage::TunnelReadMark {
            request_id: "m".into(),
            source_runtime_id: me_id.clone(),
            source_principal_did: "user:hub:alice".into(),
            channel_id: "chan_x".into(),
            msg_id: "msg_1".into(),
            signature: "sig".into(),
        };
        let frames = me.outbound(mark).await;
        assert_eq!(recipients(&frames), vec![c2_id]);
        // Each copy says where alice's list ends, so a computer behind asks.
        let Inbound::MessageAnd(_, ask) = c2.inbound(&frames[0]).await else {
            panic!("c2 holds no list for alice: it should ask");
        };
        let TunnelMessage::DeviceListRequest { account, after, .. } = &ask[0] else {
            panic!("expected a device list request, got {ask:?}");
        };
        assert_eq!((account.as_str(), *after), ("user:hub:alice", None));
    }

    #[tokio::test]
    async fn phone_traffic_is_sealed_to_the_phone() {
        let phone = AgreementKey::derive(&SigningKey::from_bytes(&rand::random::<[u8; 32]>()));
        let policy = Policy::default();
        policy
            .devices
            .lock()
            .unwrap()
            .insert("dev_1".into(), phone.public().into());
        let (me, me_id) = sealer(policy);
        let frames = me
            .outbound(TunnelMessage::TunnelDeviceEvent {
                device_id: "dev_1".into(),
                runtime_id: me_id,
                signature: "what the phone shows".into(),
            })
            .await;
        let f = frame(&frames[0]);
        assert!(!serde_json::to_string(&f)
            .unwrap()
            .contains("what the phone shows"));
        let (opened, found) = open_frame(f, &phone).unwrap();
        assert_eq!(opened["signature"], "what the phone shows");
        assert!(found.sender.is_none());
    }
}
