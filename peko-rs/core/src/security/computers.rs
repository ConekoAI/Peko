//! The owner's computers this one trusts, and owner changes PekoHub
//! reported that wait for the owner (ADR-109).
//!
//! PekoHub vouches that a runtime is registered to an account (ADR-090
//! D3), but anyone who can sign in as the person, or PekoHub itself, can
//! register one. So a runtime that speaks for this computer's account is
//! obeyed only once the owner approved it on a computer they already use
//! (D1–D3), and an owner change this computer did not ask for waits for
//! the owner too (D5).
//!
//! On disk: `<runtime>/own_computers.json`, `0600`.

use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Approval-request ids of a pending computer start with this
/// (`apr_computer<hex>`), an owner change's with [`OWNER_CHANGE_ID_PREFIX`].
/// Ordinary requests are `apr_<hex>`, so neither prefix can collide.
pub const COMPUTER_ID_PREFIX: &str = "apr_computer";
/// See [`COMPUTER_ID_PREFIX`].
pub const OWNER_CHANGE_ID_PREFIX: &str = "apr_owner";

/// Where a computer stands with this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerStatus {
    Approved,
    Pending,
    Denied,
}

/// One of the account's computers, as this one knows it.
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

impl Computer {
    /// The approval-request id the owner decides it by.
    #[must_use]
    pub fn request_id(&self) -> String {
        format!("{COMPUTER_ID_PREFIX}{}", short_hash(&self.runtime))
    }
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

/// What to do with a message from a computer that speaks for the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Apply it.
    Approved,
    /// Hold it: the owner has not decided yet (and is now asked).
    Held,
    /// Refuse it: the owner said no.
    Denied,
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
/// may not reach): a computer approved, so what it sent while waiting is
/// asked for again; an owner change approved, so the peko is announced
/// again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decided {
    ComputerApproved {
        runtime: String,
        channels: Vec<String>,
    },
    OwnerChangeApproved {
        principal_name: String,
    },
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

    /// Whether the upgrade pass (D4) still has to run for `account`.
    #[must_use]
    pub fn needs_seed(&self, account: &str) -> bool {
        self.with(account, |f| (!f.seeded, false))
    }

    /// The upgrade pass (D4): approve `runtimes`, the computers already
    /// carrying the account's person in a conversation here. Runs once.
    pub fn seed(&self, account: &str, runtimes: impl IntoIterator<Item = String>) {
        let runtimes: Vec<String> = runtimes.into_iter().collect();
        self.with(account, |f| {
            if f.seeded {
                return ((), false);
            }
            let now = Utc::now();
            for runtime in runtimes {
                if f.computers.iter().any(|c| c.runtime == runtime) {
                    continue;
                }
                f.computers.push(Computer {
                    runtime,
                    status: ComputerStatus::Approved,
                    first_seen: now,
                    decided_at: Some(now),
                    via: Some("upgrade".into()),
                    held_channels: Vec::new(),
                });
            }
            f.seeded = true;
            ((), true)
        });
    }

    /// Whether `runtime` may speak for `account`; an unknown one is filed
    /// as pending (the owner is asked) and held. `channel` is the
    /// conversation a held post was for.
    pub fn admit(&self, account: &str, runtime: &str, channel: Option<&str>) -> Admission {
        self.with(account, |f| {
            if let Some(c) = f.computers.iter_mut().find(|c| c.runtime == runtime) {
                return match c.status {
                    ComputerStatus::Approved => (Admission::Approved, false),
                    ComputerStatus::Denied => (Admission::Denied, false),
                    ComputerStatus::Pending => {
                        let added = channel.is_some_and(|ch| {
                            !c.held_channels.iter().any(|h| h == ch) && c.held_channels.len() < 64
                        });
                        if added {
                            c.held_channels
                                .push(channel.unwrap_or_default().to_string());
                        }
                        (Admission::Held, added)
                    }
                };
            }
            f.computers.push(Computer {
                runtime: runtime.to_string(),
                status: ComputerStatus::Pending,
                first_seen: Utc::now(),
                decided_at: None,
                via: None,
                held_channels: channel.map(str::to_string).into_iter().collect(),
            });
            (Admission::Held, true)
        })
    }

