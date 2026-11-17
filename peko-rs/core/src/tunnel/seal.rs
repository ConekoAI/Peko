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
//! computers of an account a message is sealed to follows D4: the
//! owner's own computers by their ADR-109 approval, another person's by
//! first use and then by a vouch from a computer already pinned.
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
use super::protocol::{RuntimeKeyEntry, TunnelMessage};
use super::tunnel_channel_signature::{jws_sign, jws_verify};
use crate::security::computers::ComputerStatus;

type Kem = X25519HkdfSha256;

/// HPKE `info` of a sealed envelope.
pub const SEAL_INFO: &str = "peko-tunnel-seal/1";
/// Version tag of a runtime's key statement.
pub const KEY_STATEMENT_VERSION: &str = "peko-runtime-key/1";
/// Version tag of a computer vouching for another of its account.
pub const VOUCH_VERSION: &str = "peko-computer-vouch/1";
/// Prefix of the input the agreement key is derived from, so it is never
/// the signing key (D2).
const AGREEMENT_KEY_LABEL: &[u8] = b"peko-agreement-key/1";
/// The field of a sealed body that carries the sender's key statement.
const SENDER_KEY_FIELD: &str = "senderKey";
/// How long a message waits for its recipient's key before it is dropped
/// (a post stays in the outbox and is sent again; ADR-072).
const PARK_FOR: Duration = Duration::from_secs(30);
/// How long a key request is left to answer before it is asked again.
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(10);
/// How long the Hub's list of an account's computers is used before it is
/// asked for again.
const ACCOUNT_LIST_FOR: Duration = Duration::from_mins(1);

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
        &mut rand::rngs::OsRng,
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

/// A computer's word that another computer belongs to its account (D4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vouch {
    pub v: String,
    /// `user:hub:<id>`.
    pub account: String,
    pub runtime_id: String,
    pub iat: i64,
}

/// Vouch, as this runtime, that `runtime` is one of `account`'s computers.
#[must_use]
pub fn sign_vouch(signing: &SigningKey, account: &str, runtime: &str) -> String {
    let payload = Vouch {
        v: VOUCH_VERSION.into(),
        account: account.into(),
        runtime_id: runtime.into(),
        iat: chrono::Utc::now().timestamp(),
    };
    jws_sign(
        signing,
        &serde_json::to_vec(&payload).expect("payload serializes"),
    )
}

/// Check that `voucher` vouched for a computer.
///
/// # Errors
///
/// `voucher` did not sign it, or it is not a vouch.
pub fn verify_vouch(voucher: &str, compact: &str) -> Result<Vouch, String> {
    let vouch: Vouch = verified_by(voucher, compact)?;
    if vouch.v != VOUCH_VERSION {
        return Err(format!("vouch version must be {VOUCH_VERSION:?}"));
    }
    Ok(vouch)
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

/// Seal `frame` to `recipient` (an X25519 key): the header stays, the
/// rest (with `sender_statement`, when given) goes into `sealed`.
///
/// # Errors
///
/// `recipient` is not an X25519 key.
pub fn seal_frame(
    frame: Map<String, Value>,
    recipient: &str,
    sender_statement: Option<&str>,
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
    let plaintext = serde_json::to_vec(&body).expect("body serializes");
    let sealed = seal_to(recipient, SEAL_INFO, &plaintext, &header_aad(&header))?;
    header.insert("sealed".into(), Value::String(sealed));
    Ok(header)
}

/// Open a sealed frame with `key`. Returns the whole frame and the
/// sender's key statement, when the body carried one.
///
/// # Errors
///
/// It is unsealed, carries a field outside the header, does not open with
/// `key`, or its body repeats a header field.
pub fn open_frame(
    mut frame: Map<String, Value>,
    key: &AgreementKey,
) -> Result<(Map<String, Value>, Option<String>), String> {
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
    let mut sender = None;
    for (name, value) in body {
        if name == SENDER_KEY_FIELD {
            sender = value.as_str().map(str::to_owned);
            continue;
        }
        if frame.contains_key(&name) {
            return Err(format!("sealed body repeats header field {name:?}"));
        }
        frame.insert(name, value);
    }
    Ok((frame, sender))
}

// ---------------------------------------------------------------------------
// The sealer
// ---------------------------------------------------------------------------

/// What the sealer needs to know about this computer (D4, D7).
#[async_trait::async_trait]
pub trait SealPolicy: Send + Sync {
    /// The Hub account of this computer's local user (the bare id).
    async fn own_account(&self) -> Option<String>;
    /// Where `runtime` stands among the owner's computers (ADR-109).
    fn own_computer(&self, account: &str, runtime: &str) -> Option<ComputerStatus>;
    /// A paired phone's agreement key (ADR-108 D2).
    fn device_key(&self, device_id: &str) -> Option<String>;
}

/// Who a message waits for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Want {
    Runtime(String),
    /// `user:hub:<id>`.
    Account(String),
}

