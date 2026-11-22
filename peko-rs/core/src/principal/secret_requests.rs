//! A peko asks its owner for a secret (ADR-116).
//!
//! A request is a runtime-created record in the principal's local tier
//! (`secret_requests/<id>.json`), announced in the owner's conversation
//! and answered only by the owner's own action: the desktop card or
//! `peko secret set` (IPC `secret_provide` / `secret_decline`). The value
//! goes straight to the vault, as a peko secret
//! (`secret:principal:<id>`, named by its environment variable) or into
//! one of the peko's MCP server credential slots. The session that
//! asked learns the answer, never the value; `Bash` sets a peko secret
//! as an environment variable only for a call that names it, and takes
//! its value out of what the command prints.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock, Weak};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::common::vault::{Credential, CredentialFilter, CredentialKind, Vault};
use crate::principal::manager::PrincipalManager;
use crate::principal::Principal;

/// How a peko's notice of a secret request starts, in the owner's
/// conversation with it.
pub const SECRET_NOTICE_PREFIX: &str = "🔑 Secret requested";

/// How long a request waits for the owner.
pub const SECRET_REQUEST_TTL_HOURS: i64 = 24;

/// Most requests one peko may have waiting.
pub const MAX_WAITING: usize = 5;

/// Longest reason a request may give.
pub const MAX_WHY_CHARS: usize = 300;

/// Longest secret name (an environment variable name).
pub const MAX_NAME_CHARS: usize = 64;

/// Values shorter than this are not taken out of command output: they
/// would hit ordinary text (the ledger redactor's limit too).
pub const MIN_REDACTED_LEN: usize = 8;

/// The tool's wire name.
pub const SECRET_TOOL_NAME: &str = "Secret";

/// The vault namespace of a peko's own secrets.
#[must_use]
pub fn peko_namespace(principal_id: &str) -> String {
    format!("secret:principal:{principal_id}")
}

/// The vault namespace of one of a peko's MCP servers (as the MCP
/// service reads it).
#[must_use]
pub fn mcp_namespace(principal_id: &str, server: &str) -> String {
    format!("mcp:principal:{principal_id}:{server}")
}

/// What a request asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// A peko secret, set as environment variable `name` for the `Bash`
    /// calls that name it.
    Peko { name: String },
    /// Credential slot `slot` of MCP server `server` in the workspace.
    Mcp { server: String, slot: String },
}

impl Target {
    /// Build and check a target from what the tool was given.
    pub fn parse(name: &str, server: Option<&str>) -> Result<Self> {
        let name = name.trim();
        match server.map(str::trim).filter(|s| !s.is_empty()) {
            Some(server) => {
                crate::extensions::mcp::service::config::validate_id(server)?;
                crate::extensions::mcp::service::config::validate_id(name)?;
                Ok(Self::Mcp {
                    server: server.to_string(),
                    slot: name.to_string(),
                })
            }
            None => {
                validate_env_name(name)?;
                Ok(Self::Peko {
                    name: name.to_string(),
                })
            }
        }
    }

    /// `GITHUB_TOKEN`, or `notion/api_token` for an MCP slot.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Peko { name } => name.clone(),
            Self::Mcp { server, slot } => format!("{server}/{slot}"),
        }
    }

    /// How the owner reads it in a notice.
    fn described(&self) -> String {
        match self {
            Self::Peko { name } => format!("`{name}`"),
            Self::Mcp { server, slot } => {
                format!("`{slot}` for its MCP server `{server}`")
            }
        }
    }

    fn vault_slot(&self, principal_id: &str) -> (String, String) {
        match self {
            Self::Peko { name } => (peko_namespace(principal_id), name.clone()),
            Self::Mcp { server, slot } => (mcp_namespace(principal_id, server), slot.clone()),
        }
    }
}

/// A peko secret's name is an environment variable name.
pub fn validate_env_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    anyhow::ensure!(
        chars.next().is_some_and(|c| c.is_ascii_uppercase())
            && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            && name.len() <= MAX_NAME_CHARS,
        "a secret's name is an environment variable name: uppercase letters, digits and `_`, \
         starting with a letter, at most {MAX_NAME_CHARS} characters (e.g. GITHUB_TOKEN)"
    );
    Ok(())
}

/// Where a request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Waiting,
    Saved,
    Declined,
}

/// One stored request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretRequest {
    /// `sec_…`
    pub id: String,
    pub target: Target,
    /// Why the peko needs it, shown to the owner.
    pub why: String,
    /// The session that asked, told when the owner answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub status: Status,
}