    /// Every computer this one knows for `account`.
    #[must_use]
    pub fn list(&self, account: &str) -> Vec<Computer> {
        self.with(account, |f| (f.computers.clone(), false))
    }

    /// The computers waiting for the owner, oldest first.
    #[must_use]
    pub fn pending(&self, account: &str) -> Vec<Computer> {
        let mut out: Vec<_> = self
            .list(account)
            .into_iter()
            .filter(|c| c.status == ComputerStatus::Pending)
            .collect();
        out.sort_by_key(|c| c.first_seen);
        out
    }

    /// The computer an approval-request id (or runtime id) names.
    #[must_use]
    pub fn find(&self, account: &str, id_or_runtime: &str) -> Option<Computer> {
        self.list(account)
            .into_iter()
            .find(|c| c.runtime == id_or_runtime || c.request_id() == id_or_runtime)
    }

    /// The owner's decision on a computer (by approval-request id or
    /// runtime id). Returns the computer as it was before (its held
    /// conversations included). Deciding again changes the decision:
    /// approving a denied computer, or denying an approved one.
    pub fn decide(
        &self,
        account: &str,
        id_or_runtime: &str,
        approve: bool,
        via: &str,
    ) -> anyhow::Result<Computer> {
        self.with(account, |f| {
            let Some(c) = f
                .computers
                .iter_mut()
                .find(|c| c.runtime == id_or_runtime || c.request_id() == id_or_runtime)
            else {
                return (
                    Err(anyhow::anyhow!("No computer {id_or_runtime} has asked.")),
                    false,
                );
            };
            let before = c.clone();
            c.status = if approve {
                ComputerStatus::Approved
            } else {
                ComputerStatus::Denied
            };
            c.decided_at = Some(Utc::now());
            c.via = Some(via.to_string());
            c.held_channels.clear();
            (Ok(before), true)
        })
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
    fn an_unknown_computer_waits_until_the_owner_approves_it() {
        let (_dir, computers) = store();
        assert_eq!(
            computers.admit("acct", "did:key:zNew", Some("chan_a")),
            Admission::Held
        );
        assert_eq!(
            computers.admit("acct", "did:key:zNew", Some("chan_b")),
            Admission::Held
        );
        let pending = computers.pending("acct");
        assert_eq!(pending.len(), 1);
        let id = pending[0].request_id();
        assert!(id.starts_with(COMPUTER_ID_PREFIX));

        let before = computers.decide("acct", &id, true, "local").unwrap();
        assert_eq!(before.held_channels, ["chan_a", "chan_b"]);
        assert_eq!(
            computers.admit("acct", "did:key:zNew", Some("chan_a")),
            Admission::Approved
        );
        assert!(computers.pending("acct").is_empty());
    }

    #[test]
    fn a_denied_computer_is_refused_without_asking_again() {
        let (_dir, computers) = store();
        computers.admit("acct", "did:key:zNew", None);
        computers
            .decide("acct", "did:key:zNew", false, "local")
            .unwrap();
        assert_eq!(
            computers.admit("acct", "did:key:zNew", None),
            Admission::Denied
        );
        assert!(computers.pending("acct").is_empty());
    }

    #[test]
    fn the_list_survives_a_restart_and_belongs_to_one_account() {
        let (dir, computers) = store();
        computers.seed("acct", ["did:key:zOld".to_string()]);
        assert!(!computers.needs_seed("acct"));
        let path = dir.path().join("own_computers.json");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let reopened = OwnComputers::new(path);
        assert_eq!(
            reopened.admit("acct", "did:key:zOld", None),
            Admission::Approved
        );
        // Another account: nothing carries over.
        assert!(reopened.needs_seed("other"));
        assert_eq!(
            reopened.admit("other", "did:key:zOld", None),
            Admission::Held
        );
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
