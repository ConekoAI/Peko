//! Paired phones (ADR-108): the records, the pairing proof, what a device
//! signs and what the runtime signs back, and notifications sealed to a
//! device's key.
//!
//! A phone runs no runtime. It asks one of its owner's computers through
//! PekoHub, and that runtime answers a request only when it is signed by a
//! key recorded in `devices.json`. That key was enrolled with a secret the
//! phone read from the computer's screen, so PekoHub, which relays every
//! message, can neither enroll a key nor act as the owner. Everything the
//! runtime sends a device is signed with the runtime's key, which the
//! phone learned from the same screen.
//!
//! This module holds no daemon state: [`DeviceGate`] keeps the records,
//! the open pairings, the replay window and the watches; the daemon
//! carries out the verbs (`daemon/devices.rs`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::{SigningKey, VerifyingKey};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use super::tunnel_channel_signature::{jws_sign, jws_verify};

/// Version tag of a pairing proof.
pub const DEVICE_PAIR_VERSION: &str = "peko-device-pair/1";
/// Version tag of the runtime's answer to a pairing.
pub const DEVICE_PAIRED_VERSION: &str = "peko-device-paired/1";
/// Version tag of a device request.
pub const DEVICE_REQUEST_VERSION: &str = "peko-device-request/1";
/// Version tag of the runtime's answer to a device request.
pub const DEVICE_RESPONSE_VERSION: &str = "peko-device-response/1";
/// Version tag of a live event.
pub const DEVICE_EVENT_VERSION: &str = "peko-device-event/1";
/// Version tag of a notification, and the HPKE `info` it is sealed under.
pub const DEVICE_NOTIFY_VERSION: &str = "peko-device-notify/1";
/// Version tag of a read mark sent between a person's runtimes.
pub const READ_MARK_VERSION: &str = "peko-read-mark/1";

/// How long a pairing shown on the computer may be used.
pub const PAIRING_TTL: Duration = Duration::from_mins(10);
/// Wrong proofs that end a pairing.
const PAIRING_MAX_FAILURES: u32 = 3;
/// Longest a device request may be valid for, and how long its id is
/// remembered (seconds).
pub const REQUEST_WINDOW_SECS: i64 = 300;
/// Clock-skew leeway on `exp` (seconds).
const EXP_LEEWAY_SECS: i64 = 30;
/// Clock-skew allowance on `iat` (seconds).
const IAT_FUTURE_SECS: i64 = 60;
/// A watch the runtime does not hear from for this long ends.
pub const WATCH_TTL: Duration = Duration::from_secs(90);
/// Most devices one runtime pairs with.
const MAX_DEVICES: usize = 16;

/// A paired phone, as `devices.json` keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRecord {
    pub device_id: String,
    /// What the phone calls itself ("Mira's iPhone").
    pub name: String,
    /// `ios` or `android`.
    pub platform: String,
    /// ed25519 public key, base64url: signs the device's requests.
    pub signing_key: String,
    /// X25519 public key, base64url: notifications are sealed to it.
    pub agreement_key: String,
    /// RFC 3339.
    pub paired_at: String,
    /// RFC 3339; when the device last asked for something.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DevicesFile {
    #[serde(default)]
    devices: Vec<DeviceRecord>,
}

/// What the computer shows for a phone to scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingOffer {
    pub pairing_id: String,
    /// 32 random bytes, base64url. Shown on screen and never sent.
    pub secret: String,
    pub runtime_id: String,
    pub hub_url: String,
    /// RFC 3339.
    pub expires_at: String,
}

impl PairingOffer {
    /// The text the QR code holds.
    #[must_use]
    pub fn qr_text(&self) -> String {
        let mut url = url::Url::parse("peko://pair").expect("static URL");
        url.query_pairs_mut()
            .append_pair("v", "1")
            .append_pair("hub", &self.hub_url)
            .append_pair("runtime", &self.runtime_id)
            .append_pair("id", &self.pairing_id)
            .append_pair("secret", &self.secret);
        url.into()
    }
}

/// The signed payload of a `TunnelDevicePair` proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePairPayload {
    pub v: String,
    pub pairing_id: String,
    pub runtime_id: String,
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub signing_key: String,
    pub agreement_key: String,
    pub iat: i64,
    pub exp: i64,
    /// HMAC-SHA256 under the pairing secret over [`pairing_mac_input`],
    /// base64url.
    pub mac: String,
}

/// The signed payload of a `TunnelDevicePaired`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePairedPayload {
    pub v: String,
    pub request_id: String,
    pub device_id: String,
    pub runtime_id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// ADR-110 D7: the runtime's X25519 agreement key (base64url), which
    /// the phone seals its requests to. Signed here by the key the phone
    /// read from the computer's screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agreement_key: Option<String>,
    pub iat: i64,
}

/// The signed payload of a `TunnelDeviceRequest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceRequestPayload {
    pub v: String,
    pub request_id: String,
    pub device_id: String,
    pub runtime_id: String,
    pub verb: String,
    #[serde(default)]
    pub args: serde_json::Value,
    pub iat: i64,
    pub exp: i64,
}

/// The signed payload of a `TunnelDeviceResponse`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceResponsePayload {
    pub v: String,
    pub request_id: String,
    pub device_id: String,
    pub runtime_id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub iat: i64,
}

