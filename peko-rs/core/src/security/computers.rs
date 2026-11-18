//! Owner changes PekoHub reported that wait for the owner (ADR-109 D5),
//! and the computers the owner approved here before ADR-111.
//!
//! Which computers speak for the owner's account is the account's device
//! list's to say now (ADR-111 D5). What ADR-109 recorded here is read only
//! by the upgrade pass that moves it onto that list (ADR-111 D7).
//!
//! On disk: `<runtime>/own_computers.json`, `0600`.

use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Approval-request ids of an owner change start with this
/// (`apr_owner<hex>`). Ordinary requests are `apr_<hex>`, so the prefix
/// cannot collide.
pub const OWNER_CHANGE_ID_PREFIX: &str = "apr_owner";

/// Where a computer stood with this one under ADR-109.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerStatus {
    Approved,
    Pending,
    Denied,
}

/// One of the account's computers, as ADR-109 recorded it here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Computer {
    /// Its runtime id (`did:key:…`).
    pub runtime: String,
    pub status: ComputerStatus,
    pub first_seen: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<DateTime<Utc>>,
    /// Where it was decided: `local`, a runtime id, `device:<id>`, or
    /// `upgrade` (D4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// Conversations it posted in while it waited; each gets an empty
    /// receipt once it is approved, so the sender resends at once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held_channels: Vec<String>,
}

/// An owner change PekoHub reported that this computer did not ask for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerChange {
    /// The peko's local id.
    pub local_id: String,
    pub hub_id: String,
    /// The Hub account PekoHub says owns it now.
    pub owner_id: String,
    pub reported_at: DateTime<Utc>,
}

impl OwnerChange {
    /// The approval-request id the owner decides it by.
    #[must_use]
    pub fn request_id(&self) -> String {
        format!(
            "{OWNER_CHANGE_ID_PREFIX}{}",
            short_hash(&format!("{}\n{}", self.local_id, self.owner_id))
        )
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ComputersFile {
    /// The account the list belongs to; another account starts afresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    /// Whether the upgrade pass (D4) ran for `account`.
    #[serde(default)]
    seeded: bool,
    #[serde(default)]
    computers: Vec<Computer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    owner_changes: Vec<OwnerChange>,
}

/// An owner's decision the tunnel acts on (it lives where `principal/`
/// may not reach): an owner change approved, so the peko is announced
/// again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decided {
    OwnerChangeApproved { principal_name: String },
}

type DecidedHook = std::sync::Arc<dyn Fn(Decided) + Send + Sync>;

/// `runtime/own_computers.json`, shared by everything on this runtime
/// (one instance per [`crate::principal::PrincipalManager`]).
pub struct OwnComputers {
    path: PathBuf,
    file: Mutex<Option<ComputersFile>>,
    on_decided: Mutex<Option<DecidedHook>>,
}

impl std::fmt::Debug for OwnComputers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnComputers")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

fn short_hash(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .take(10)
        .fold(String::new(), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        })
}

