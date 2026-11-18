//! Joining a device to an account by a short code (ADR-111 D3).
//!
//! A device already on the account's list (the **holder**) shows a code. A
//! new device (the **joiner**) types or scans it. The first two characters
//! name the attempt in public (the *nameplate*, how PekoHub relays the
//! messages to the right device). The other ten are the password, about 50
//! bits that never cross PekoHub.
//!
//! The two devices run CPace over ristretto255, keyed by the password. It
//! is the balanced PAKE the IRTF's crypto research group (CFRG) recommends
//! (draft-irtf-cfrg-cpace), so PekoHub gets one guess per attempt and
//! nothing to search offline. Then, under the key they agreed:
//!
//! 1. the joiner sends its device description with a MAC;
//! 2. the holder answers with the exact payload of the list link that adds
//!    it, with a MAC;
//! 3. the joiner signs that payload as the new device; the holder signs it
//!    as the adding device and appends the link.
//!
//! This module is the arithmetic and the wire bodies, with no I/O. The
//! tunnel drives it (`tunnel::device_sync`). The phone app runs the same
//! steps (peko-mobile `src/core/join.ts`), and
//! `tests::write_mobile_fixture` pins them byte for byte.
//!
//! Every length-prefixed input below is `lp(x)`: a 4-byte big-endian
//! length, then `x`.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;
use curve25519_dalek::traits::IsIdentity;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};

/// Version tag of the join protocol, and the prefix of its labels.
pub const JOIN_VERSION: &str = "peko-join/1";
/// Crockford base32: no I, L, O or U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const NAMEPLATE_CHARS: usize = 2;
const PASSWORD_CHARS: usize = 10;
/// How long a code stays usable.
pub const CODE_LIFETIME_SECS: i64 = 600;
/// Wrong answers that end a code.
pub const MAX_STRIKES: u32 = 3;

/// A join code: a public nameplate and a secret password.
#[derive(Clone, PartialEq, Eq)]
pub struct JoinCode {
    nameplate: String,
    password: String,
}

impl std::fmt::Debug for JoinCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinCode")
            .field("nameplate", &self.nameplate)
            .finish_non_exhaustive()
    }
}

/// Why a typed code is not one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a join code: it has 12 letters and digits, like ABCD-EFGH-JKMN")]
pub struct BadCode;

impl JoinCode {
    /// A fresh random code.
    #[must_use]
    pub fn generate() -> Self {
        let pick = |n: usize| -> String {
            (0..n)
                .map(|_| ALPHABET[usize::from(rand::random::<u8>() & 31)] as char)
                .collect()
        };
        Self {
            nameplate: pick(NAMEPLATE_CHARS),
            password: pick(PASSWORD_CHARS),
        }
    }

    /// Read what a person typed: any case, with or without spaces and
    /// hyphens, the letters O, I and L read as 0, 1 and 1. A QR code's
    /// `peko://join?...&code=...` is read too.
    ///
    /// # Errors
    ///
    /// It is not twelve characters of the alphabet.
    pub fn parse(input: &str) -> Result<Self, BadCode> {
        let raw = match url::Url::parse(input.trim()) {
            Ok(url) if url.scheme() == "peko" => url
                .query_pairs()
                .find(|(k, _)| k == "code")
                .map(|(_, v)| v.into_owned())
                .ok_or(BadCode)?,
            _ => input.to_string(),
        };
        let mut chars = String::new();
        for c in raw.chars() {
            match c.to_ascii_uppercase() {
                ' ' | '-' => {}
                'O' => chars.push('0'),
                'I' | 'L' => chars.push('1'),
                c if ALPHABET.contains(&(c as u8)) && c.is_ascii() => chars.push(c),
                _ => return Err(BadCode),
            }
        }
        if chars.len() != NAMEPLATE_CHARS + PASSWORD_CHARS {
            return Err(BadCode);
        }
        Ok(Self {
            nameplate: chars[..NAMEPLATE_CHARS].to_string(),
            password: chars[NAMEPLATE_CHARS..].to_string(),
        })
    }

