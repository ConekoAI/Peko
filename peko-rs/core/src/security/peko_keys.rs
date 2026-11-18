//! Which computer hosts a peko, on the peko's own word (ADR-110 D4).
//!
//! PekoHub's directory says which runtime hosts a peko. What a runtime
//! seals to that peko is only as private as that answer, so the answer
//! must carry the peko's own signed word for it: its announce proof (a
//! compact JWS by the peko's signing key over `{runtimeId, principalDid,
//! iat, exp}`), which PekoHub keeps and serves as `hostStatement`. The
//! peko's signing key moves with it, so a move to another computer brings
//! a new statement from the same key.
//!
//! The first time this runtime sees a peko it pins the peko's signing key.
//! A later answer with another key is not used: it is held as an approval
//! request for the owner (`apr_keychange…`), and nothing goes to that peko
//! until they decide.
//!
//! On disk: `<runtime>/peko_keys.json`, `0600`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Approval-request ids of a key change start with this.
pub const KEY_CHANGE_ID_PREFIX: &str = "apr_keychange";

/// A peko's signing key as this runtime first knew it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PekoPin {
    pub signing_did: String,
    pub pinned_at: DateTime<Utc>,
}

/// A peko PekoHub now names with another key, waiting for the owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyChange {
    pub hub_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The key this runtime pinned.
    pub pinned: String,
    /// The key PekoHub now names.
    pub offered: String,
    pub reported_at: DateTime<Utc>,
}

impl KeyChange {
    /// The approval-request id the owner decides it by.
    #[must_use]
    pub fn request_id(&self) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(format!("{}\n{}", self.hub_id, self.offered).as_bytes());
        let hex = digest.iter().take(10).fold(String::new(), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        });
        format!("{KEY_CHANGE_ID_PREFIX}{hex}")
    }
}

/// Why a directory answer is not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostRefusal {
    /// No statement from the peko that it is hosted where PekoHub says.
    NoHostStatement,
    /// PekoHub names the peko with another key than the one pinned here.
    KeyChanged(KeyChange),
}

impl std::fmt::Display for HostRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHostStatement => write!(
                f,
                "PekoHub did not show that this peko is on the computer it named; nothing was sent"
            ),
            Self::KeyChanged(change) => write!(
                f,
                "this peko has a new identity since you last talked; nothing goes to it until you approve the change on this computer ({})",
                change.request_id()
            ),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PekoKeysFile {
    #[serde(default)]
    pins: BTreeMap<String, PekoPin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    changes: Vec<KeyChange>,
}

/// `runtime/peko_keys.json`, shared by everything on this runtime.
#[derive(Debug)]
pub struct PekoKeys {
    path: Option<PathBuf>,
    file: Mutex<PekoKeysFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HostClaims {
    runtime_id: String,
    principal_did: String,
}

/// Whether `statement` is `signing_did`'s word that it is hosted on
/// `runtime_id`.
#[must_use]
pub fn host_statement_names(statement: &str, signing_did: &str, runtime_id: &str) -> bool {
    let Ok(key) = crate::tunnel::did_key_to_verifying_key(signing_did) else {
        return false;
    };
    let Ok((_, bytes)) = crate::tunnel::tunnel_channel_signature::jws_verify(&key, statement)
    else {
        return false;
    };
    serde_json::from_slice::<HostClaims>(&bytes)
        .is_ok_and(|c| c.runtime_id == runtime_id && c.principal_did == signing_did)
}

impl PekoKeys {
    /// The store at `path` (`None`: in memory only).
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Self {
        let file = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            path,
            file: Mutex::new(file),
        }
    }

    fn save(&self, file: &PekoKeysFile) {
        let Some(path) = &self.path else { return };
        let bytes = serde_json::to_vec_pretty(file).expect("peko keys serialize");
        if let Err(e) = crate::security::computers::write_private(path, &bytes) {
            tracing::warn!(path = %path.display(), "could not save peko keys: {e}");
        }
    }

    /// Check PekoHub's answer that the peko `hub_id`, signing as
    /// `signing_did`, is hosted on `runtime_id`, by `host_statement`.
    /// Pins the key the first time; a different key is held for the owner.
    ///
    /// # Errors
    ///
    /// The answer carries no valid host statement, or names another key.
    pub fn check(
        &self,
        hub_id: &str,
        name: Option<&str>,
        signing_did: &str,
        runtime_id: &str,
        host_statement: Option<&str>,
    ) -> Result<(), HostRefusal> {
        if !host_statement.is_some_and(|s| host_statement_names(s, signing_did, runtime_id)) {
            return Err(HostRefusal::NoHostStatement);
        }
        let mut file = self.file.lock().unwrap();
        match file.pins.get(hub_id) {
            Some(pin) if pin.signing_did == signing_did => Ok(()),
            Some(pin) => {
                let change = KeyChange {
                    hub_id: hub_id.into(),
                    name: name.map(str::to_owned),
                    pinned: pin.signing_did.clone(),
                    offered: signing_did.into(),
                    reported_at: Utc::now(),
                };
                if !file
                    .changes
                    .iter()
                    .any(|c| c.hub_id == hub_id && c.offered == signing_did)
                {
                    file.changes.push(change.clone());
                    self.save(&file);
                }
                Err(HostRefusal::KeyChanged(change))
            }
            None => {
                file.pins.insert(
                    hub_id.into(),
                    PekoPin {
                        signing_did: signing_did.into(),
                        pinned_at: Utc::now(),
                    },
                );
                self.save(&file);
                Ok(())
            }
        }
    }

