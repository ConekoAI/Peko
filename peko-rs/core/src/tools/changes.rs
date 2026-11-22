//! What each turn changed on disk, so the owner can undo it (ADR-117).
//!
//! Per principal, in its local tier (`<local>/changes/`):
//!
//! - `turns/<run_id>.json`: one record per run that changed a file —
//!   per file its state before and after the turn;
//! - `blobs/<sha256>`: the bytes of every version kept, deduplicated.
//!
//! `Write` and `Edit` name the file they write through a recorder on the
//! call's context; a `Bash` call's changes inside the workspace are the
//! difference of two scans around it. Undo puts each file back only if
//! it is still exactly as the turn left it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Larger files are recorded as changed but not kept.
pub const MAX_SAVED_BYTES: u64 = 5 * 1024 * 1024;

/// How long turns and the bytes only they use are kept.
pub const KEEP_DAYS: i64 = 7;

/// Folders a workspace scan skips: version control, dependencies,
/// build output, and triage source caches.
const SKIPPED_DIRS: &[&str] = &[".git", "node_modules", "target", ".triage", ".venv"];

/// A scan gives up past this many files and records nothing for the
/// call (a workspace that large is not the peko's own notes).
const MAX_SCANNED_FILES: usize = 20_000;

/// What a file was at one moment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FileState {
    Absent,
    Saved { hash: String, size: u64 },
    NotSaved { reason: String },
}

impl FileState {
    fn same_content(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Absent, Self::Absent) => true,
            (Self::Saved { hash: a, .. }, Self::Saved { hash: b, .. }) => a == b,
            _ => false,
        }
    }
}

/// One file a turn changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    /// Absolute path.
    pub path: String,
    pub before: FileState,
    pub after: FileState,
}

impl FileChange {
    /// `added`, `deleted`, or `modified`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match (&self.before, &self.after) {
            (FileState::Absent, _) => "added",
            (_, FileState::Absent) => "deleted",
            _ => "modified",
        }
    }
}

/// A file undo left alone, and why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skipped {
    pub path: String,
    pub why: String,
}

/// What an undo did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Undone {
    pub at: DateTime<Utc>,
    pub restored: Vec<String>,
    pub skipped: Vec<Skipped>,
}

/// One run's changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    /// The run id.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The conversation it ran in, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Whom it acted for.
    pub actor: String,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub files: Vec<FileChange>,
    /// It ran shell commands: it may have changed files outside the
    /// workspace that are not listed.
    #[serde(default)]
    pub shell: bool,
    /// Runs it delegated (foreground `Agent` calls); undone with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delegated: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undone: Option<Undone>,
}

/// Which turn a call belongs to.
#[derive(Debug, Clone)]
pub struct TurnKey {
    pub run_id: String,
    pub session_id: Option<String>,
    pub channel: Option<String>,
    pub actor: String,
}

/// A workspace scan: every file's state.
pub type Scan = BTreeMap<PathBuf, FileState>;

#[derive(Clone)]
struct Indexed {
    size: u64,
    modified: Option<std::time::SystemTime>,
    state: FileState,
}

/// A principal's change log.
pub struct ChangeLog {
    dir: PathBuf,
    workspace: PathBuf,
    /// Serializes record updates.
    lock: Mutex<()>,
    /// Size and mtime of each scanned file, so unchanged files are not
    /// read again.
    index: Mutex<HashMap<PathBuf, Indexed>>,
}

impl std::fmt::Debug for ChangeLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeLog")
            .field("dir", &self.dir)
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

/// Where a principal's change log lives, beside its sessions.
#[must_use]
pub fn dir_for_sessions(sessions: &Path) -> PathBuf {
    sessions
        .parent()
        .map_or_else(|| sessions.join("changes"), |local| local.join("changes"))
}

impl ChangeLog {
    /// Open (creating) the log in `dir` for a peko whose workspace is
    /// `workspace`, and drop what is older than [`KEEP_DAYS`].
    pub fn open(dir: &Path, workspace: &Path) -> Result<Self> {
        let log = Self {
            dir: dir.to_path_buf(),
            workspace: workspace.to_path_buf(),
            lock: Mutex::new(()),
            index: Mutex::new(HashMap::new()),
        };
        log.prune(Utc::now() - chrono::Duration::days(KEEP_DAYS));
        Ok(log)
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.dir.join("blobs").join(hash)
    }