/// The signed payload of a `TunnelDeviceEvent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceEventPayload {
    pub v: String,
    pub device_id: String,
    pub runtime_id: String,
    /// Counts up per device watch, for ordering. Not for replay: a gap
    /// is filled by reading.
    pub seq: u64,
    pub event: serde_json::Value,
    pub iat: i64,
}

/// What a notification says, signed by the runtime and then sealed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceNotifyPayload {
    pub v: String,
    pub device_id: String,
    pub runtime_id: String,
    pub title: String,
    pub body: String,
    pub channel: String,
    /// The post it is about; absent for an approval request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<String>,
    /// The approval request it is about, and its peko.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peko: Option<String>,
    pub iat: i64,
}

/// The signed payload of a `TunnelReadMark`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadMarkPayload {
    pub v: String,
    pub request_id: String,
    pub source_runtime_id: String,
    pub source_principal_did: String,
    pub channel_id: String,
    pub msg_id: String,
    pub iat: i64,
    pub exp: i64,
}

/// What the pairing MAC covers: the pairing, the runtime, the device and
/// both of its keys, one per line after the version tag.
#[must_use]
pub fn pairing_mac_input(
    pairing_id: &str,
    runtime_id: &str,
    device_id: &str,
    signing_key: &str,
    agreement_key: &str,
) -> String {
    format!(
        "{DEVICE_PAIR_VERSION}\n{pairing_id}\n{runtime_id}\n{device_id}\n{signing_key}\n{agreement_key}"
    )
}

/// The pairing MAC, base64url. `secret` is the offer's secret as shown.
///
/// # Errors
///
/// `secret` is not base64url.
pub fn pairing_mac(secret: &str, input: &str) -> Result<String, String> {
    Ok(URL_SAFE_NO_PAD.encode(pairing_hmac(secret, input)?.finalize().into_bytes()))
}

fn pairing_hmac(secret: &str, input: &str) -> Result<Hmac<Sha256>, String> {
    let key = URL_SAFE_NO_PAD
        .decode(secret)
        .map_err(|_| "pairing secret is not base64url".to_string())?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(|e| e.to_string())?;
    mac.update(input.as_bytes());
    Ok(mac)
}

/// Decode a base64url ed25519 public key.
fn signing_key_of(encoded: &str) -> Result<VerifyingKey, String> {
    let bytes: [u8; 32] = URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or("signing key must be 32 bytes, base64url")?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| "signing key is not an ed25519 key".to_string())
}

/// The payload segment of a compact JWS, decoded, before its signature
/// is checked (to learn which key should have signed it).
fn unverified_payload<T: serde::de::DeserializeOwned>(compact: &str) -> Result<T, String> {
    let segment = compact.split('.').nth(1).ok_or("not a compact JWS")?;
    let bytes = URL_SAFE_NO_PAD
        .decode(segment)
        .map_err(|_| "payload is not base64url")?;
    serde_json::from_slice(&bytes).map_err(|e| format!("payload is not valid: {e}"))
}