struct Parked {
    msg: TunnelMessage,
    at: Instant,
}

#[derive(Default)]
struct Pending {
    waiting: HashMap<Want, Vec<Parked>>,
    /// Key requests in flight, by request id.
    asked: HashMap<String, (Want, Instant)>,
}

struct AccountList {
    at: Instant,
    entries: Vec<RuntimeKeyEntry>,
}

/// How a computer of another person's account came to be trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Pin {
    /// `first` (D4, first use) or `vouch:<runtime>`.
    via: String,
    at: chrono::DateTime<chrono::Utc>,
}

/// `<runtime>/sealing.json`: keys learned, and other people's computers
/// pinned.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SealingFile {
    /// Runtime id → agreement key (base64url), from verified statements.
    #[serde(default)]
    keys: BTreeMap<String, String>,
    /// `user:hub:<id>` → runtime id → how it was pinned.
    #[serde(default)]
    accounts: BTreeMap<String, BTreeMap<String, Pin>>,
}

/// What [`Sealer::inbound`] made of a frame.
#[derive(Debug)]
pub enum Inbound {
    /// A message to handle, opened if it was sealed.
    Message(Box<TunnelMessage>),
    /// Keys arrived: send these again, they can be sealed now.
    Release(Vec<TunnelMessage>),
    /// Nothing to handle (refused, or not understood).
    Ignore,
}

/// Seals what this runtime sends and opens what it receives (ADR-110).
pub struct Sealer {
    runtime_id: String,
    signing: Arc<SigningKey>,
    key: AgreementKey,
    statement: String,
    policy: Arc<dyn SealPolicy>,
    path: Option<PathBuf>,
    file: Mutex<SealingFile>,
    pending: Mutex<Pending>,
    lists: Mutex<HashMap<String, AccountList>>,
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
    /// A sealer for `runtime_id`, keeping what it learns at `path`
    /// (`None`: in memory only).
    #[must_use]
    pub fn new(
        runtime_id: String,
        signing: Arc<SigningKey>,
        policy: Arc<dyn SealPolicy>,
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
            signing,
            key,
            statement,
            policy,
            path,
            file: Mutex::new(file),
            pending: Mutex::new(Pending::default()),
            lists: Mutex::new(HashMap::new()),
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

    /// Vouches for every computer of this computer's account the owner
    /// approved here (D4), to publish to the Hub.
    pub fn vouches(
        &self,
        account: &str,
        approved: impl IntoIterator<Item = String>,
    ) -> Vec<String> {
        let account = format!("user:hub:{account}");
        approved
            .into_iter()
            .filter(|r| r != &self.runtime_id)
            .map(|r| sign_vouch(&self.signing, &account, &r))
            .collect()
    }

    fn known_key(&self, runtime: &str) -> Option<String> {
        self.file.lock().unwrap().keys.get(runtime).cloned()
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

    /// Seal `msg` for the wire. Returns the frames to write now: the
    /// sealed message (one per computer for an account), and any key
    /// request it is waiting on. A message of a sealed type is never
    /// written unsealed (D5).
    pub async fn outbound(&self, msg: TunnelMessage) -> Vec<Vec<u8>> {
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
                .map(|b| vec![b])
                .unwrap_or_default();
        }
        self.sweep();
        if is_device_type(&ty) {
            let device = value.get("deviceId").and_then(Value::as_str).unwrap_or("");
            let Some(key) = self.policy.device_key(device) else {
                warn!(device, %ty, "no paired phone with that id; not sent");
                return Vec::new();
            };
            return self.sealed_bytes(value, &key, false);
        }
        if let Some(recipient) = value.get("recipientRuntimeId").and_then(Value::as_str) {
            let recipient = recipient.to_string();
            if let Some(key) = self.known_key(&recipient) {
                return self.sealed_bytes(value, &key, true);
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
        let fresh = {
            let lists = self.lists.lock().unwrap();
            lists
                .get(&account)
                .filter(|l| l.at.elapsed() < ACCOUNT_LIST_FOR)
                .map(|l| l.entries.clone())
        };
        let Some(entries) = fresh else {
            return self.park(Want::Account(account), msg);
        };
        let mut frames = Vec::new();
        for runtime in self.computers_of(&account, &entries, &ty).await {
            let Some(key) = self.known_key(&runtime) else {
                continue;
            };
            let mut copy = value.clone();
            copy.insert("recipientRuntimeId".into(), Value::String(runtime));
            frames.extend(self.sealed_bytes(copy, &key, true));
        }
        if frames.is_empty() {
            debug!(%account, %ty, "no computer of the account to seal to; not sent");
        }
        frames
    }

    fn sealed_bytes(
        &self,
        frame: Map<String, Value>,
        key: &str,
        with_sender: bool,
    ) -> Vec<Vec<u8>> {
        let sender = with_sender.then_some(self.statement.as_str());
        match seal_frame(frame, key, sender) {
            Ok(sealed) => serde_json::to_vec(&sealed)
                .map(|b| vec![b])
                .unwrap_or_default(),
            Err(e) => {
                warn!("could not seal an envelope: {e}; not sent");
                Vec::new()
            }
        }
    }

    fn park(&self, want: Want, msg: TunnelMessage) -> Vec<Vec<u8>> {
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
        let request_id = format!("keys_{}", uuid::Uuid::new_v4().simple());
        pending
            .asked
            .insert(request_id.clone(), (want.clone(), Instant::now()));
        let request = match want {
            Want::Runtime(runtime) => TunnelMessage::RuntimeKeysRequest {
                request_id,
                runtime_ids: vec![runtime],
                account: None,
            },
            Want::Account(account) => TunnelMessage::RuntimeKeysRequest {
                request_id,
                runtime_ids: Vec::new(),
                account: Some(account),
            },
        };
        request.to_bytes().map(|b| vec![b]).unwrap_or_default()
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
                    "no key arrived for these envelopes; not sent"
                );
            }
        }
        pending.waiting.retain(|_, v| !v.is_empty());
        pending.asked.retain(|_, (_, at)| at.elapsed() < PARK_FOR);
    }