    /// The signing key pinned for the peko `hub_id`, if it was met.
    #[must_use]
    pub fn pinned(&self, hub_id: &str) -> Option<String> {
        self.file
            .lock()
            .unwrap()
            .pins
            .get(hub_id)
            .map(|p| p.signing_did.clone())
    }

    /// Key changes waiting for the owner, oldest first.
    #[must_use]
    pub fn pending(&self) -> Vec<KeyChange> {
        self.file.lock().unwrap().changes.clone()
    }

    /// The owner's decision on a key change: approved, the new key is
    /// pinned; denied, the old one stays and the request is gone.
    ///
    /// # Errors
    ///
    /// No key change has that id.
    pub fn decide(&self, id: &str, approve: bool) -> anyhow::Result<KeyChange> {
        let mut file = self.file.lock().unwrap();
        let at = file
            .changes
            .iter()
            .position(|c| c.request_id() == id)
            .ok_or_else(|| anyhow::anyhow!("No approval request {id}."))?;
        let change = file.changes.remove(at);
        if approve {
            file.pins.insert(
                change.hub_id.clone(),
                PekoPin {
                    signing_did: change.offered.clone(),
                    pinned_at: Utc::now(),
                },
            );
            // Other offers for the same peko are moot now.
            file.changes.retain(|c| c.hub_id != change.hub_id);
        }
        self.save(&file);
        Ok(change)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::tunnel_channel_signature::jws_sign;
    use crate::tunnel::verifying_key_to_did_key;
    use ed25519_dalek::SigningKey;

    fn peko() -> (SigningKey, String) {
        let key = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
        let did = verifying_key_to_did_key(&key.verifying_key());
        (key, did)
    }

    fn host(key: &SigningKey, did: &str, runtime: &str) -> String {
        jws_sign(
            key,
            &serde_json::to_vec(&serde_json::json!({
                "runtimeId": runtime, "principalDid": did, "iat": 1, "exp": 2
            }))
            .unwrap(),
        )
    }

    #[test]
    fn pins_a_peko_on_first_sight_and_follows_it_to_another_computer() {
        let keys = PekoKeys::new(None);
        let (key, did) = peko();
        keys.check(
            "h1",
            None,
            &did,
            "did:key:zR1",
            Some(&host(&key, &did, "did:key:zR1")),
        )
        .unwrap();
        // Moved: a new statement by the same key.
        keys.check(
            "h1",
            None,
            &did,
            "did:key:zR2",
            Some(&host(&key, &did, "did:key:zR2")),
        )
        .unwrap();
    }

    #[test]
    fn refuses_a_computer_the_peko_did_not_name() {
        let keys = PekoKeys::new(None);
        let (key, did) = peko();
        let statement = host(&key, &did, "did:key:zR1");
        assert_eq!(
            keys.check("h1", None, &did, "did:key:zHub", Some(&statement)),
            Err(HostRefusal::NoHostStatement)
        );
        assert_eq!(
            keys.check("h1", None, &did, "did:key:zR1", None),
            Err(HostRefusal::NoHostStatement)
        );
    }

    #[test]
    fn holds_another_key_for_the_owner_and_pins_it_once_approved() {
        let keys = PekoKeys::new(None);
        let (key, did) = peko();
        keys.check(
            "h1",
            Some("ada"),
            &did,
            "did:key:zR1",
            Some(&host(&key, &did, "did:key:zR1")),
        )
        .unwrap();
        let (other, other_did) = peko();
        let offered = Some(host(&other, &other_did, "did:key:zR9"));
        let Err(HostRefusal::KeyChanged(change)) = keys.check(
            "h1",
            Some("ada"),
            &other_did,
            "did:key:zR9",
            offered.as_deref(),
        ) else {
            panic!("a new key must wait for the owner");
        };
        assert_eq!(keys.pending(), vec![change.clone()]);
        // Asked again: still one request.
        let _ = keys.check(
            "h1",
            Some("ada"),
            &other_did,
            "did:key:zR9",
            offered.as_deref(),
        );
        assert_eq!(keys.pending().len(), 1);

        keys.decide(&change.request_id(), true).unwrap();
        assert!(keys.pending().is_empty());
        keys.check("h1", None, &other_did, "did:key:zR9", offered.as_deref())
            .unwrap();
        assert!(matches!(
            keys.check(
                "h1",
                None,
                &did,
                "did:key:zR1",
                Some(&host(&key, &did, "did:key:zR1"))
            ),
            Err(HostRefusal::KeyChanged(_))
        ));
    }

    #[test]
    fn a_denied_change_keeps_the_old_key() {
        let keys = PekoKeys::new(None);
        let (key, did) = peko();
        keys.check(
            "h1",
            None,
            &did,
            "did:key:zR1",
            Some(&host(&key, &did, "did:key:zR1")),
        )
        .unwrap();
        let (other, other_did) = peko();
        let offered = host(&other, &other_did, "did:key:zR9");
        let Err(HostRefusal::KeyChanged(change)) =
            keys.check("h1", None, &other_did, "did:key:zR9", Some(&offered))
        else {
            panic!()
        };
        keys.decide(&change.request_id(), false).unwrap();
        keys.check(
            "h1",
            None,
            &did,
            "did:key:zR1",
            Some(&host(&key, &did, "did:key:zR1")),
        )
        .unwrap();
    }
}