impl SecretRequest {
    fn is_waiting(&self, now: DateTime<Utc>) -> bool {
        self.status == Status::Waiting && self.expires_at > now
    }
}

// ─── The desk: the vault and inboxes the daemon lends ──────────────

/// What answering a request needs from the daemon.
pub struct SecretDesk {
    vault: Arc<Vault>,
    inbox: Arc<peko_session::InboxRegistry>,
    manager: Weak<PrincipalManager>,
}

static DESK: OnceLock<RwLock<Option<Arc<SecretDesk>>>> = OnceLock::new();

fn desk_slot() -> &'static RwLock<Option<Arc<SecretDesk>>> {
    DESK.get_or_init(|| RwLock::new(None))
}

/// Install the desk; the daemon calls this once its vault, inboxes and
/// principals exist. Replacing is allowed (tests, restarts in one
/// process).
pub fn install_secret_desk(
    vault: Arc<Vault>,
    inbox: Arc<peko_session::InboxRegistry>,
    manager: Weak<PrincipalManager>,
) {
    *desk_slot().write().unwrap_or_else(|e| e.into_inner()) =
        Some(Arc::new(SecretDesk::new(vault, inbox, manager)));
}

/// The installed desk, if any (none in CLI one-shots and most tests).
#[must_use]
pub fn secret_desk() -> Option<Arc<SecretDesk>> {
    desk_slot()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

impl SecretDesk {
    #[must_use]
    pub fn new(
        vault: Arc<Vault>,
        inbox: Arc<peko_session::InboxRegistry>,
        manager: Weak<PrincipalManager>,
    ) -> Self {
        Self {
            vault,
            inbox,
            manager,
        }
    }

    /// Store a peko secret directly (tests; the owner's path is
    /// [`provide`]).
    #[cfg(test)]
    pub(crate) fn store_peko_secret(&self, principal_id: &str, name: &str, value: &str) {
        let target = Target::parse(name, None).unwrap();
        self.store(principal_id, &target, SecretString::from(value.to_string()))
            .unwrap();
    }

    /// The vault as it is on disk now: `peko credential` writes the
    /// file directly.
    fn fresh_vault(&self) -> &Vault {
        if let Err(e) = self.vault.reload_unlocked() {
            tracing::debug!("secret desk: vault reload failed, using what is loaded: {e}");
        }
        &self.vault
    }

    /// A loaded principal by id.
    pub async fn principal(&self, principal_id: &str) -> Option<Arc<Principal>> {
        let manager = self.manager.upgrade()?;
        crate::daemon::cron_engine::resolve_principal(
            &manager,
            &peko_subject::PrincipalId(principal_id.to_string()),
        )
        .await
    }

    /// The names of a peko's secrets, sorted.
    #[must_use]
    pub fn secret_names(&self, principal_id: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .fresh_vault()
            .list_credentials(&CredentialFilter {
                namespace: Some(peko_namespace(principal_id)),
                ..Default::default()
            })
            .into_iter()
            .map(|c| c.name)
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// A peko secret's value, for the `Bash` call that names it.
    pub fn peko_secret(&self, principal_id: &str, name: &str) -> Result<Option<SecretString>> {
        validate_env_name(name)?;
        Ok(self
            .fresh_vault()
            .get_material_for(&peko_namespace(principal_id), name)?)
    }

    fn store(&self, principal_id: &str, target: &Target, value: SecretString) -> Result<()> {
        let (namespace, name) = target.vault_slot(principal_id);
        let vault = self.fresh_vault();
        let mut credential = Credential::now(
            namespace.clone(),
            name.clone(),
            CredentialKind::GenericSecret,
            value,
        );
        if let Some(existing) = vault
            .list_credentials(&CredentialFilter {
                namespace: Some(namespace),
                ..Default::default()
            })
            .into_iter()
            .find(|c| c.name == name)
        {
            credential.id = existing.id;
        }
        vault.set_credential(&credential)
    }

    /// Remove a peko secret; `false` when it had none of that name.
    pub fn delete(&self, principal_id: &str, name: &str) -> Result<bool> {
        validate_env_name(name)?;
        let vault = self.fresh_vault();
        let mut removed = false;
        for credential in vault.list_credentials(&CredentialFilter {
            namespace: Some(peko_namespace(principal_id)),
            ..Default::default()
        }) {
            if credential.name == name {
                removed |= vault.delete_credential(&credential.id)?;
            }
        }
        Ok(removed)
    }

    /// Tell the session that asked how its request was answered, and
    /// wake it if it is idle.
    async fn tell_session(&self, principal_id: &str, record: &SecretRequest, text: String) {
        let Some(session) = record.session_id.clone() else {
            return;
        };
        let result = serde_json::json!({
            "secret": record.target.label(),
            "status": record.status,
            "note": text,
        });
        let event = peko_session::CompletionEvent {
            task_id: format!("secret:{}", record.id),
            tool_name: SECRET_TOOL_NAME.to_string(),
            result: result.clone(),
            status: peko_tools_core::AsyncTaskStatus::Completed {
                result: peko_tools_core::ToolResult::success(result),
            },
            completed_at: Utc::now(),
            parent_session_key: session.clone(),
        };
        let inbox = self
            .inbox
            .get_or_create(&peko_session::inbox_key(principal_id, &session))
            .await;
        inbox
            .push(peko_session::InboxItem::Completion(event).into())
            .await;
        crate::async_exec::executor::wake::notify_completion_wake(
            crate::async_exec::executor::wake::CompletionWakeNotice {
                session_key: session,
                task_id: format!("secret:{}", record.id),
                tool_name: SECRET_TOOL_NAME.to_string(),
                principal_id: Some(principal_id.to_string()),
                via_steering: false,
            },
        );
    }
}

// ─── Records ───────────────────────────────────────────────────────

fn requests_dir(principal: &Principal) -> PathBuf {
    // `<local>/sessions` is the sessions dir; requests sit beside it.
    let sessions = principal.memory.sessions_dir();
    sessions.parent().map_or_else(
        || sessions.join("secret_requests"),
        |local| local.join("secret_requests"),
    )
}

fn record_path(dir: &Path, id: &str) -> Option<PathBuf> {
    let valid = id
        .strip_prefix("sec_")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric()));
    valid.then(|| dir.join(format!("{id}.json")))
}