    /// The computers of `account` to seal a `ty` envelope to (D4).
    async fn computers_of(
        &self,
        account: &str,
        entries: &[RuntimeKeyEntry],
        ty: &str,
    ) -> Vec<String> {
        let listed: Vec<&RuntimeKeyEntry> = entries
            .iter()
            .filter(|e| e.runtime_id != self.runtime_id && self.known_key(&e.runtime_id).is_some())
            .collect();
        let own = self.policy.own_account().await;
        let bare = account.strip_prefix("user:hub:").unwrap_or(account);
        if own.as_deref() == Some(bare) {
            // The owner's own computers: approved ones (ADR-109). One not
            // approved yet may only be asked for the DM list, which carries
            // nothing; it answers nothing until approved anyway.
            return listed
                .into_iter()
                .filter(|e| match self.policy.own_computer(bare, &e.runtime_id) {
                    Some(ComputerStatus::Approved) => true,
                    Some(ComputerStatus::Denied) => false,
                    Some(ComputerStatus::Pending) | None => ty == "tunnel_account_dms_request",
                })
                .map(|e| e.runtime_id.clone())
                .collect();
        }
        let mut file = self.file.lock().unwrap();
        let pins = file.accounts.entry(account.to_string()).or_default();
        let mut changed = false;
        let now = chrono::Utc::now();
        if pins.is_empty() {
            // First use: the computers the account has now.
            for e in &listed {
                pins.insert(
                    e.runtime_id.clone(),
                    Pin {
                        via: "first".into(),
                        at: now,
                    },
                );
                changed = true;
            }
        } else {
            for e in &listed {
                if pins.contains_key(&e.runtime_id) {
                    continue;
                }
                let voucher = e.vouches.iter().find_map(|v| {
                    if !pins.contains_key(&v.by) {
                        return None;
                    }
                    let vouch = verify_vouch(&v.by, &v.statement).ok()?;
                    (vouch.account == account && vouch.runtime_id == e.runtime_id)
                        .then(|| v.by.clone())
                });
                if let Some(by) = voucher {
                    pins.insert(
                        e.runtime_id.clone(),
                        Pin {
                            via: format!("vouch:{by}"),
                            at: now,
                        },
                    );
                    changed = true;
                } else {
                    debug!(%account, runtime = %e.runtime_id, "a computer no pinned computer of the account vouches for; not sealed to");
                }
            }
        }
        let targets: Vec<String> = listed
            .iter()
            .filter(|e| pins.contains_key(&e.runtime_id))
            .map(|e| e.runtime_id.clone())
            .collect();
        if changed {
            self.persist(&file);
        }
        targets
    }

