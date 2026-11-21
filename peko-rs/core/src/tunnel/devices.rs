//! Phones (ADR-108): what a device signs and what the runtime signs back,
//! and notifications sealed to a device's key.
//!
//! A phone runs no runtime. It asks one of its owner's computers through
//! PekoHub, and that runtime answers a request only when it is signed by
//! the key of a phone on the owner's device list (ADR-111 D5). A phone
//! joins that list by a code shown on a device already on it, so PekoHub,
//! which relays every message, can neither enroll a key nor act as the
//! owner. Everything the runtime sends a device is signed with the
//! runtime's key, which is on the same list.
//!
//! This module holds no daemon state: [`DeviceGate`] keeps the phones the
//! list names, the replay window and the watches; the daemon carries out
//! the verbs (`daemon/devices.rs`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use super::tunnel_channel_signature::{jws_sign, jws_verify};

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

/// Longest a device request may be valid for, and how long its id is
/// remembered (seconds).
pub const REQUEST_WINDOW_SECS: i64 = 300;
/// Clock-skew leeway on `exp` (seconds).
const EXP_LEEWAY_SECS: i64 = 30;
/// Clock-skew allowance on `iat` (seconds).
const IAT_FUTURE_SECS: i64 = 60;
/// A watch the runtime does not hear from for this long ends.
pub const WATCH_TTL: Duration = Duration::from_secs(90);
/// A phone on the owner's device list, as the gate keeps it. Before
/// ADR-111 these were stored in `devices.json` (read once, by the upgrade
/// pass).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRecord {
    pub device_id: String,
    /// What the phone calls itself ("Mira's iPhone").
    pub name: String,
    /// `ios` or `android` (empty for a phone that joined by a code).
    #[serde(default)]
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