    /// The public part PekoHub relays by.
    #[must_use]
    pub fn nameplate(&self) -> &str {
        &self.nameplate
    }

    /// How a person reads it: `ABCD-EFGH-JKMN`.
    #[must_use]
    pub fn display(&self) -> String {
        let all = format!("{}{}", self.nameplate, self.password);
        format!("{}-{}-{}", &all[..4], &all[4..8], &all[8..])
    }

    /// What the QR code holds.
    #[must_use]
    pub fn qr_text(&self, hub: &str) -> String {
        let mut url = url::Url::parse("peko://join").expect("static URL");
        url.query_pairs_mut()
            .append_pair("v", "1")
            .append_pair("hub", hub)
            .append_pair("code", &format!("{}{}", self.nameplate, self.password));
        url.into()
    }
}

fn lp(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(
        &u32::try_from(bytes.len())
            .expect("short input")
            .to_be_bytes(),
    );
    out.extend_from_slice(bytes);
}

fn labelled(label: &str, parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    lp(&mut out, format!("{JOIN_VERSION} {label}").as_bytes());
    for part in parts {
        lp(&mut out, part);
    }
    out
}

/// The session id: the version, the account and the nameplate.
fn session_id(account: &str, nameplate: &str) -> Vec<u8> {
    labelled("sid", &[account.as_bytes(), nameplate.as_bytes()])
}

/// CPace's generator for this password and session (RFC 9496 element
/// derivation over SHA-512).
fn generator(password: &str, sid: &[u8]) -> RistrettoPoint {
    let hash = Sha512::digest(labelled("generator", &[password.as_bytes(), sid]));
    let mut bytes = [0u8; 64];
    bytes.copy_from_slice(&hash);
    RistrettoPoint::from_uniform_bytes(&bytes)
}

/// Which side of the exchange this device is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Holder,
    Joiner,
}

/// One side of CPace, before the other side's share arrives.
pub struct Cpace {
    role: Role,
    sid: Vec<u8>,
    scalar: Scalar,
    share: [u8; 32],
}

impl std::fmt::Debug for Cpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cpace")
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

/// The key both sides agreed, for the MACs that follow.
#[derive(Clone)]
pub struct JoinKey([u8; 32]);

impl std::fmt::Debug for JoinKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JoinKey(..)")
    }
}

/// Why the exchange failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JoinError {
    #[error("the other device's share is not a valid point")]
    BadShare,
    #[error("the code does not match: check it and try again")]
    WrongCode,
    #[error("{0}")]
    Refused(String),
}

impl Cpace {
    /// Start as `role` with `code`, for `account`. Returns this side and
    /// the share to send (base64url).
    #[must_use]
    pub fn start(role: Role, code: &JoinCode, account: &str) -> (Self, String) {
        let mut wide = [0u8; 64];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut wide);
        Self::start_with(
            role,
            code,
            account,
            Scalar::from_bytes_mod_order_wide(&wide),
        )
    }

    fn start_with(role: Role, code: &JoinCode, account: &str, scalar: Scalar) -> (Self, String) {
        let sid = session_id(account, &code.nameplate);
        let share = (generator(&code.password, &sid) * scalar)
            .compress()
            .to_bytes();
        let encoded = URL_SAFE_NO_PAD.encode(share);
        (
            Self {
                role,
                sid,
                scalar,
                share,
            },
            encoded,
        )
    }

    /// Finish with the other side's share.
    ///
    /// # Errors
    ///
    /// The share is not a point, or is the identity.
    pub fn finish(self, theirs: &str) -> Result<JoinKey, JoinError> {
        let bytes: [u8; 32] = URL_SAFE_NO_PAD
            .decode(theirs)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or(JoinError::BadShare)?;
        let point = CompressedRistretto(bytes)
            .decompress()
            .filter(|p| !p.is_identity())
            .ok_or(JoinError::BadShare)?;
        let shared = point * self.scalar;
        if shared.is_identity() {
            return Err(JoinError::BadShare);
        }
        let (holder, joiner) = match self.role {
            Role::Holder => (self.share, bytes),
            Role::Joiner => (bytes, self.share),
        };
        let isk = Sha512::digest(labelled(
            "isk",
            &[&self.sid, shared.compress().as_bytes(), &holder, &joiner],
        ));
        let mut key = [0u8; 32];
        key.copy_from_slice(&isk[..32]);
        Ok(JoinKey(key))
    }
}