    fn turn_path(&self, run_id: &str) -> Option<PathBuf> {
        let valid = !run_id.is_empty()
            && run_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
        valid.then(|| self.dir.join("turns").join(format!("{run_id}.json")))
    }

    /// Hash `bytes` and keep them.
    fn keep(&self, bytes: &[u8]) -> Result<String> {
        let hash = hex(&Sha256::digest(bytes));
        let path = self.blob_path(&hash);
        if !path.exists() {
            std::fs::create_dir_all(self.dir.join("blobs"))?;
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(tmp, &path)?;
        }
        Ok(hash)
    }

    /// The state of `path` now, keeping its bytes (`keep`) or only
    /// hashing them.
    fn state_of(&self, path: &Path, keep: bool) -> FileState {
        let meta = match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => meta,
            Ok(_) => {
                return FileState::NotSaved {
                    reason: "not a regular file".into(),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FileState::Absent,
            Err(e) => {
                return FileState::NotSaved {
                    reason: format!("unreadable: {e}"),
                }
            }
        };
        if meta.len() > MAX_SAVED_BYTES {
            return FileState::NotSaved {
                reason: format!("larger than {} MB", MAX_SAVED_BYTES / (1024 * 1024)),
            };
        }
        match std::fs::read(path) {
            Ok(bytes) => {
                let hash = if keep {
                    match self.keep(&bytes) {
                        Ok(hash) => hash,
                        Err(e) => {
                            return FileState::NotSaved {
                                reason: format!("could not be kept: {e}"),
                            }
                        }
                    }
                } else {
                    hex(&Sha256::digest(&bytes))
                };
                FileState::Saved {
                    hash,
                    size: bytes.len() as u64,
                }
            }
            Err(e) => FileState::NotSaved {
                reason: format!("unreadable: {e}"),
            },
        }
    }

    /// Every file in the workspace, keeping the bytes of new versions.
    /// `None` when the workspace is too large to scan.
    #[must_use]
    pub fn scan(&self) -> Option<Scan> {
        let mut files = Vec::new();
        let mut stack = vec![self.workspace.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                if kind.is_dir() {
                    let skipped = entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| SKIPPED_DIRS.contains(&name));
                    if !skipped {
                        stack.push(path);
                    }
                } else if kind.is_file() {
                    files.push(path);
                    if files.len() > MAX_SCANNED_FILES {
                        return None;
                    }
                }
            }
        }
        let mut index = self.index.lock().unwrap_or_else(|e| e.into_inner());
        let mut scan = Scan::new();
        for path in files {
            let meta = std::fs::metadata(&path).ok();
            let size = meta.as_ref().map_or(0, std::fs::Metadata::len);
            let modified = meta.as_ref().and_then(|m| m.modified().ok());
            let cached = index
                .get(&path)
                .filter(|i| i.size == size && i.modified == modified && modified.is_some())
                .map(|i| i.state.clone());
            let state = match cached {
                Some(state) => state,
                None => {
                    let state = self.state_of(&path, true);
                    index.insert(
                        path.clone(),
                        Indexed {
                            size,
                            modified,
                            state: state.clone(),
                        },
                    );
                    state
                }
            };
            scan.insert(path, state);
        }
        Some(scan)
    }

    /// Change `run_id`'s record, creating it.
    fn update(&self, key: &TurnKey, change: impl FnOnce(&mut Turn)) -> Result<()> {
        let path = self.turn_path(&key.run_id).context("no run id")?;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let now = Utc::now();
        let mut turn = read_json::<Turn>(&path).unwrap_or_else(|| Turn {
            id: key.run_id.clone(),
            session_id: key.session_id.clone(),
            channel: key.channel.clone(),
            actor: key.actor.clone(),
            started_at: now,
            updated_at: now,
            files: Vec::new(),
            shell: false,
            delegated: Vec::new(),
            undone: None,
        });
        change(&mut turn);
        turn.updated_at = now;
        if turn.files.is_empty() && !turn.shell && turn.delegated.is_empty() {
            // Nothing left to undo (a file put back where it started).
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        write_json(&path, &turn)
    }

    /// Fold one file's change into `key`'s turn: its first `before`, its
    /// latest `after`; a file back where it started drops out.
    fn note(&self, key: &TurnKey, path: &Path, before: FileState, after: FileState) -> Result<()> {
        let shown = path.display().to_string();
        let existing = self
            .turn_path(&key.run_id)
            .and_then(|p| read_json::<Turn>(&p))
            .is_some_and(|t| t.files.iter().any(|f| f.path == shown));
        if !existing && before.same_content(&after) {
            return Ok(());
        }
        self.update(key, |turn| {
            match turn.files.iter_mut().find(|f| f.path == shown) {
                Some(file) => file.after = after,
                None => turn.files.push(FileChange {
                    path: shown.clone(),
                    before,
                    after,
                }),
            }
            turn.files
                .retain(|f| !(f.before.same_content(&f.after) && f.path == shown));
        })
    }

    /// Record a `Bash` call: what changed between two scans, and that
    /// the turn ran shell commands.
    pub fn record_scan(&self, key: &TurnKey, before: &Scan, after: &Scan) -> Result<()> {
        let paths: std::collections::BTreeSet<&PathBuf> =
            before.keys().chain(after.keys()).collect();
        for path in paths {
            let was = before.get(path).cloned().unwrap_or(FileState::Absent);
            let now = after.get(path).cloned().unwrap_or(FileState::Absent);
            if was != now {
                self.note(key, path, was, now)?;
            }
        }
        self.update(key, |turn| turn.shell = true)
    }

    /// Note that `parent` delegated `child` (a foreground `Agent` call).
    pub fn link(&self, parent: &TurnKey, child: &str) -> Result<()> {
        let child_has_changes = self
            .turn_path(child)
            .and_then(|p| read_json::<Turn>(&p))
            .is_some();
        if !child_has_changes || parent.run_id == child {
            return Ok(());
        }
        self.update(parent, |turn| {
            if !turn.delegated.iter().any(|c| c == child) {
                turn.delegated.push(child.to_string());
            }
        })
    }

    /// One turn.
    #[must_use]
    pub fn turn(&self, run_id: &str) -> Option<Turn> {
        read_json(&self.turn_path(run_id)?)
    }

    /// Recent turns, newest first: those in `channel` when given.
    /// Turns another turn delegated are folded into it.
    #[must_use]
    pub fn turns(&self, channel: Option<&str>, limit: usize) -> Vec<Turn> {
        let Ok(entries) = std::fs::read_dir(self.dir.join("turns")) else {
            return Vec::new();
        };
        let all: Vec<Turn> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| read_json::<Turn>(&e.path()))
            .collect();
        let delegated: HashSet<&str> = all
            .iter()
            .flat_map(|t| t.delegated.iter().map(String::as_str))
            .collect();
        let mut turns: Vec<Turn> = all
            .iter()
            .filter(|t| !delegated.contains(t.id.as_str()))
            .filter(|t| channel.is_none_or(|c| t.channel.as_deref() == Some(c)))
            .cloned()
            .collect();
        turns.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        turns.truncate(limit);
        turns
    }

    /// Every file a turn changed, its delegated runs' included.
    #[must_use]
    pub fn all_files(&self, turn: &Turn) -> Vec<FileChange> {
        let mut files = turn.files.clone();
        let mut seen: HashSet<String> = HashSet::from([turn.id.clone()]);
        let mut queue = turn.delegated.clone();
        while let Some(id) = queue.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(child) = self.turn(&id) {
                files.extend(child.files);
                queue.extend(child.delegated);
            }
        }
        files
    }

    /// Put back what turn `run_id` (and the runs it delegated) changed,
    /// where each file is still as it left it.
    pub fn undo(&self, run_id: &str) -> Result<Undone> {
        let turn = self
            .turn(run_id)
            .with_context(|| format!("no recorded changes for turn {run_id}"))?;
        anyhow::ensure!(turn.undone.is_none(), "turn {run_id} was already undone");
        let mut ids = vec![turn.id.clone()];
        let mut queue = turn.delegated.clone();
        let mut turns = vec![turn];
        while let Some(id) = queue.pop() {
            if ids.contains(&id) {
                continue;
            }
            ids.push(id.clone());
            if let Some(child) = self.turn(&id).filter(|t| t.undone.is_none()) {
                queue.extend(child.delegated.clone());
                turns.push(child);
            }
        }
        let mut restored = Vec::new();
        let mut skipped = Vec::new();
        // Newest first, so a file several of them changed ends at the
        // oldest `before`.
        turns.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        for turn in &turns {
            for file in &turn.files {
                let path = Path::new(&file.path);
                let now = self.state_of(path, false);
                if !now.same_content(&file.after) {
                    skipped.push(Skipped {
                        path: file.path.clone(),
                        why: "changed since".into(),
                    });
                    continue;
                }
                match self.restore(path, &file.before) {
                    Ok(()) => restored.push(file.path.clone()),
                    Err(e) => skipped.push(Skipped {
                        path: file.path.clone(),
                        why: e.to_string(),
                    }),
                }
            }
        }
        let undone = Undone {
            at: Utc::now(),
            restored,
            skipped,
        };
        for turn in &turns {
            let key = TurnKey {
                run_id: turn.id.clone(),
                session_id: turn.session_id.clone(),
                channel: turn.channel.clone(),
                actor: turn.actor.clone(),
            };
            let record = undone.clone();
            self.update(&key, |t| t.undone = Some(record))?;
        }
        Ok(undone)
    }

    fn restore(&self, path: &Path, state: &FileState) -> Result<()> {
        match state {
            FileState::Absent => match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            },
            FileState::Saved { hash, .. } => {
                let bytes = std::fs::read(self.blob_path(hash))
                    .context("its earlier version is no longer kept")?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let tmp = path.with_extension("peko-undo.tmp");
                std::fs::write(&tmp, bytes)?;
                std::fs::rename(&tmp, path)?;
                // A later scan must read it again.
                self.index
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(path);
                Ok(())
            }
            FileState::NotSaved { reason } => {
                anyhow::bail!("its earlier version was not kept ({reason})")
            }
        }
    }

    /// Drop turns last changed before `cutoff`, and the bytes no turn
    /// or scan still uses.
    pub fn prune(&self, cutoff: DateTime<Utc>) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut used = HashSet::new();
        if let Ok(entries) = std::fs::read_dir(self.dir.join("turns")) {
            for entry in entries.flatten() {
                let Some(turn) = read_json::<Turn>(&entry.path()) else {
                    continue;
                };
                if turn.updated_at < cutoff {
                    let _ = std::fs::remove_file(entry.path());
                    continue;
                }
                for file in &turn.files {
                    for state in [&file.before, &file.after] {
                        if let FileState::Saved { hash, .. } = state {
                            used.insert(hash.clone());
                        }
                    }
                }
            }
        }
        for indexed in self
            .index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            if let FileState::Saved { hash, .. } = &indexed.state {
                used.insert(hash.clone());
            }
        }
        let Ok(blobs) = std::fs::read_dir(self.dir.join("blobs")) else {
            return;
        };
        let cutoff_time = std::time::SystemTime::from(cutoff);
        for blob in blobs.flatten() {
            let name = blob.file_name().to_string_lossy().to_string();
            let old = blob
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|m| m < cutoff_time);
            if old && !used.contains(&name) {
                let _ = std::fs::remove_file(blob.path());
            }
        }
    }
}