impl DeviceRecord {
    /// A phone on the device list (ADR-111): its Hub id, its name, and the
    /// keys the list carries. `None` for a phone without a Hub id.
    #[must_use]
    pub fn from_member(member: &crate::security::device_list::Member) -> Option<Self> {
        let device = &member.device;
        let key = super::did_key_to_verifying_key(&device.id).ok()?;
        Some(Self {
            device_id: device.hub_device_id.clone()?,
            name: device.name.clone().unwrap_or_default(),
            platform: String::new(),
            signing_key: URL_SAFE_NO_PAD.encode(key.to_bytes()),
            agreement_key: device.agreement_key.clone(),
            paired_at: chrono::DateTime::from_timestamp(member.added_at, 0)
                .unwrap_or_default()
                .to_rfc3339(),
            last_seen: None,
        })
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DevicesFile {
    #[serde(default)]
    devices: Vec<DeviceRecord>,
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

/// Decode a base64url ed25519 public key.
fn signing_key_of(encoded: &str) -> Result<VerifyingKey, String> {
    let bytes: [u8; 32] = URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or("signing key must be 32 bytes, base64url")?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| "signing key is not an ed25519 key".to_string())
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
            &mut crate::tunnel::seal::os_rng(),
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

struct Watch {
    seq: u64,
    heard: Instant,
}

#[derive(Default)]
struct GateState {
    devices: Vec<DeviceRecord>,
    /// Request ids seen, with when each may be forgotten (unix seconds).
    seen: HashMap<String, i64>,
    watches: HashMap<String, Watch>,
    /// ADR-110 D7: phones removed in the last minute, with their X25519
    /// key, so the answer to their own `unpair` can still be sealed.
    removed: HashMap<String, (String, Instant)>,
}

/// The phones on the owner's list and everything that decides whether a
/// device message is believed.
pub struct DeviceGate {
    path: PathBuf,
    runtime_id: String,
    /// What `devices.json` held before ADR-111, for the upgrade pass.
    legacy: Vec<DeviceRecord>,
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

    /// A gate with no phones yet. `path` is `<runtime>/devices.json`: what
    /// it held before ADR-111 is read once, for the upgrade pass; the
    /// device list names the phones from then on ([`DeviceGate::set_phones`]).
    #[must_use]
    pub fn load(path: PathBuf, runtime_id: String) -> Self {
        let legacy = match std::fs::read(&path) {
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
            legacy,
            started_at: chrono::Utc::now().timestamp(),
            state: Mutex::new(GateState::default()),
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

    /// The phones `devices.json` held before ADR-111 (the upgrade pass
    /// moves them onto the list).
    #[must_use]
    pub fn legacy(&self) -> Vec<DeviceRecord> {
        self.legacy.clone()
    }

    /// The phones on the owner's list now. A phone that left it is no
    /// longer believed; its watch ends, and its key stays for a minute so
    /// the answer to its own removal can still be sealed (ADR-110 D7).
    /// Returns the phones that left.
    pub fn set_phones(&self, phones: Vec<DeviceRecord>) -> Vec<DeviceRecord> {
        let mut state = self.state();
        let gone: Vec<DeviceRecord> = state
            .devices
            .iter()
            .filter(|d| !phones.iter().any(|p| p.device_id == d.device_id))
            .cloned()
            .collect();
        let last_seen: HashMap<String, Option<String>> = state
            .devices
            .iter()
            .map(|d| (d.device_id.clone(), d.last_seen.clone()))
            .collect();
        state.devices = phones
            .into_iter()
            .map(|mut p| {
                p.last_seen = last_seen.get(&p.device_id).cloned().flatten();
                p
            })
            .collect();
        state
            .removed
            .retain(|_, (_, at)| at.elapsed() < Duration::from_mins(1));
        for d in &gone {
            state.watches.remove(&d.device_id);
            state.removed.insert(
                d.device_id.clone(),
                (d.agreement_key.clone(), Instant::now()),
            );
        }
        gone
    }

    /// Every phone on the list, oldest first.
    #[must_use]
    pub fn devices(&self) -> Vec<DeviceRecord> {
        self.state().devices.clone()
    }

    /// The phone `device_id`, when it is on the list.
    #[must_use]
    pub fn device(&self, device_id: &str) -> Option<DeviceRecord> {
        self.state()
            .devices
            .iter()
            .find(|d| d.device_id == device_id)
            .cloned()
    }

    /// The X25519 key to seal to `device_id` (ADR-110 D7): a listed
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
        let unpaired = || "[unpaired] This phone is not on your account's devices.".to_string();
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
        // `last_seen` is for the Devices list, to the minute.
        let stamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:00Z").to_string();
        if let Some(record) = state.devices.iter_mut().find(|d| d.device_id == device_id) {
            record.last_seen = Some(stamp);
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
                hpke::kem::X25519HkdfSha256::gen_keypair(&mut crate::tunnel::seal::os_rng());
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

        /// The phone as the device list records it (ADR-111).
        #[must_use]
        pub fn list_device(&self) -> crate::security::device_list::Device {
            crate::security::device_list::Device {
                id: crate::security::device_list::did_of(&self.signing),
                kind: crate::security::device_list::DeviceKind::Phone,
                name: Some("Test phone".into()),
                agreement_key: self.agreement_key.clone(),
                hub_device_id: Some(self.device_id.clone()),
            }
        }

        /// The gate's record of this phone, as if it were on the list.
        #[must_use]
        pub fn record(&self) -> DeviceRecord {
            DeviceRecord {
                device_id: self.device_id.clone(),
                name: "Test phone".into(),
                platform: String::new(),
                signing_key: self.signing_key(),
                agreement_key: self.agreement_key.clone(),
                paired_at: chrono::Utc::now().to_rfc3339(),
                last_seen: None,
            }
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
    fn a_request_is_believed_once_and_only_from_a_listed_key() {
        let (_dir, gate, _) = gate();
        let phone = TestDevice::new();
        gate.set_phones(vec![phone.record()]);

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
        assert_eq!(gate.set_phones(Vec::new()).len(), 1);
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
        gate.set_phones(vec![phone.record()]);
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
        gate.set_phones(vec![phone.record()]);
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