impl OwnComputers {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: Mutex::new(None),
            on_decided: Mutex::new(None),
        }
    }

    /// Where the file lives (tests write an ADR-109 file to upgrade from).
    #[cfg(test)]
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Who acts on the owner's decisions (the tunnel; the latest wins).
    pub fn on_decided(&self, hook: impl Fn(Decided) + Send + Sync + 'static) {
        *self.on_decided.lock().expect("own computers hook") = Some(std::sync::Arc::new(hook));
    }

    /// Tell the hook about `decided`, if one is set.
    pub fn decided(&self, decided: Decided) {
        let hook = self.on_decided.lock().expect("own computers hook").clone();
        if let Some(hook) = hook {
            hook(decided);
        }
    }

    /// Run `f` on the file as it is for `account` (loaded on first use;
    /// emptied when it belonged to another account), saving it when `f`
    /// says it changed.
    fn with<T>(&self, account: &str, f: impl FnOnce(&mut ComputersFile) -> (T, bool)) -> T {
        let mut guard = self.file.lock().expect("own computers mutex");
        let file = guard.get_or_insert_with(|| self.load());
        let mut changed = false;
        if file.account.as_deref() != Some(account) {
            *file = ComputersFile {
                account: Some(account.to_string()),
                ..ComputersFile::default()
            };
            changed = true;
        }
        let (out, dirty) = f(file);
        if changed || dirty {
            self.save(file);
        }
        out
    }

    fn load(&self) -> ComputersFile {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                // Unreadable: trust no computer rather than guess; the
                // owner is asked again.
                tracing::warn!(path = %self.path.display(), "own_computers.json is not readable: {e}");
                ComputersFile::default()
            }),
            Err(_) => ComputersFile::default(),
        }
    }

    fn save(&self, file: &ComputersFile) {
        let bytes = serde_json::to_vec_pretty(file).expect("own computers serialize");
        if let Err(e) = write_private(&self.path, &bytes) {
            tracing::warn!(path = %self.path.display(), "could not save own_computers.json: {e}");
        }
    }

    /// Every computer ADR-109 recorded here for `account`.
    #[must_use]
    pub fn list(&self, account: &str) -> Vec<Computer> {
        self.with(account, |f| (f.computers.clone(), false))
    }

    /// Note an owner change PekoHub reported (D5). Replaces an earlier
    /// report for the same peko.
    pub fn report_owner_change(&self, account: &str, change: OwnerChange) {
        self.with(account, |f| {
            f.owner_changes.retain(|c| c.local_id != change.local_id);
            f.owner_changes.push(change);
            ((), true)
        });
    }

    /// The owner changes waiting for the owner of peko `local_id`.
    #[must_use]
    pub fn owner_changes_for(&self, account: &str, local_id: &str) -> Vec<OwnerChange> {
        self.with(account, |f| {
            (
                f.owner_changes
                    .iter()
                    .filter(|c| c.local_id == local_id)
                    .cloned()
                    .collect(),
                false,
            )
        })
    }

    /// Take the owner change an approval-request id names, if it waits.
    pub fn take_owner_change(&self, account: &str, id: &str) -> Option<OwnerChange> {
        self.with(account, |f| {
            let at = f.owner_changes.iter().position(|c| c.request_id() == id);
            match at {
                Some(i) => (Some(f.owner_changes.remove(i)), true),
                None => (None, false),
            }
        })
    }
}

/// Write `bytes` to `path` atomically, readable by the owner only.
pub(crate) fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, OwnComputers) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("own_computers.json");
        (dir, OwnComputers::new(path))
    }

    #[test]
    fn the_old_approvals_are_read_for_one_account() {
        let (dir, _) = store();
        let path = dir.path().join("own_computers.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "account": "acct",
                "seeded": true,
                "computers": [{
                    "runtime": "did:key:zOld",
                    "status": "approved",
                    "first_seen": "2026-11-17T00:00:00Z"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let reopened = OwnComputers::new(path);
        let listed = reopened.list("acct");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, ComputerStatus::Approved);
        // Another account: nothing carries over.
        assert!(reopened.list("other").is_empty());
    }

    #[test]
    fn an_owner_change_waits_until_taken() {
        let (_dir, computers) = store();
        let change = OwnerChange {
            local_id: "prin_ada".into(),
            hub_id: "hub-1".into(),
            owner_id: "bob".into(),
            reported_at: Utc::now(),
        };
        computers.report_owner_change("acct", change.clone());
        assert_eq!(
            computers.owner_changes_for("acct", "prin_ada"),
            std::slice::from_ref(&change)
        );
        assert!(computers.owner_changes_for("acct", "prin_other").is_empty());
        assert_eq!(
            computers.take_owner_change("acct", &change.request_id()),
            Some(change)
        );
        assert!(computers.owner_changes_for("acct", "prin_ada").is_empty());
    }
}