impl ChangeLog {
    /// `path` as people read it: relative to the workspace when inside.
    #[must_use]
    pub fn shown_path(&self, path: &str) -> String {
        Path::new(path)
            .strip_prefix(&self.workspace)
            .map_or_else(|_| path.to_string(), |rel| rel.display().to_string())
    }

    /// A turn as tools and clients show it: its files (delegated runs'
    /// included), newest change last.
    #[must_use]
    pub fn summary(&self, turn: &Turn) -> TurnSummary {
        let mut seen = HashSet::new();
        let files = self
            .all_files(turn)
            .into_iter()
            .filter(|f| seen.insert(f.path.clone()))
            .map(|f| ChangedFile {
                change: f.kind().to_string(),
                path: self.shown_path(&f.path),
            })
            .collect();
        TurnSummary {
            turn: turn.id.clone(),
            session_id: turn.session_id.clone(),
            channel: turn.channel.clone(),
            actor: turn.actor.clone(),
            started_at: turn.started_at.to_rfc3339(),
            updated_at: turn.updated_at.to_rfc3339(),
            files,
            shell: turn.shell,
            undone_at: turn.undone.as_ref().map(|u| u.at.to_rfc3339()),
        }
    }
}

/// A turn as shown (`Changes` tool, IPC `turn_changes`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSummary {
    pub turn: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    pub actor: String,
    pub started_at: String,
    pub updated_at: String,
    pub files: Vec<ChangedFile>,
    /// It ran shell commands: changes outside the workspace aren't listed.
    pub shell: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undone_at: Option<String>,
}