impl JoinKey {
    /// The MAC of `bytes` under `label` (`device` from the joiner,
    /// `payload` from the holder), base64url.
    #[must_use]
    pub fn mac(&self, label: &str, bytes: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).expect("any key length");
        mac.update(&labelled(label, &[bytes]));
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }

    /// Check a MAC from the other side, in constant time.
    ///
    /// # Errors
    ///
    /// It does not match: the other side used another code.
    pub fn check(&self, label: &str, bytes: &[u8], mac: &str) -> Result<(), JoinError> {
        let theirs = URL_SAFE_NO_PAD
            .decode(mac)
            .map_err(|_| JoinError::WrongCode)?;
        let mut ours = Hmac::<Sha256>::new_from_slice(&self.0).expect("any key length");
        ours.update(&labelled(label, &[bytes]));
        ours.verify_slice(&theirs).map_err(|_| JoinError::WrongCode)
    }
}

/// The body of each `device_join` step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum JoinStep {
    /// Each side's CPace share; the joiner sends first.
    Hello { share: String },
    /// The joiner's device description (a JSON string, MAC'd as sent).
    Device { device: String, mac: String },
    /// The payload of the link that adds it (a JSON string), MAC'd.
    Payload { payload: String, mac: String },
    /// The joiner's JWS over that payload.
    Signed { joiner: String },
    /// The holder appended the link.
    Done { seq: u64 },
    /// Either side gave up, and why.
    Failed { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "user:hub:3f2c9a7e-1111-4222-8333-944455556666";

    #[test]
    fn a_code_reads_back_however_it_is_typed() {
        let code = JoinCode::generate();
        let shown = code.display();
        assert_eq!(shown.len(), 14);
        assert_eq!(JoinCode::parse(&shown).unwrap(), code);
        assert_eq!(
            JoinCode::parse(&shown.to_lowercase().replace('-', " ")).unwrap(),
            code
        );
        assert_eq!(
            JoinCode::parse(&code.qr_text("https://api.pekohub.ai")).unwrap(),
            code
        );
        assert_eq!(
            JoinCode::parse("ab0o-1lI1-2345").unwrap().display(),
            "AB00-1111-2345"
        );
        assert_eq!(JoinCode::parse("ABCD-EFGH-JKM"), Err(BadCode));
        assert_eq!(JoinCode::parse("ABCD-EFGH-JKMU"), Err(BadCode));
        assert!(!format!("{code:?}").contains(&code.password));
    }

    #[test]
    fn the_same_code_agrees_on_a_key_and_another_does_not() {
        let code = JoinCode::generate();
        let (holder, h_share) = Cpace::start(Role::Holder, &code, ACCOUNT);
        let (joiner, j_share) = Cpace::start(Role::Joiner, &code, ACCOUNT);
        let hk = holder.finish(&j_share).unwrap();
        let jk = joiner.finish(&h_share).unwrap();
        let mac = jk.mac("device", b"{}");
        hk.check("device", b"{}", &mac).unwrap();
        assert_eq!(hk.check("payload", b"{}", &mac), Err(JoinError::WrongCode));
        assert_eq!(hk.check("device", b"{ }", &mac), Err(JoinError::WrongCode));

        // A guess at the password agrees on another key.
        let mut guess = code.clone();
        guess.password = "0000000000".into();
        let (holder, h_share) = Cpace::start(Role::Holder, &code, ACCOUNT);
        let (joiner, j_share) = Cpace::start(Role::Joiner, &guess, ACCOUNT);
        let hk = holder.finish(&j_share).unwrap();
        let jk = joiner.finish(&h_share).unwrap();
        assert_eq!(
            hk.check("device", b"{}", &jk.mac("device", b"{}")),
            Err(JoinError::WrongCode)
        );

        // So does the right password for another account.
        let (holder, h_share) = Cpace::start(Role::Holder, &code, ACCOUNT);
        let (joiner, j_share) = Cpace::start(Role::Joiner, &code, "user:hub:other");
        let hk = holder.finish(&j_share).unwrap();
        let jk = joiner.finish(&h_share).unwrap();
        assert!(hk.check("device", b"x", &jk.mac("device", b"x")).is_err());
    }

    #[test]
    fn a_share_that_is_not_a_point_is_refused() {
        let code = JoinCode::generate();
        let (holder, _) = Cpace::start(Role::Holder, &code, ACCOUNT);
        assert_eq!(holder.finish("short").unwrap_err(), JoinError::BadShare);
        let (holder, _) = Cpace::start(Role::Holder, &code, ACCOUNT);
        let identity = URL_SAFE_NO_PAD.encode([0u8; 32]);
        assert_eq!(holder.finish(&identity).unwrap_err(), JoinError::BadShare);
        let (holder, _) = Cpace::start(Role::Holder, &code, ACCOUNT);
        let not_canonical = URL_SAFE_NO_PAD.encode([0xffu8; 32]);
        assert_eq!(
            holder.finish(&not_canonical).unwrap_err(),
            JoinError::BadShare
        );
    }

    #[test]
    fn steps_keep_their_wire_shape() {
        let step = JoinStep::Hello {
            share: "abc".into(),
        };
        assert_eq!(
            serde_json::to_value(&step).unwrap(),
            serde_json::json!({ "step": "hello", "share": "abc" })
        );
        let step: JoinStep =
            serde_json::from_value(serde_json::json!({ "step": "done", "seq": 3 })).unwrap();
        assert_eq!(step, JoinStep::Done { seq: 3 });
    }

    /// Fixed scalars, so peko-mobile checks its CPace against this one
    /// (`tests/fixtures/join-vector.json` there):
    /// `PEKO_JOIN_FIXTURE=<path> cargo test -p peko --lib
    /// security::join::tests::write_mobile_fixture -- --ignored`.
    #[test]
    #[ignore = "writes a fixture for peko-mobile"]
    fn write_mobile_fixture() {
        let Some(path) = std::env::var_os("PEKO_JOIN_FIXTURE") else {
            return;
        };
        std::fs::write(path, serde_json::to_vec_pretty(&vector()).unwrap()).unwrap();
    }

    fn vector() -> serde_json::Value {
        let code = JoinCode::parse("K7DX-KQ7R-MTPZ").unwrap();
        let holder_scalar = Scalar::from_bytes_mod_order_wide(&[7u8; 64]);
        let joiner_scalar = Scalar::from_bytes_mod_order_wide(&[9u8; 64]);
        let (holder, h_share) = Cpace::start_with(Role::Holder, &code, ACCOUNT, holder_scalar);
        let (joiner, j_share) = Cpace::start_with(Role::Joiner, &code, ACCOUNT, joiner_scalar);
        let hk = holder.finish(&j_share).unwrap();
        let jk = joiner.finish(&h_share).unwrap();
        assert_eq!(hk.mac("device", b"x"), jk.mac("device", b"x"));
        serde_json::json!({
            "account": ACCOUNT,
            "code": "K7DXKQ7RMTPZ",
            "holder_scalar_wide": URL_SAFE_NO_PAD.encode([7u8; 64]),
            "joiner_scalar_wide": URL_SAFE_NO_PAD.encode([9u8; 64]),
            "holder_share": h_share,
            "joiner_share": j_share,
            "device_mac_of_x": hk.mac("device", b"x"),
            "payload_mac_of_y": hk.mac("payload", b"y"),
        })
    }

    #[test]
    fn the_fixture_is_stable() {
        let v = vector();
        // Recomputing gives the same bytes: nothing random leaks in.
        assert_eq!(v, vector());
        let check = JoinCode::parse(v["code"].as_str().unwrap()).unwrap();
        assert_eq!(check.nameplate(), "K7");
    }
}