fn verified_payload<T: serde::de::DeserializeOwned>(
    key: &VerifyingKey,
    compact: &str,
) -> Result<T, String> {
    let (_, bytes) = jws_verify(key, compact).map_err(|e| format!("{e:#}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("payload is not valid: {e}"))
}

fn check_freshness(iat: i64, exp: i64) -> Result<(), String> {
    let now = chrono::Utc::now().timestamp();
    if now > exp + EXP_LEEWAY_SECS {
        return Err("[stale] the request has expired".into());
    }
    if iat > now + IAT_FUTURE_SECS {
        return Err("[stale] the request is dated in the future".into());
    }
    if exp - iat > REQUEST_WINDOW_SECS {
        return Err("[stale] the request is valid for too long".into());
    }
    Ok(())
}

fn valid_device_id(id: &str) -> bool {
    (8..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Sign the runtime's answer to a pairing.
#[must_use]
pub fn sign_paired(
    runtime_key: &SigningKey,
    request_id: &str,
    device_id: &str,
    runtime_id: &str,
    outcome: &Result<DeviceRecord, String>,
) -> String {
    let payload = DevicePairedPayload {
        v: DEVICE_PAIRED_VERSION.into(),
        request_id: request_id.into(),
        device_id: device_id.into(),
        runtime_id: runtime_id.into(),
        ok: outcome.is_ok(),
        error: outcome.as_ref().err().cloned(),
        agreement_key: outcome.is_ok().then(|| {
            super::seal::AgreementKey::derive(runtime_key)
                .public()
                .to_string()
        }),
        iat: chrono::Utc::now().timestamp(),
    };
    jws_sign(
        runtime_key,
        &serde_json::to_vec(&payload).expect("payload serializes"),
    )
}

/// Sign the runtime's answer to a device request.
#[must_use]
pub fn sign_response(
    runtime_key: &SigningKey,
    request_id: &str,
    device_id: &str,
    runtime_id: &str,
    outcome: Result<serde_json::Value, String>,
) -> String {
    let (ok, body, error) = match outcome {
        Ok(body) => (true, Some(body), None),
        Err(error) => (false, None, Some(error)),
    };
    let payload = DeviceResponsePayload {
        v: DEVICE_RESPONSE_VERSION.into(),
        request_id: request_id.into(),
        device_id: device_id.into(),
        runtime_id: runtime_id.into(),
        ok,
        body,
        error,
        iat: chrono::Utc::now().timestamp(),
    };
    jws_sign(
        runtime_key,
        &serde_json::to_vec(&payload).expect("payload serializes"),
    )
}

/// Sign a live event for a watching device.
#[must_use]
pub fn sign_event(
    runtime_key: &SigningKey,
    device_id: &str,
    runtime_id: &str,
    seq: u64,
    event: serde_json::Value,
) -> String {
    let payload = DeviceEventPayload {
        v: DEVICE_EVENT_VERSION.into(),
        device_id: device_id.into(),
        runtime_id: runtime_id.into(),
        seq,
        event,
        iat: chrono::Utc::now().timestamp(),
    };
    jws_sign(
        runtime_key,
        &serde_json::to_vec(&payload).expect("payload serializes"),
    )
}

/// Sign `notification` with the runtime's key and seal it to the device's
/// agreement key: HPKE base mode (RFC 9180) with DHKEM(X25519,
/// HKDF-SHA256), HKDF-SHA256 and ChaCha20-Poly1305, `info` =
/// [`DEVICE_NOTIFY_VERSION`], empty AAD. Returns base64url of the
/// 32-byte encapsulated key followed by the ciphertext.
///
/// # Errors
///
/// The device's agreement key is not an X25519 public key.
pub fn seal_notification(
    runtime_key: &SigningKey,
    agreement_key: &str,
    notification: &DeviceNotifyPayload,
) -> Result<String, String> {
    use hpke::{
        aead::ChaCha20Poly1305, kdf::HkdfSha256, kem::X25519HkdfSha256, Deserializable,
        Kem as KemTrait, OpModeS, Serializable,
    };
    let signed = jws_sign(
        runtime_key,
        &serde_json::to_vec(notification).expect("payload serializes"),
    );
    let key_bytes = URL_SAFE_NO_PAD
        .decode(agreement_key)
        .map_err(|_| "agreement key is not base64url".to_string())?;
    let recipient = <X25519HkdfSha256 as KemTrait>::PublicKey::from_bytes(&key_bytes)
        .map_err(|_| "agreement key is not an X25519 key".to_string())?;
    let (encapped, ciphertext) =
        hpke::single_shot_seal::<ChaCha20Poly1305, HkdfSha256, X25519HkdfSha256, _>(
            &OpModeS::Base,
            &recipient,
            DEVICE_NOTIFY_VERSION.as_bytes(),
            signed.as_bytes(),
            b"",
            &mut rand::rngs::OsRng,
        )
        .map_err(|e| format!("seal failed: {e}"))?;
    let mut out = encapped.to_bytes().to_vec();
    out.extend_from_slice(&ciphertext);
    Ok(URL_SAFE_NO_PAD.encode(out))
}

/// Sign a read mark for the person's other runtimes.
#[must_use]
pub fn sign_read_mark(runtime_key: &SigningKey, payload: &ReadMarkPayload) -> String {
    jws_sign(
        runtime_key,
        &serde_json::to_vec(payload).expect("payload serializes"),
    )
}

/// Verify a read mark's runtime signature, version and freshness.
///
/// # Errors
///
/// The JWS does not verify, the version differs, or it expired.
pub fn verify_read_mark(
    runtime_key: &VerifyingKey,
    compact: &str,
) -> Result<ReadMarkPayload, String> {
    let payload: ReadMarkPayload = verified_payload(runtime_key, compact)?;
    if payload.v != READ_MARK_VERSION {
        return Err(format!("read mark version must be {READ_MARK_VERSION:?}"));
    }
    check_freshness(payload.iat, payload.exp)?;
    Ok(payload)
}

struct Pairing {
    secret: String,
    expires: Instant,
    expires_at: String,
    failures: u32,
}

struct Watch {
    seq: u64,
    heard: Instant,
}

#[derive(Default)]
struct GateState {
    devices: Vec<DeviceRecord>,
    pairings: HashMap<String, Pairing>,
    /// Request ids seen, with when each may be forgotten (unix seconds).
    seen: HashMap<String, i64>,
    watches: HashMap<String, Watch>,
    /// ADR-110 D7: phones removed in the last minute, with their X25519
    /// key, so the answer to their own `unpair` can still be sealed.
    removed: HashMap<String, (String, Instant)>,
}

/// The runtime's paired devices and everything that decides whether a
/// device message is believed.
pub struct DeviceGate {
    path: PathBuf,
    runtime_id: String,
    /// Requests issued before this are refused: the seen list starts
    /// empty at every start.
    started_at: i64,
    state: Mutex<GateState>,
}

impl std::fmt::Debug for DeviceGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceGate")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl DeviceGate {
    /// Where files a phone is sending are kept until its post names
    /// them (`<runtime>/device_uploads/<device>/`, ADR-108 D8).
    #[must_use]
    pub fn uploads_dir(&self) -> PathBuf {
        self.path.with_file_name("device_uploads")
    }

    /// Load the records at `path` (`<runtime>/devices.json`); a missing
    /// file is no devices. An unreadable one is logged and read as none,
    /// so every device must pair again rather than any being believed.
    #[must_use]
    pub fn load(path: PathBuf, runtime_id: String) -> Self {
        let devices = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<DevicesFile>(&bytes) {
                Ok(file) => file.devices,
                Err(e) => {
                    tracing::warn!(path = %path.display(), "devices.json is not readable: {e}");
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };
        Self {
            path,
            runtime_id,
            started_at: chrono::Utc::now().timestamp(),
            state: Mutex::new(GateState {
                devices,
                ..GateState::default()
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().expect("device gate mutex")
    }

    /// This runtime's id (`did:key`).
    #[must_use]
    pub fn runtime_id(&self) -> &str {
        &self.runtime_id
    }

    fn save(&self, devices: &[DeviceRecord]) -> Result<(), String> {
        write_private(
            &self.path,
            &serde_json::to_vec_pretty(&DevicesFile {
                devices: devices.to_vec(),
            })
            .expect("devices serialize"),
        )
        .map_err(|e| format!("could not save {}: {e}", self.path.display()))
    }

    /// Open a pairing for the owner to show: single use, ten minutes.
    #[must_use]
    pub fn start_pairing(&self, hub_url: &str) -> PairingOffer {
        use rand::RngCore as _;
        let mut secret = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut secret);
        let secret = URL_SAFE_NO_PAD.encode(secret);
        let pairing_id = format!("pair_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
        let expires_at = (chrono::Utc::now()
            + chrono::Duration::from_std(PAIRING_TTL).expect("ten minutes"))
        .to_rfc3339();
        let mut state = self.state();
        let now = Instant::now();
        state.pairings.retain(|_, p| p.expires > now);
        state.pairings.insert(
            pairing_id.clone(),
            Pairing {
                secret: secret.clone(),
                expires: now + PAIRING_TTL,
                expires_at: expires_at.clone(),
                failures: 0,
            },
        );
        PairingOffer {
            pairing_id,
            secret,
            runtime_id: self.runtime_id.clone(),
            hub_url: hub_url.to_string(),
            expires_at,
        }
    }

    /// Check a pairing proof and record the device (ADR-108 D2).
    ///
    /// # Errors
    ///
    /// A sentence for the phone to show: the pairing is unknown, used or
    /// expired, the proof is not the phone's, or it was not made with the
    /// secret on the screen.
    pub fn pair(
        &self,
        runtime_id: &str,
        device_id: &str,
        proof: &str,
    ) -> Result<DeviceRecord, String> {
        const BAD: &str = "This pairing code did not work. Show a new one on your computer.";
        let claimed: DevicePairPayload = unverified_payload(proof).map_err(|_| BAD.to_string())?;
        let key = signing_key_of(&claimed.signing_key)?;
        let payload: DevicePairPayload =
            verified_payload(&key, proof).map_err(|_| BAD.to_string())?;
        if payload.v != DEVICE_PAIR_VERSION
            || payload.runtime_id != self.runtime_id
            || runtime_id != self.runtime_id
            || payload.device_id != device_id
        {
            return Err(BAD.into());
        }
        check_freshness(payload.iat, payload.exp)?;
        if !valid_device_id(device_id) {
            return Err("That device id is not valid.".into());
        }
        let agreement: [u8; 32] = URL_SAFE_NO_PAD
            .decode(&payload.agreement_key)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or("agreement key must be 32 bytes, base64url")?;
        let _ = agreement;
        let name: String = payload.name.trim().chars().take(64).collect();
        if name.is_empty() || !matches!(payload.platform.as_str(), "ios" | "android") {
            return Err("The phone did not say what it is.".into());
        }

        let mut state = self.state();
        let now = Instant::now();
        state.pairings.retain(|_, p| p.expires > now);
        let Some(pairing) = state.pairings.get_mut(&payload.pairing_id) else {
            return Err("That pairing code has expired. Show a new one on your computer.".into());
        };
        let input = pairing_mac_input(
            &payload.pairing_id,
            &payload.runtime_id,
            &payload.device_id,
            &payload.signing_key,
            &payload.agreement_key,
        );
        let mac_ok = URL_SAFE_NO_PAD
            .decode(&payload.mac)
            .ok()
            .zip(pairing_hmac(&pairing.secret, &input).ok())
            .is_some_and(|(mac, expected)| expected.verify_slice(&mac).is_ok());
        if !mac_ok {
            pairing.failures += 1;
            if pairing.failures >= PAIRING_MAX_FAILURES {
                state.pairings.remove(&payload.pairing_id);
            }
            return Err(BAD.into());
        }
        state.pairings.remove(&payload.pairing_id);

        let record = DeviceRecord {
            device_id: device_id.to_string(),
            name,
            platform: payload.platform,
            signing_key: payload.signing_key,
            agreement_key: payload.agreement_key,
            paired_at: chrono::Utc::now().to_rfc3339(),
            last_seen: None,
        };
        let mut devices = state.devices.clone();
        devices.retain(|d| d.device_id != device_id);
        if devices.len() >= MAX_DEVICES {
            return Err("This computer is paired with too many phones. Remove one first.".into());
        }
        devices.push(record.clone());
        self.save(&devices)?;
        state.devices = devices;
        Ok(record)
    }

    /// When the pairing `pairing_id` stops working, while it is open.
    #[must_use]
    pub fn pairing_expires_at(&self, pairing_id: &str) -> Option<String> {
        let state = self.state();
        state
            .pairings
            .get(pairing_id)
            .filter(|p| p.expires > Instant::now())
            .map(|p| p.expires_at.clone())
    }

    /// Every paired device, oldest pairing first.
    #[must_use]
    pub fn devices(&self) -> Vec<DeviceRecord> {
        self.state().devices.clone()
    }

    /// The record of `device_id`, when paired.
    #[must_use]
    pub fn device(&self, device_id: &str) -> Option<DeviceRecord> {
        self.state()
            .devices
            .iter()
            .find(|d| d.device_id == device_id)
            .cloned()
    }

    /// Delete a device's record and end its watch. Returns the record
    /// removed, if there was one.
    ///
    /// # Errors
    ///
    /// The file could not be written; the record stays.
    pub fn remove(&self, device_id: &str) -> Result<Option<DeviceRecord>, String> {
        let mut state = self.state();
        let Some(index) = state.devices.iter().position(|d| d.device_id == device_id) else {
            return Ok(None);
        };
        let mut devices = state.devices.clone();
        let removed = devices.remove(index);
        self.save(&devices)?;
        state.devices = devices;
        state.watches.remove(device_id);
        state
            .removed
            .retain(|_, (_, at)| at.elapsed() < Duration::from_mins(1));
        state.removed.insert(
            device_id.to_string(),
            (removed.agreement_key.clone(), Instant::now()),
        );
        Ok(Some(removed))
    }

    /// The X25519 key to seal to `device_id` (ADR-110 D7): a paired
    /// phone's, or that of one removed in the last minute, whose answer to
    /// being removed is still on its way.
    #[must_use]
    pub fn sealing_key(&self, device_id: &str) -> Option<String> {
        let state = self.state();
        if let Some(record) = state.devices.iter().find(|d| d.device_id == device_id) {
            return Some(record.agreement_key.clone());
        }
        state
            .removed
            .get(device_id)
            .filter(|(_, at)| at.elapsed() < Duration::from_mins(1))
            .map(|(key, _)| key.clone())
    }

    /// Check a device request (ADR-108 D3): signed by the key recorded
    /// for `device_id`, addressed to this runtime, fresh, issued since
    /// this runtime started, and not seen before. Returns the device and
    /// what it asks.
    ///
    /// # Errors
    ///
    /// A tagged reason: `[unpaired]`, `[stale]`, `[replayed]`, or
    /// `[bad_request]`.
    pub fn verify_request(
        &self,
        request_id: &str,
        device_id: &str,
        runtime_id: &str,
        signature: &str,
    ) -> Result<(DeviceRecord, DeviceRequestPayload), String> {
        let unpaired = || "[unpaired] This phone is not paired with this computer.".to_string();
        let device = self.device(device_id).ok_or_else(unpaired)?;
        let key = signing_key_of(&device.signing_key).map_err(|_| unpaired())?;
        let payload: DeviceRequestPayload =
            verified_payload(&key, signature).map_err(|_| unpaired())?;
        if payload.v != DEVICE_REQUEST_VERSION
            || payload.request_id != request_id
            || payload.device_id != device_id
            || payload.runtime_id != self.runtime_id
            || runtime_id != self.runtime_id
        {
            return Err("[bad_request] the request does not match what was signed".into());
        }
        check_freshness(payload.iat, payload.exp)?;
        if payload.iat < self.started_at - IAT_FUTURE_SECS {
            return Err("[stale] the request was made before this computer was ready".into());
        }
        let now = chrono::Utc::now().timestamp();
        let mut state = self.state();
        state.seen.retain(|_, forget| *forget > now);
        if state.seen.contains_key(request_id) {
            return Err("[replayed] the request was already carried out".into());
        }
        state
            .seen
            .insert(request_id.to_string(), payload.exp + EXP_LEEWAY_SECS);
        // `last_seen` is for the Devices list: kept to the minute, so a
        // busy phone does not rewrite the file on every request.
        let stamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:00Z").to_string();
        let mut seen_now = None;
        if let Some(record) = state.devices.iter_mut().find(|d| d.device_id == device_id) {
            if record.last_seen.as_deref() != Some(&stamp) {
                record.last_seen = Some(stamp);
                seen_now = Some(());
            }
        }
        if seen_now.is_some() {
            let devices = state.devices.clone();
            if let Err(e) = self.save(&devices) {
                tracing::debug!("device last-seen not saved: {e}");
            }
        }
        Ok((device, payload))
    }

    /// Start or keep `device_id`'s watch (ADR-108 D4).
    pub fn watch(&self, device_id: &str) {
        let mut state = self.state();
        let now = Instant::now();
        state
            .watches
            .entry(device_id.to_string())
            .and_modify(|w| w.heard = now)
            .or_insert(Watch { seq: 0, heard: now });
    }

    /// End `device_id`'s watch.
    pub fn unwatch(&self, device_id: &str) {
        self.state().watches.remove(device_id);
    }

    /// The devices watching now, each with the `seq` of its next event.
    /// Watches not heard from for [`WATCH_TTL`] end here.
    #[must_use]
    pub fn next_events(&self) -> Vec<(DeviceRecord, u64)> {
        let mut state = self.state();
        let now = Instant::now();
        state
            .watches
            .retain(|_, w| now.duration_since(w.heard) < WATCH_TTL);
        let GateState {
            devices, watches, ..
        } = &mut *state;
        devices
            .iter()
            .filter_map(|device| {
                let watch = watches.get_mut(&device.device_id)?;
                watch.seq += 1;
                Some((device.clone(), watch.seq))
            })
            .collect()
    }

    /// Whether any device watches now.
    #[must_use]
    pub fn has_watchers(&self) -> bool {
        let state = self.state();
        let now = Instant::now();
        state
            .watches
            .values()
            .any(|w| now.duration_since(w.heard) < WATCH_TTL)
    }
}

/// Write `bytes` to `path` through a temp file, readable by the owner
/// only: the file names keys that act as the owner.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// A fresh ed25519 key (tests).
#[cfg(any(test, feature = "test-utils"))]
#[must_use]
pub fn fresh_signing_key() -> SigningKey {
    use rand::RngCore as _;
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    SigningKey::from_bytes(&seed)
}

/// A phone, for tests: its keys, and the messages it signs. No app.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_device {
    use super::*;
    use hpke::{Kem as KemTrait, Serializable};

    /// A scripted phone.
    pub struct TestDevice {
        pub device_id: String,
        pub signing: SigningKey,
        agreement_secret: <hpke::kem::X25519HkdfSha256 as KemTrait>::PrivateKey,
        pub agreement_key: String,
    }

    impl Default for TestDevice {
        fn default() -> Self {
            Self::new()
        }
    }

    impl TestDevice {
        /// A phone with fresh keys.
        #[must_use]
        pub fn new() -> Self {
            let signing = fresh_signing_key();
            let (agreement_secret, public) =
                hpke::kem::X25519HkdfSha256::gen_keypair(&mut rand::rngs::OsRng);
            Self {
                device_id: format!("dev_{}", uuid::Uuid::new_v4().simple()),
                signing,
                agreement_secret,
                agreement_key: URL_SAFE_NO_PAD.encode(public.to_bytes()),
            }
        }

        /// The phone's signing key as it enrolls it.
        #[must_use]
        pub fn signing_key(&self) -> String {
            URL_SAFE_NO_PAD.encode(self.signing.verifying_key().to_bytes())
        }

        /// The pairing proof for what the phone scanned.
        #[must_use]
        pub fn pair_proof(&self, offer: &PairingOffer) -> String {
            self.pair_proof_with(offer, &offer.secret)
        }

        /// A pairing proof made with `secret` instead of the offer's.
        #[must_use]
        pub fn pair_proof_with(&self, offer: &PairingOffer, secret: &str) -> String {
            let iat = chrono::Utc::now().timestamp();
            let signing_key = self.signing_key();
            let mac = pairing_mac(
                secret,
                &pairing_mac_input(
                    &offer.pairing_id,
                    &offer.runtime_id,
                    &self.device_id,
                    &signing_key,
                    &self.agreement_key,
                ),
            )
            .expect("secret is base64url");
            let payload = DevicePairPayload {
                v: DEVICE_PAIR_VERSION.into(),
                pairing_id: offer.pairing_id.clone(),
                runtime_id: offer.runtime_id.clone(),
                device_id: self.device_id.clone(),
                name: "Test phone".into(),
                platform: "ios".into(),
                signing_key,
                agreement_key: self.agreement_key.clone(),
                iat,
                exp: iat + 120,
                mac,
            };
            jws_sign(&self.signing, &serde_json::to_vec(&payload).unwrap())
        }

        /// A signed request: its id and its signature.
        #[must_use]
        pub fn request(
            &self,
            runtime_id: &str,
            verb: &str,
            args: serde_json::Value,
        ) -> (String, String) {
            let request_id = format!("dreq_{}", uuid::Uuid::new_v4().simple());
            let iat = chrono::Utc::now().timestamp();
            let payload = DeviceRequestPayload {
                v: DEVICE_REQUEST_VERSION.into(),
                request_id: request_id.clone(),
                device_id: self.device_id.clone(),
                runtime_id: runtime_id.into(),
                verb: verb.into(),
                args,
                iat,
                exp: iat + 120,
            };
            let signature = jws_sign(&self.signing, &serde_json::to_vec(&payload).unwrap());
            (request_id, signature)
        }

        /// Open a sealed notification and check the runtime signed it.
        ///
        /// # Errors
        ///
        /// It was not sealed to this phone or not signed by `runtime_id`.
        pub fn open_notification(
            &self,
            runtime_id: &str,
            sealed: &str,
        ) -> Result<DeviceNotifyPayload, String> {
            use hpke::{
                aead::ChaCha20Poly1305, kdf::HkdfSha256, kem::X25519HkdfSha256, Deserializable,
                OpModeR,
            };
            let bytes = URL_SAFE_NO_PAD.decode(sealed).map_err(|e| e.to_string())?;
            if bytes.len() < 32 {
                return Err("too short".into());
            }
            let (encapped, ciphertext) = bytes.split_at(32);
            let encapped = <X25519HkdfSha256 as KemTrait>::EncappedKey::from_bytes(encapped)
                .map_err(|e| e.to_string())?;
            let signed = hpke::single_shot_open::<ChaCha20Poly1305, HkdfSha256, X25519HkdfSha256>(
                &OpModeR::Base,
                &self.agreement_secret,
                &encapped,
                DEVICE_NOTIFY_VERSION.as_bytes(),
                ciphertext,
                b"",
            )
            .map_err(|e| e.to_string())?;
            let key =
                crate::tunnel::did_key_to_verifying_key(runtime_id).map_err(|e| e.to_string())?;
            verified_payload(
                &key,
                std::str::from_utf8(&signed).map_err(|e| e.to_string())?,
            )
        }

        /// Read what the runtime signed for this phone.
        ///
        /// # Errors
        ///
        /// `signature` is not `runtime_id`'s.
        pub fn read<T: serde::de::DeserializeOwned>(
            runtime_id: &str,
            signature: &str,
        ) -> Result<T, String> {
            let key =
                crate::tunnel::did_key_to_verifying_key(runtime_id).map_err(|e| e.to_string())?;
            verified_payload(&key, signature)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_device::TestDevice;
    use super::*;

    fn gate() -> (tempfile::TempDir, DeviceGate, SigningKey) {
        let dir = tempfile::tempdir().unwrap();
        let key = fresh_signing_key();
        let runtime_id = crate::tunnel::verifying_key_to_did_key(&key.verifying_key());
        let gate = DeviceGate::load(dir.path().join("devices.json"), runtime_id);
        (dir, gate, key)
    }

    #[test]
    fn a_phone_pairs_with_the_secret_on_the_screen_once() {
        let (dir, gate, _) = gate();
        let phone = TestDevice::new();
        let offer = gate.start_pairing("https://hub.test");
        assert!(offer
            .qr_text()
            .starts_with("peko://pair?v=1&hub=https%3A%2F%2Fhub.test&runtime=did"));

        let record = gate
            .pair(
                gate.runtime_id(),
                &phone.device_id,
                &phone.pair_proof(&offer),
            )
            .expect("pairs");
        assert_eq!(record.signing_key, phone.signing_key());
        assert_eq!(gate.devices(), vec![record.clone()]);

        // Single use.
        let second = TestDevice::new();
        assert!(gate
            .pair(
                gate.runtime_id(),
                &second.device_id,
                &second.pair_proof(&offer)
            )
            .is_err());

        // The record outlives the process, readable by the owner only.
        let reloaded = DeviceGate::load(dir.path().join("devices.json"), gate.runtime_id().into());
        assert_eq!(reloaded.devices(), vec![record]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join("devices.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// PekoHub relays the proof but never sees the secret: a key enrolled
    /// without it is refused, and three wrong proofs end the pairing.
    #[test]
    fn a_proof_made_without_the_secret_is_refused() {
        let (_dir, gate, _) = gate();
        let offer = gate.start_pairing("https://hub.test");
        let wrong = URL_SAFE_NO_PAD.encode([7u8; 32]);
        for _ in 0..3 {
            let intruder = TestDevice::new();
            assert!(gate
                .pair(
                    gate.runtime_id(),
                    &intruder.device_id,
                    &intruder.pair_proof_with(&offer, &wrong)
                )
                .is_err());
        }
        assert!(gate.devices().is_empty());
        let phone = TestDevice::new();
        assert!(
            gate.pair(
                gate.runtime_id(),
                &phone.device_id,
                &phone.pair_proof(&offer)
            )
            .is_err(),
            "the pairing ended after three wrong proofs"
        );
    }

    /// A proof whose keys were swapped after signing fails the phone's
    /// own signature; one re-signed by another key fails the MAC.
    #[test]
    fn a_proof_for_another_device_or_runtime_is_refused() {
        let (_dir, gate, _) = gate();
        let phone = TestDevice::new();
        let offer = gate.start_pairing("https://hub.test");
        let proof = phone.pair_proof(&offer);
        assert!(gate
            .pair(gate.runtime_id(), "dev_someone_else", &proof)
            .is_err());
        assert!(gate
            .pair("did:key:z6MkOther", &phone.device_id, &proof)
            .is_err());
        let mut elsewhere = offer.clone();
        elsewhere.runtime_id = "did:key:z6MkOther".into();
        assert!(gate
            .pair(
                gate.runtime_id(),
                &phone.device_id,
                &phone.pair_proof(&elsewhere)
            )
            .is_err());
        assert!(gate
            .pair(gate.runtime_id(), &phone.device_id, &proof)
            .is_ok());
    }

    #[test]
    fn a_request_is_believed_once_and_only_from_a_paired_key() {
        let (_dir, gate, _) = gate();
        let phone = TestDevice::new();
        let offer = gate.start_pairing("https://hub.test");
        gate.pair(
            gate.runtime_id(),
            &phone.device_id,
            &phone.pair_proof(&offer),
        )
        .unwrap();

        let (id, signature) =
            phone.request(gate.runtime_id(), "conversations", serde_json::json!({}));
        let (device, payload) = gate
            .verify_request(&id, &phone.device_id, gate.runtime_id(), &signature)
            .expect("believed");
        assert_eq!(
            (device.device_id.as_str(), payload.verb.as_str()),
            (phone.device_id.as_str(), "conversations")
        );
        assert!(gate.device(&phone.device_id).unwrap().last_seen.is_some());

        let replay = gate
            .verify_request(&id, &phone.device_id, gate.runtime_id(), &signature)
            .unwrap_err();
        assert!(replay.starts_with("[replayed]"), "{replay}");

        // Another key claiming the paired device's id.
        let mut intruder = TestDevice::new();
        intruder.device_id.clone_from(&phone.device_id);
        let (id, signature) =
            intruder.request(gate.runtime_id(), "conversations", serde_json::json!({}));
        let refused = gate
            .verify_request(&id, &phone.device_id, gate.runtime_id(), &signature)
            .unwrap_err();
        assert!(refused.starts_with("[unpaired]"), "{refused}");

        // The envelope must be what was signed.
        let (_, signature) = phone.request(gate.runtime_id(), "post", serde_json::json!({}));
        assert!(gate
            .verify_request(
                "dreq_other",
                &phone.device_id,
                gate.runtime_id(),
                &signature
            )
            .is_err());

        // Removed: refused from then on. Its key stays for a minute, so
        // the answer to its own `unpair` can still be sealed (ADR-110 D7).
        let key = gate.sealing_key(&phone.device_id).unwrap();
        assert!(gate.remove(&phone.device_id).unwrap().is_some());
        assert_eq!(gate.sealing_key(&phone.device_id), Some(key));
        assert_eq!(gate.sealing_key("dev_never_paired"), None);
        let (id, signature) =
            phone.request(gate.runtime_id(), "conversations", serde_json::json!({}));
        assert!(gate
            .verify_request(&id, &phone.device_id, gate.runtime_id(), &signature)
            .unwrap_err()
            .starts_with("[unpaired]"));
    }

    #[test]
    fn a_request_made_before_the_runtime_started_is_refused() {
        let (_dir, mut gate, _) = gate();
        let phone = TestDevice::new();
        let offer = gate.start_pairing("https://hub.test");
        gate.pair(
            gate.runtime_id(),
            &phone.device_id,
            &phone.pair_proof(&offer),
        )
        .unwrap();
        let (id, signature) =
            phone.request(gate.runtime_id(), "conversations", serde_json::json!({}));
        gate.started_at = chrono::Utc::now().timestamp() + 200;
        let refused = gate
            .verify_request(&id, &phone.device_id, gate.runtime_id(), &signature)
            .unwrap_err();
        assert!(refused.starts_with("[stale]"), "{refused}");
    }

    #[test]
    fn the_runtime_signs_what_it_answers_and_seals_what_it_notifies() {
        let (_dir, gate, key) = gate();
        let phone = TestDevice::new();
        let signature = sign_response(
            &key,
            "dreq_1",
            &phone.device_id,
            gate.runtime_id(),
            Ok(serde_json::json!({"n": 1})),
        );
        let answer: DeviceResponsePayload =
            TestDevice::read(gate.runtime_id(), &signature).unwrap();
        assert_eq!(
            (answer.ok, answer.body),
            (true, Some(serde_json::json!({"n": 1})))
        );
        let other = fresh_signing_key();
        let forged = sign_response(
            &other,
            "dreq_1",
            &phone.device_id,
            gate.runtime_id(),
            Err("no".into()),
        );
        assert!(TestDevice::read::<DeviceResponsePayload>(gate.runtime_id(), &forged).is_err());

        let notification = DeviceNotifyPayload {
            v: DEVICE_NOTIFY_VERSION.into(),
            device_id: phone.device_id.clone(),
            runtime_id: gate.runtime_id().into(),
            title: "Mochi".into(),
            body: "Done. The report is in your inbox.".into(),
            channel: "dm_0123".into(),
            msg_id: Some("msg_1".into()),
            approval: None,
            peko: None,
            iat: 1,
        };
        let sealed = seal_notification(&key, &phone.agreement_key, &notification).unwrap();
        assert!(!sealed.contains("Mochi"));
        assert_eq!(
            phone.open_notification(gate.runtime_id(), &sealed).unwrap(),
            notification
        );
        assert!(TestDevice::new()
            .open_notification(gate.runtime_id(), &sealed)
            .is_err());
        // Sealed by someone who is not the runtime: opened, not believed.
        let forged = seal_notification(&other, &phone.agreement_key, &notification).unwrap();
        assert!(phone.open_notification(gate.runtime_id(), &forged).is_err());
    }

    #[test]
    fn a_watch_ends_when_asked_and_counts_its_events() {
        let (_dir, gate, _) = gate();
        let phone = TestDevice::new();
        let offer = gate.start_pairing("https://hub.test");
        gate.pair(
            gate.runtime_id(),
            &phone.device_id,
            &phone.pair_proof(&offer),
        )
        .unwrap();
        assert!(gate.next_events().is_empty());
        gate.watch(&phone.device_id);
        assert!(gate.has_watchers());
        assert_eq!(gate.next_events()[0].1, 1);
        gate.watch(&phone.device_id);
        assert_eq!(
            gate.next_events()[0].1,
            2,
            "keeping a watch does not restart it"
        );
        gate.unwatch(&phone.device_id);
        assert!(gate.next_events().is_empty());
    }
}