/// One file a turn changed, as shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    /// Relative to the workspace when inside it.
    pub path: String,
    /// `added`, `modified`, or `deleted`.
    pub change: String,
}

/// The recorder a `Write` / `Edit` call reports its file to.
pub struct CallRecorder {
    log: Arc<ChangeLog>,
    key: TurnKey,
    pending: Mutex<HashMap<PathBuf, FileState>>,
}

impl CallRecorder {
    #[must_use]
    pub fn new(log: Arc<ChangeLog>, key: TurnKey) -> Self {
        Self {
            log,
            key,
            pending: Mutex::new(HashMap::new()),
        }
    }
}

impl peko_tools_core::FileChangeSink for CallRecorder {
    fn before(&self, path: &Path) {
        let state = self.log.state_of(path, true);
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.to_path_buf(), state);
    }

    fn after(&self, path: &Path) {
        let Some(before) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(path)
        else {
            return;
        };
        let after = self.log.state_of(path, true);
        if let Err(e) = self.log.note(&self.key, path, before, after) {
            tracing::warn!(path = %path.display(), "could not record a file change: {e:#}");
        }
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use peko_tools_core::FileChangeSink;

    fn key(run: &str) -> TurnKey {
        TurnKey {
            run_id: run.into(),
            session_id: Some("s1".into()),
            channel: Some("chan_a".into()),
            actor: "user:local".into(),
        }
    }

    fn setup() -> (tempfile::TempDir, Arc<ChangeLog>) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("ws/kb")).unwrap();
        let log = Arc::new(
            ChangeLog::open(&root.path().join("changes"), &root.path().join("ws")).unwrap(),
        );
        (root, log)
    }

    #[test]
    fn a_written_file_is_recorded_and_undone() {
        let (root, log) = setup();
        let notes = root.path().join("ws/kb/notes.md");
        std::fs::write(&notes, "old").unwrap();
        let created = root.path().join("elsewhere.txt");

        let rec = CallRecorder::new(Arc::clone(&log), key("run1"));
        rec.before(&notes);
        std::fs::write(&notes, "new").unwrap();
        rec.after(&notes);
        rec.before(&created);
        std::fs::write(&created, "made").unwrap();
        rec.after(&created);
        // A second write in the same turn keeps the first `before`.
        rec.before(&notes);
        std::fs::write(&notes, "newer").unwrap();
        rec.after(&notes);

        let turn = log.turn("run1").unwrap();
        assert_eq!(turn.files.len(), 2);
        assert_eq!(turn.files[1].kind(), "added");
        assert_eq!(log.turns(Some("chan_a"), 10).len(), 1);
        assert!(log.turns(Some("chan_b"), 10).is_empty());

        let undone = log.undo("run1").unwrap();
        assert_eq!(undone.restored.len(), 2);
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "old");
        assert!(!created.exists());
        assert!(log
            .undo("run1")
            .unwrap_err()
            .to_string()
            .contains("already undone"));
    }

    #[test]
    fn a_file_changed_since_is_left_alone() {
        let (root, log) = setup();
        let notes = root.path().join("ws/kb/notes.md");
        std::fs::write(&notes, "old").unwrap();
        let rec = CallRecorder::new(Arc::clone(&log), key("run1"));
        rec.before(&notes);
        std::fs::write(&notes, "new").unwrap();
        rec.after(&notes);
        std::fs::write(&notes, "the owner's edit").unwrap();

        let undone = log.undo("run1").unwrap();
        assert!(undone.restored.is_empty());
        assert_eq!(undone.skipped[0].why, "changed since");
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "the owner's edit");
    }

    #[test]
    fn a_shell_call_is_the_difference_of_two_scans() {
        let (root, log) = setup();
        let ws = root.path().join("ws");
        std::fs::write(ws.join("kb/a.md"), "a").unwrap();
        std::fs::write(ws.join("kb/gone.md"), "bye").unwrap();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::write(ws.join(".git/HEAD"), "x").unwrap();
        let before = log.scan().unwrap();
        assert!(!before.contains_key(&ws.join(".git/HEAD")));

        std::fs::write(ws.join("kb/a.md"), "a2").unwrap();
        std::fs::remove_file(ws.join("kb/gone.md")).unwrap();
        std::fs::write(ws.join("kb/new.md"), "n").unwrap();
        std::fs::write(ws.join(".git/HEAD"), "y").unwrap();
        let after = log.scan().unwrap();
        log.record_scan(&key("run2"), &before, &after).unwrap();

        let turn = log.turn("run2").unwrap();
        assert!(turn.shell);
        let mut kinds: Vec<_> = turn.files.iter().map(FileChange::kind).collect();
        kinds.sort_unstable();
        assert_eq!(kinds, ["added", "deleted", "modified"]);

        log.undo("run2").unwrap();
        assert_eq!(std::fs::read_to_string(ws.join("kb/a.md")).unwrap(), "a");
        assert_eq!(
            std::fs::read_to_string(ws.join("kb/gone.md")).unwrap(),
            "bye"
        );
        assert!(!ws.join("kb/new.md").exists());
        assert_eq!(std::fs::read_to_string(ws.join(".git/HEAD")).unwrap(), "y");
    }

    #[test]
    fn a_shell_call_that_changes_nothing_records_only_that_it_ran() {
        let (_root, log) = setup();
        let scan = log.scan().unwrap();
        log.record_scan(&key("run3"), &scan, &scan).unwrap();
        let turn = log.turn("run3").unwrap();
        assert!(turn.shell && turn.files.is_empty());
    }

    #[test]
    fn delegated_turns_fold_into_the_turn_that_delegated_them() {
        let (root, log) = setup();
        let file = root.path().join("ws/kb/w.md");
        let rec = CallRecorder::new(Arc::clone(&log), key("worker"));
        rec.before(&file);
        std::fs::write(&file, "by the worker").unwrap();
        rec.after(&file);
        log.link(&key("conv"), "worker").unwrap();
        log.link(&key("conv"), "nothing-changed").unwrap();

        let turns = log.turns(None, 10);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].id, "conv");
        assert_eq!(log.all_files(&turns[0]).len(), 1);
        log.undo("conv").unwrap();
        assert!(!file.exists());
        assert!(log.turn("worker").unwrap().undone.is_some());
    }

    #[test]
    fn large_files_are_named_but_not_kept() {
        let (root, log) = setup();
        let big = root.path().join("ws/big.bin");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_SAVED_BYTES + 1).unwrap();
        let rec = CallRecorder::new(Arc::clone(&log), key("run4"));
        rec.before(&big);
        std::fs::write(&big, "small now").unwrap();
        rec.after(&big);
        let undone = log.undo("run4").unwrap();
        assert!(
            undone.skipped[0].why.contains("not kept"),
            "{:?}",
            undone.skipped
        );
    }

    #[test]
    fn old_turns_and_their_bytes_are_pruned() {
        let (root, log) = setup();
        let file = root.path().join("ws/kb/x.md");
        let rec = CallRecorder::new(Arc::clone(&log), key("old"));
        rec.before(&file);
        std::fs::write(&file, "content").unwrap();
        rec.after(&file);
        // Blobs are pruned once older than the cutoff too.
        log.prune(Utc::now() + chrono::Duration::days(1));
        assert!(log.turn("old").is_none());
        assert_eq!(
            std::fs::read_dir(log.dir().join("blobs")).unwrap().count(),
            0
        );
    }

    #[test]
    fn run_ids_cannot_name_paths_outside_the_log() {
        let (_root, log) = setup();
        assert!(log.turn("../../etc/passwd").is_none());
        assert!(log.undo("../x").is_err());
    }
}
