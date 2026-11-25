//! `link_hello` (ADR-125 D3): each side of a link proves its runtime key
//! over the TLS session it is on.
//!
//! The certificate a link's TLS session runs on proves nothing. What
//! does is the first frame each side sends: a compact JWS by its runtime
//! signing key over `{ v, runtime_id, binding, iat }`, where `binding` is
//! 32 bytes exported from this TLS session (RFC 5705, label
//! [`EXPORTER_LABEL`]). Someone in the middle holds two sessions with two
//! different exports, so a hello signed on one is refused on the other.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};

use crate::tunnel::did_key::did_key_to_verifying_key;
use crate::tunnel::tunnel_channel_signature::{jws_sign, jws_verify};

/// Version tag of a hello.
pub const LINK_HELLO_VERSION: &str = "peko-link/1";
/// RFC 5705 exporter label the binding is taken under.
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-peko-link";
/// How far a hello's `iat` may be from now, either way.
pub const HELLO_FRESH_FOR_SECS: i64 = 300;
/// The wire `type` of the hello frame. Link-only: it never reaches
/// PekoHub and is not a `TunnelMessage`.
pub const HELLO_TYPE: &str = "link_hello";

/// What a hello signs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloPayload {
    pub v: String,
    pub runtime_id: String,
    /// base64url of the 32 exported bytes.
    pub binding: String,
    pub iat: i64,
}

/// Why a hello was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HelloRefusal {
    #[error("not a link hello: {0}")]
    Malformed(String),
    #[error("the hello is not signed by the runtime it names")]
    BadSignature,
    #[error("the hello is for another version ({0})")]
    WrongVersion(String),
    #[error("the hello was signed on another TLS session")]
    WrongBinding,
    #[error("the hello is stale (iat {iat}, now {now})")]
    Stale { iat: i64, now: i64 },
    #[error("the hello names this computer")]
    Itself,
    #[error("{0} is not a computer this one links with")]
    NotAdmitted(String),
}

impl HelloRefusal {
    /// A short reason for the `link.refused` audit event and the status.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "malformed",
            Self::BadSignature => "bad_signature",
            Self::WrongVersion(_) => "wrong_version",
            Self::WrongBinding => "wrong_binding",
            Self::Stale { .. } => "stale",
            Self::Itself => "itself",
            Self::NotAdmitted(_) => "not_admitted",
        }
    }
}

/// Sign this runtime's hello for a TLS session whose export is `binding`.
#[must_use]
pub fn sign_hello(key: &SigningKey, runtime_id: &str, binding: &[u8; 32]) -> String {
    sign_hello_at(key, runtime_id, binding, chrono::Utc::now().timestamp())
}

/// [`sign_hello`] with a given `iat`.
#[must_use]
pub fn sign_hello_at(key: &SigningKey, runtime_id: &str, binding: &[u8; 32], iat: i64) -> String {
    let payload = HelloPayload {
        v: LINK_HELLO_VERSION.into(),
        runtime_id: runtime_id.into(),
        binding: URL_SAFE_NO_PAD.encode(binding),
        iat,
    };
    jws_sign(
        key,
        &serde_json::to_vec(&payload).expect("a hello serializes"),
    )
}

/// The hello frame on the wire.
#[must_use]
pub fn hello_frame(compact: &str) -> String {
    serde_json::json!({ "type": HELLO_TYPE, "hello": compact }).to_string()
}

/// The wire `type` each side sends once it accepted the other's hello.
pub const READY_TYPE: &str = "link_ready";

/// The frame saying this side accepted the other's hello.
#[must_use]
pub fn ready_frame() -> String {
    serde_json::json!({ "type": READY_TYPE }).to_string()
}

/// Whether `frame` is [`ready_frame`].
#[must_use]
pub fn is_ready(frame: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(frame)
        .ok()
        .and_then(|v| v.get("type")?.as_str().map(|t| t == READY_TYPE))
        .unwrap_or(false)
}

/// The hello a frame carries, if it is one.
#[must_use]
pub fn hello_of(frame: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(frame).ok()?;
    if value.get("type")?.as_str()? != HELLO_TYPE {
        return None;
    }
    Some(value.get("hello")?.as_str()?.to_string())
}