fn load_record(dir: &Path, id: &str) -> Option<SecretRequest> {
    let path = record_path(dir, id)?;
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn save_record(dir: &Path, record: &SecretRequest) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = record_path(dir, &record.id).context("invalid secret request id")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(record)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn waiting_in(dir: &Path) -> Vec<SecretRequest> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let now = Utc::now();
    let mut records: Vec<SecretRequest> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .filter(|r: &SecretRequest| r.is_waiting(now))
        .collect();
    records.sort_by_key(|r| r.created_at);
    records
}

/// The requests a peko has waiting, oldest first.
#[must_use]
pub fn waiting(principal: &Principal) -> Vec<SecretRequest> {
    waiting_in(&requests_dir(principal))
}

/// Store a request, or return the one already waiting for the same
/// target. `Ok((record, new))`.
fn file_request(
    dir: &Path,
    target: Target,
    why: &str,
    session_id: Option<String>,
) -> Result<(SecretRequest, bool)> {
    let why = why.trim();
    anyhow::ensure!(!why.is_empty(), "say why the secret is needed (`why`)");
    anyhow::ensure!(
        why.chars().count() <= MAX_WHY_CHARS,
        "`why` is at most {MAX_WHY_CHARS} characters"
    );
    let waiting = waiting_in(dir);
    if let Some(existing) = waiting.iter().find(|r| r.target == target) {
        return Ok((existing.clone(), false));
    }
    anyhow::ensure!(
        waiting.len() < MAX_WAITING,
        "{} secret requests already wait for the owner; no more are taken until some are \
         answered",
        waiting.len()
    );
    let now = Utc::now();
    let record = SecretRequest {
        id: format!("sec_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
        target,
        why: peko_tools_core::neutralize_runtime_tags(why),
        session_id,
        created_at: now,
        expires_at: now + chrono::Duration::hours(SECRET_REQUEST_TTL_HOURS),
        status: Status::Waiting,
    };
    save_record(dir, &record)?;
    Ok((record, true))
}

/// Ask the owner for a secret (the `Secret` tool's `request`).
pub async fn request(
    principal: &Principal,
    target: Target,
    why: &str,
    session_id: Option<String>,
) -> Result<SecretRequest> {
    let (record, new) = file_request(&requests_dir(principal), target, why, session_id)?;
    if new {
        notify_owner(principal, &record).await;
    }
    Ok(record)
}

/// Tell the owner a request waits; best-effort (`secret_list` lists it
/// either way).
async fn notify_owner(principal: &Principal, record: &SecretRequest) {
    let Some(messenger) = crate::principal::messenger::global_messenger() else {
        return;
    };
    let peko = principal.name().await;
    let note = format!(
        "{SECRET_NOTICE_PREFIX} ({id}): {peko} asks for {what}. Why: {why}\n\nEnter it in the \
         Peko app, or run `peko secret set {peko} {id}`; don't paste it here. Expires {exp}.",
        id = record.id,
        what = record.target.described(),
        why = record.why,
        exp = record.expires_at.format("%Y-%m-%d %H:%M UTC"),
    );
    let owner = principal.config.read().await.owner.clone();
    let delivered = messenger
        .deliver_note(
            &principal.id.0,
            &owner,
            &note,
            None,
            peko_session::events::MessageSource::Agent,
            Some("secrets"),
        )
        .await;
    if !matches!(delivered, Ok(true)) {
        tracing::warn!(id = %record.id, "secret request stored; no owner conversation to announce it in");
    }
}

/// Find the request `key` names: its id, or the label of a waiting one.
fn find(dir: &Path, key: &str) -> Option<SecretRequest> {
    let key = key.trim();
    if key.starts_with("sec_") {
        return load_record(dir, key);
    }
    waiting_in(dir)
        .into_iter()
        .find(|r| r.target.label() == key)
}

/// The owner's value for request `key` (an id, or a waiting request's
/// name), or for a new peko secret of that name when none waits.
/// Returns what to tell the owner.
pub async fn provide(
    desk: &SecretDesk,
    principal: &Principal,
    key: &str,
    value: SecretString,
) -> Result<String> {
    anyhow::ensure!(
        !value.expose_secret().trim().is_empty(),
        "the secret is empty"
    );
    let dir = requests_dir(principal);
    let pid = principal.id.0.as_str();
    let Some(mut record) = find(&dir, key) else {
        anyhow::ensure!(!key.trim().starts_with("sec_"), "no secret request {key}");
        let target = Target::parse(key, None)?;
        desk.store(pid, &target, value)?;
        return Ok(format!(
            "Saved {} for {}.",
            target.label(),
            principal.name().await
        ));
    };
    anyhow::ensure!(
        record.is_waiting(Utc::now()),
        "request {} is no longer waiting",
        record.id
    );
    desk.store(pid, &record.target, value)?;
    record.status = Status::Saved;
    save_record(&dir, &record)?;
    let label = record.target.label();
    let note = match &record.target {
        Target::Peko { name } => format!(
            "The owner saved {name}. Pass `secrets: [\"{name}\"]` to the `Bash` calls that need \
             it: it is set as that environment variable, and its value is taken out of their \
             output. Never print it."
        ),
        Target::Mcp { server, slot } => format!(
            "The owner saved `{slot}` for the MCP server `{server}`; the server reads it the \
             next time it starts."
        ),
    };
    desk.tell_session(pid, &record, note).await;
    Ok(format!("Saved {label} for {}.", principal.name().await))
}

/// The owner declines request `key`.
pub async fn decline(desk: &SecretDesk, principal: &Principal, key: &str) -> Result<String> {
    let dir = requests_dir(principal);
    let mut record = find(&dir, key).with_context(|| format!("no secret request {key}"))?;
    anyhow::ensure!(
        record.is_waiting(Utc::now()),
        "request {} is no longer waiting",
        record.id
    );
    record.status = Status::Declined;
    save_record(&dir, &record)?;
    let note = format!(
        "The owner declined to give {}. Go on without it, or say what you can't do.",
        record.target.label()
    );
    desk.tell_session(&principal.id.0, &record, note).await;
    Ok(format!("Declined {}.", record.target.label()))
}

// ─── Output redaction ──────────────────────────────────────────────

/// `text` with every value of 8 or more characters replaced by
/// `[secret NAME]`, longest first.
#[must_use]
pub fn redact(text: &str, secrets: &[(String, String)]) -> String {
    let mut sorted: Vec<&(String, String)> = secrets
        .iter()
        .filter(|(_, value)| value.len() >= MIN_REDACTED_LEN)
        .collect();
    sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
    let mut out = text.to_string();
    for (name, value) in sorted {
        if out.contains(value.as_str()) {
            out = out.replace(value.as_str(), &format!("[secret {name}]"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_environment_variables() {
        for ok in ["GITHUB_TOKEN", "A", "X1_2"] {
            validate_env_name(ok).unwrap();
        }
        for bad in [
            "",
            "github_token",
            "1X",
            "_X",
            "A-B",
            "A B",
            &"A".repeat(65),
        ] {
            assert!(validate_env_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn targets_parse_and_label() {
        assert_eq!(
            Target::parse(" GITHUB_TOKEN ", None).unwrap(),
            Target::Peko {
                name: "GITHUB_TOKEN".into()
            }
        );
        let mcp = Target::parse("api_token", Some("notion")).unwrap();
        assert_eq!(mcp.label(), "notion/api_token");
        assert_eq!(
            mcp.vault_slot("prin_a"),
            ("mcp:principal:prin_a:notion".into(), "api_token".into())
        );
        assert!(Target::parse("a/b", Some("notion")).is_err());
        assert!(Target::parse("lower", None).is_err());
    }

    #[test]
    fn a_request_waits_once_per_target_and_at_most_five() {
        let dir = tempfile::tempdir().unwrap();
        let t = |n: &str| Target::parse(n, None).unwrap();
        let (first, new) =
            file_request(dir.path(), t("A_KEY"), "for gh", Some("s".into())).unwrap();
        assert!(new && first.id.starts_with("sec_"));
        let (again, new) = file_request(dir.path(), t("A_KEY"), "again", None).unwrap();
        assert!(!new);
        assert_eq!(again.id, first.id);
        for n in ["B_KEY", "C_KEY", "D_KEY", "E_KEY"] {
            file_request(dir.path(), t(n), "x", None).unwrap();
        }
        let err = file_request(dir.path(), t("F_KEY"), "x", None).unwrap_err();
        assert!(err.to_string().contains("already wait"), "{err}");
        assert!(file_request(dir.path(), t("G_KEY"), "  ", None).is_err());
        assert!(file_request(dir.path(), t("G_KEY"), &"w".repeat(301), None).is_err());
        assert_eq!(waiting_in(dir.path()).len(), 5);
        assert_eq!(find(dir.path(), "A_KEY").unwrap().id, first.id);
        assert_eq!(find(dir.path(), &first.id).unwrap().id, first.id);
        assert!(find(dir.path(), "sec_../x").is_none());
    }

    #[test]
    fn lapsed_and_answered_requests_no_longer_wait() {
        let dir = tempfile::tempdir().unwrap();
        let (mut record, _) =
            file_request(dir.path(), Target::parse("A_KEY", None).unwrap(), "x", None).unwrap();
        record.status = Status::Saved;
        save_record(dir.path(), &record).unwrap();
        assert!(waiting_in(dir.path()).is_empty());
        let (mut lapsed, _) =
            file_request(dir.path(), Target::parse("B_KEY", None).unwrap(), "x", None).unwrap();
        lapsed.expires_at = Utc::now() - chrono::Duration::minutes(1);
        save_record(dir.path(), &lapsed).unwrap();
        assert!(waiting_in(dir.path()).is_empty());
    }

    #[test]
    fn redaction_replaces_long_values_longest_first() {
        let secrets = vec![
            ("SHORT".to_string(), "abc".to_string()),
            ("TOKEN".to_string(), "ghp_abcdefgh".to_string()),
            ("LONGER".to_string(), "ghp_abcdefgh_more".to_string()),
        ];
        assert_eq!(
            redact("x ghp_abcdefgh_more y ghp_abcdefgh abc", &secrets),
            "x [secret LONGER] y [secret TOKEN] abc"
        );
    }

    #[test]
    fn the_desk_stores_replaces_lists_and_deletes_peko_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::for_test(dir.path(), "test-passphrase"));
        let desk = SecretDesk {
            vault,
            inbox: Arc::new(peko_session::InboxRegistry::new(
                crate::async_exec::executor::executor::default_inbox_factory(),
            )),
            manager: Weak::new(),
        };
        let target = Target::parse("GITHUB_TOKEN", None).unwrap();
        desk.store(
            "prin_a",
            &target,
            SecretString::from("one-value".to_string()),
        )
        .unwrap();
        desk.store(
            "prin_a",
            &target,
            SecretString::from("two-value".to_string()),
        )
        .unwrap();
        assert_eq!(desk.secret_names("prin_a"), vec!["GITHUB_TOKEN"]);
        assert!(desk.secret_names("prin_b").is_empty());
        let value = desk.peko_secret("prin_a", "GITHUB_TOKEN").unwrap().unwrap();
        assert_eq!(value.expose_secret(), "two-value");
        assert!(desk
            .peko_secret("prin_b", "GITHUB_TOKEN")
            .unwrap()
            .is_none());
        assert!(desk.delete("prin_a", "GITHUB_TOKEN").unwrap());
        assert!(!desk.delete("prin_a", "GITHUB_TOKEN").unwrap());
        assert!(desk.secret_names("prin_a").is_empty());
    }
}