    /// Read a frame from the wire: open it if it is sealed, take in keys
    /// the Hub sent, and refuse anything of a sealed type that is not
    /// sealed to this computer (D5).
    pub async fn inbound(&self, bytes: &[u8]) -> Inbound {
        let Ok(Value::Object(frame)) = serde_json::from_slice::<Value>(bytes) else {
            warn!("ignoring a tunnel frame that is not a JSON object");
            return Inbound::Ignore;
        };
        let ty = frame
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if ty == "runtime_keys" {
            return match serde_json::from_value::<TunnelMessage>(Value::Object(frame)) {
                Ok(TunnelMessage::RuntimeKeys {
                    request_id,
                    account,
                    keys,
                }) => Inbound::Release(self.keys_arrived(&request_id, account, keys)),
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
        let (opened, sender) = match open_frame(frame, &self.key) {
            Ok(opened) => opened,
            Err(e) => {
                warn!(%ty, "refusing an envelope: {e}");
                return Inbound::Ignore;
            }
        };
        if let (Some(statement), Some(source)) = (sender, source) {
            if let Some(s) = self.learn(&statement) {
                if s.runtime_id != source {
                    warn!(%ty, "an envelope carried another runtime's key statement");
                }
            }
        }
        match serde_json::from_value(Value::Object(opened)) {
            Ok(msg) => Inbound::Message(Box::new(msg)),
            Err(e) => {
                warn!(%ty, "failed to parse an opened envelope: {e}");
                Inbound::Ignore
            }
        }
    }

    fn keys_arrived(
        &self,
        request_id: &str,
        account: Option<String>,
        keys: Vec<RuntimeKeyEntry>,
    ) -> Vec<TunnelMessage> {
        let mut verified = Vec::new();
        for entry in keys {
            match self.learn(&entry.statement) {
                Some(s) if s.runtime_id == entry.runtime_id => verified.push(entry),
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
        if let (Want::Account(wanted), Some(account)) = (&want, account.as_ref()) {
            if wanted != account {
                return Vec::new();
            }
            self.lists.lock().unwrap().insert(
                account.clone(),
                AccountList {
                    at: Instant::now(),
                    entries: verified,
                },
            );
        }
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
    use crate::tunnel::protocol::RuntimeVouchEntry;
    use crate::tunnel::verifying_key_to_did_key;

    fn runtime() -> (Arc<SigningKey>, String) {
        let key = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
        let did = verifying_key_to_did_key(&key.verifying_key());
        (Arc::new(key), did)
    }

    #[derive(Default)]
    struct Policy {
        own: Option<String>,
        computers: Mutex<HashMap<String, ComputerStatus>>,
        devices: Mutex<HashMap<String, String>>,
    }

    #[async_trait::async_trait]
    impl SealPolicy for Policy {
        async fn own_account(&self) -> Option<String> {
            self.own.clone()
        }
        fn own_computer(&self, _account: &str, runtime: &str) -> Option<ComputerStatus> {
            self.computers.lock().unwrap().get(runtime).copied()
        }
        fn device_key(&self, device_id: &str) -> Option<String> {
            self.devices.lock().unwrap().get(device_id).cloned()
        }
    }

    fn sealer(policy: Policy) -> (Sealer, String) {
        let (key, did) = runtime();
        (Sealer::new(did.clone(), key, Arc::new(policy), None), did)
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
        let sealed = seal_frame(frame, &phone_public, None).unwrap();
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
        let (opened, sender) = open_frame(frame, &runtime).unwrap();
        assert_eq!(opened["type"], "tunnel_device_request");
        assert_eq!(opened["signature"], "a phone's signed request");
        assert!(sender.is_none());
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
    fn a_vouch_verifies_only_for_its_voucher() {
        let (key, did) = runtime();
        let vouch = sign_vouch(&key, "user:hub:bob", "did:key:znew");
        let v = verify_vouch(&did, &vouch).unwrap();
        assert_eq!(v.runtime_id, "did:key:znew");
        let (_, other) = runtime();
        assert!(verify_vouch(&other, &vouch).is_err());
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
        let sealed = seal_frame(frame.clone(), recipient.public(), Some("stmt")).unwrap();
        let wire = serde_json::to_string(&sealed).unwrap();
        assert!(!wire.contains("the plans"));
        assert!(!wire.contains("jws"));
        assert_eq!(sealed.len(), 3);

        let (opened, sender) = open_frame(sealed, &recipient).unwrap();
        assert_eq!(opened, frame);
        assert_eq!(sender.as_deref(), Some("stmt"));
    }

    #[test]
    fn a_changed_header_does_not_open() {
        let (key, _) = runtime();
        let recipient = AgreementKey::derive(&key);
        let mut frame = Map::new();
        frame.insert("type".into(), "tunnel_channel_event".into());
        frame.insert("channelId".into(), "chan_a".into());
        frame.insert("event".into(), "x".into());
        let mut sealed = seal_frame(frame, recipient.public(), None).unwrap();
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
                vouches: Vec::new(),
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

    fn entry(s: &Sealer, id: &str, vouches: Vec<RuntimeVouchEntry>) -> RuntimeKeyEntry {
        RuntimeKeyEntry {
            runtime_id: id.into(),
            statement: s.statement().into(),
            vouches,
        }
    }

    async fn answer_account(
        sealer: &Sealer,
        frames: &[Vec<u8>],
        account: &str,
        keys: Vec<RuntimeKeyEntry>,
    ) -> Vec<TunnelMessage> {
        let request = frame(&frames[0]);
        assert_eq!(request["account"], account);
        let answer = serde_json::to_vec(&TunnelMessage::RuntimeKeys {
            request_id: request["requestId"].as_str().unwrap().into(),
            account: Some(account.into()),
            keys,
        })
        .unwrap();
        match sealer.inbound(&answer).await {
            Inbound::Release(m) => m,
            other => panic!("expected a release, got {other:?}"),
        }
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
    async fn another_persons_computers_are_pinned_on_first_use_then_need_a_vouch() {
        let (alice, alice_id) = sealer(Policy::default());
        let (b1, b1_id) = sealer(Policy::default());
        let (b2, b2_id) = sealer(Policy::default());
        let (b3, b3_id) = sealer(Policy::default());
        let bob = "user:hub:bob";

        // First use: Bob's one computer is pinned and sealed to.
        let frames = alice.outbound(invitation(&alice_id, bob)).await;
        let released = answer_account(&alice, &frames, bob, vec![entry(&b1, &b1_id, vec![])]).await;
        let frames = alice.outbound(released[0].clone()).await;
        assert_eq!(recipients(&frames), vec![b1_id.clone()]);
        assert!(matches!(b1.inbound(&frames[0]).await, Inbound::Message(_)));

        // Later Bob has two more: one vouched for by b1, one by nobody.
        alice.lists.lock().unwrap().clear();
        let b1_key = Arc::clone(&b1.signing);
        let vouch = sign_vouch(&b1_key, bob, &b2_id);
        let frames = alice.outbound(invitation(&alice_id, bob)).await;
        let released = answer_account(
            &alice,
            &frames,
            bob,
            vec![
                entry(&b1, &b1_id, vec![]),
                entry(
                    &b2,
                    &b2_id,
                    vec![RuntimeVouchEntry {
                        by: b1_id.clone(),
                        statement: vouch,
                    }],
                ),
                entry(&b3, &b3_id, vec![]),
            ],
        )
        .await;
        let frames = alice.outbound(released[0].clone()).await;
        let mut want = vec![b1_id, b2_id];
        want.sort();
        assert_eq!(recipients(&frames), want);
    }

    #[tokio::test]
    async fn own_account_messages_go_only_to_approved_computers() {
        let policy = Policy {
            own: Some("alice".into()),
            ..Policy::default()
        };
        let (c2, c2_id) = sealer(Policy::default());
        let (c3, c3_id) = sealer(Policy::default());
        policy
            .computers
            .lock()
            .unwrap()
            .insert(c2_id.clone(), ComputerStatus::Approved);
        let (me, me_id) = sealer(policy);
        let mark = TunnelMessage::TunnelReadMark {
            request_id: "m".into(),
            source_runtime_id: me_id.clone(),
            source_principal_did: "user:hub:alice".into(),
            channel_id: "chan_x".into(),
            msg_id: "msg_1".into(),
            signature: "sig".into(),
        };
        let frames = me.outbound(mark).await;
        let released = answer_account(
            &me,
            &frames,
            "user:hub:alice",
            vec![entry(&c2, &c2_id, vec![]), entry(&c3, &c3_id, vec![])],
        )
        .await;
        let frames = me.outbound(released[0].clone()).await;
        assert_eq!(recipients(&frames), vec![c2_id]);
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
        let (opened, sender) = open_frame(f, &phone).unwrap();
        assert_eq!(opened["signature"], "what the phone shows");
        assert!(sender.is_none());
    }
}