/// Check a peer's hello: it verifies against the `did:key` it names, was
/// signed for this session (`binding`, what this end exports), is within
/// [`HELLO_FRESH_FOR_SECS`] of `now`, does not name `own_runtime`, and
/// names a runtime `admits` lets in. Returns that runtime's id.
///
/// # Errors
///
/// The first of those it fails.
pub fn verify_hello(
    compact: &str,
    binding: &[u8; 32],
    now: i64,
    own_runtime: &str,
    admits: impl FnOnce(&str) -> bool,
) -> Result<String, HelloRefusal> {
    let claimed: HelloPayload = unverified(compact)?;
    let key = did_key_to_verifying_key(&claimed.runtime_id)
        .map_err(|e| HelloRefusal::Malformed(format!("runtime id: {e}")))?;
    let (_, bytes) = jws_verify(&key, compact).map_err(|_| HelloRefusal::BadSignature)?;
    let payload: HelloPayload =
        serde_json::from_slice(&bytes).map_err(|e| HelloRefusal::Malformed(e.to_string()))?;
    if payload.v != LINK_HELLO_VERSION {
        return Err(HelloRefusal::WrongVersion(payload.v));
    }
    let signed_binding = URL_SAFE_NO_PAD
        .decode(&payload.binding)
        .map_err(|_| HelloRefusal::WrongBinding)?;
    if !constant_time_eq(&signed_binding, binding) {
        return Err(HelloRefusal::WrongBinding);
    }
    if (now - payload.iat).abs() > HELLO_FRESH_FOR_SECS {
        return Err(HelloRefusal::Stale {
            iat: payload.iat,
            now,
        });
    }
    if payload.runtime_id == own_runtime {
        return Err(HelloRefusal::Itself);
    }
    if !admits(&payload.runtime_id) {
        return Err(HelloRefusal::NotAdmitted(payload.runtime_id));
    }
    Ok(payload.runtime_id)
}

fn unverified(compact: &str) -> Result<HelloPayload, HelloRefusal> {
    let payload = compact
        .split('.')
        .nth(1)
        .ok_or_else(|| HelloRefusal::Malformed("not a compact JWS".into()))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| HelloRefusal::Malformed("payload is not base64url".into()))?;
    serde_json::from_slice(&bytes).map_err(|e| HelloRefusal::Malformed(e.to_string()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::verifying_key_to_did_key;

    fn runtime() -> (SigningKey, String) {
        let key = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
        let did = verifying_key_to_did_key(&key.verifying_key());
        (key, did)
    }

    const ME: &str = "did:key:z6MkThisComputer";

    #[test]
    fn a_hello_for_this_session_is_accepted() {
        let (key, did) = runtime();
        let binding = [3u8; 32];
        let hello = sign_hello(&key, &did, &binding);
        let now = chrono::Utc::now().timestamp();
        assert_eq!(
            verify_hello(&hello, &binding, now, ME, |r| r == did),
            Ok(did.clone())
        );
    }

    #[test]
    fn a_hello_signed_on_another_session_is_refused() {
        let (key, did) = runtime();
        let hello = sign_hello(&key, &did, &[3u8; 32]);
        let now = chrono::Utc::now().timestamp();
        assert_eq!(
            verify_hello(&hello, &[4u8; 32], now, ME, |_| true),
            Err(HelloRefusal::WrongBinding)
        );
    }

    #[test]
    fn a_stale_hello_is_refused_either_way() {
        let (key, did) = runtime();
        let binding = [3u8; 32];
        let now = chrono::Utc::now().timestamp();
        for iat in [
            now - HELLO_FRESH_FOR_SECS - 1,
            now + HELLO_FRESH_FOR_SECS + 1,
        ] {
            let hello = sign_hello_at(&key, &did, &binding, iat);
            assert!(matches!(
                verify_hello(&hello, &binding, now, ME, |_| true),
                Err(HelloRefusal::Stale { .. })
            ));
        }
        let edge = sign_hello_at(&key, &did, &binding, now - HELLO_FRESH_FOR_SECS);
        assert!(verify_hello(&edge, &binding, now, ME, |_| true).is_ok());
    }

    #[test]
    fn a_runtime_not_admitted_is_refused() {
        let (key, did) = runtime();
        let binding = [3u8; 32];
        let hello = sign_hello(&key, &did, &binding);
        let now = chrono::Utc::now().timestamp();
        assert_eq!(
            verify_hello(&hello, &binding, now, ME, |_| false),
            Err(HelloRefusal::NotAdmitted(did))
        );
    }

    #[test]
    fn a_hello_naming_another_runtime_than_its_signer_is_refused() {
        let (key, _) = runtime();
        let (_, other) = runtime();
        let binding = [3u8; 32];
        let hello = sign_hello(&key, &other, &binding);
        let now = chrono::Utc::now().timestamp();
        assert_eq!(
            verify_hello(&hello, &binding, now, ME, |_| true),
            Err(HelloRefusal::BadSignature)
        );
    }

    #[test]
    fn a_hello_naming_this_computer_is_refused() {
        let (key, did) = runtime();
        let binding = [3u8; 32];
        let hello = sign_hello(&key, &did, &binding);
        let now = chrono::Utc::now().timestamp();
        assert_eq!(
            verify_hello(&hello, &binding, now, &did, |_| true),
            Err(HelloRefusal::Itself)
        );
    }

    #[test]
    fn hello_frames_round_trip_and_other_frames_are_not_hellos() {
        let frame = hello_frame("a.b.c");
        assert_eq!(hello_of(frame.as_bytes()).as_deref(), Some("a.b.c"));
        assert_eq!(hello_of(br#"{"type":"heartbeat","seq":1}"#), None);
        assert_eq!(hello_of(b"not json"), None);
        assert!(is_ready(ready_frame().as_bytes()));
        assert!(!is_ready(frame.as_bytes()));
    }
}
