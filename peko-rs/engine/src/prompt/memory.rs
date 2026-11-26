//! The principal's hot knowledge-base set and directory-scoped project
//! instructions (`AGENTS.md`).
//!
//! - **Hot kb set** (ADR-055 D2/D8): `kb/MEMORY.md`, `kb/CONVENTIONS.md`,
//!   `kb/index.md`, plus the two targeted scope notes
//!   (`kb/groups/<channel>.md`, `kb/roles/<name>.md`). [`HotKb::load`]
//!   reads them under one shared byte budget and the renderer injects
//!   them as `<runtime-context>` tail sections, on change.
//! - **Memory list** (ADR-129): one line per memory file under
//!   `kb/memory/`, built from each file's `description`. Part of the hot
//!   set and of its budget.
//! - **Project instructions** (ADR-052 D5): the nearest `AGENTS.md`
//!   above the run's focus directory, via
//!   [`discover_project_instructions`].
//!
//! Both are conventions rather than required files: a missing or blank
//! file renders absent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// Directory (relative to the principal workspace) holding the
/// principal's persistent knowledge base (ADR-055). The hot set
/// (`MEMORY.md`, `index.md`, `CONVENTIONS.md`) lives here, as do the
/// cold conventions (`people/`, `groups/`, `roles/`) that feed the
/// targeted scope notes.
pub const KB_DIR: &str = "kb";

/// Filename peko uses for per-principal long-term memory. Resolved
/// under [`KB_DIR`] — `<principal_workspace>/kb/MEMORY.md`.
pub const PRINCIPAL_MEMORY_FILE: &str = "MEMORY.md";

/// Filename peko uses for the shared behavioral rulebook rendered to
/// every agent in the principal — `<principal_workspace>/kb/CONVENTIONS.md`.
pub const PRINCIPAL_CONVENTIONS_FILE: &str = "CONVENTIONS.md";

/// Filename peko uses for the kb map (ADR-055 D2: the second hot
/// file). Resolved under [`KB_DIR`] —
/// `<principal_workspace>/kb/index.md`.
pub const KB_INDEX_FILE: &str = "index.md";

/// Subdirectory (under [`KB_DIR`]) of per-group notes keyed by
/// channel/group id (ADR-055 D8 binding-note injection).
pub const KB_GROUPS_DIR: &str = "groups";

/// Subdirectory (under [`KB_DIR`]) of per-role notes keyed by role
/// name (ADR-055 D8 role-note injection). Pairs with the role file
/// `roles/<name>/ROLE.md` (ADR-064).
pub const KB_ROLES_DIR: &str = "roles";

/// Filename peko uses for directory-scoped project instructions.
pub const SHARED_CONTEXT_FILE: &str = "AGENTS.md";

/// Shared byte budget for the whole hot kb set (ADR-055 D2): every
/// agent pays for these bytes on the turn they (re)enter the tail, so
/// the set is capped as one unit, codex-style. Truncation notices count
/// against the budget.
pub const HOT_KB_BUDGET_BYTES: usize = 32 * 1024;

/// Per-file cap for each non-memory member of the hot set
/// (`CONVENTIONS.md`, `index.md`, the binding note, the role note).
/// `MEMORY.md` gets whatever these leave of [`HOT_KB_BUDGET_BYTES`], so
/// a runaway map or note can never starve memory below
/// [`HOT_KB_MEMORY_FLOOR_BYTES`].
pub const HOT_KB_SECTION_MAX_BYTES: usize = 4 * 1024;

/// Subdirectory (under [`KB_DIR`]) of memory files (ADR-129): one fact
/// per file, each with a `description` in its frontmatter.
pub const KB_MEMORY_DIR: &str = "memory";

/// Cap on the memory list's lines (ADR-129 D2), about 80 memories. Past
/// it the most recently changed memories that fit are listed and the
/// rest are counted in a notice.
pub const MEMORY_LIST_MAX_BYTES: usize = 8 * 1024;

/// Cap on one memory's `description` as listed (ADR-129 D1).
pub const MEMORY_DESCRIPTION_MAX_BYTES: usize = 200;

/// How deep under `kb/memory/` the list looks, and how many files it
/// stats in one scan. Bounds the walk a render pays for; a tree past
/// either is listed as far as the walk got.
const MEMORY_SCAN_MAX_DEPTH: usize = 4;
const MEMORY_SCAN_MAX_FILES: usize = 2_000;

/// How much of a memory file is read to find its frontmatter.
const MEMORY_FRONTMATTER_READ_BYTES: u64 = 4 * 1024;

/// The least of the budget `MEMORY.md` can be left with: the budget
/// minus four fully-used [`HOT_KB_SECTION_MAX_BYTES`] sections and a
/// full memory list (ADR-129 D3).
pub const HOT_KB_MEMORY_FLOOR_BYTES: usize =
    HOT_KB_BUDGET_BYTES - 4 * HOT_KB_SECTION_MAX_BYTES - MEMORY_LIST_MAX_BYTES;

/// Maximum bytes of `AGENTS.md` content injected as the
/// project-instructions tail section. Matches the 32 KiB cap codex
/// applies to its whole instruction hierarchy.
pub const PROJECT_INSTRUCTIONS_MAX_BYTES: usize = 32 * 1024;

/// First line of a `kb_scope` prompt section that restricts the hot set
/// ([`HotKb::restricted_to`]).
pub const KB_SCOPE_RESTRICTED: &str = "restricted";

/// The principal's hot kb set for one run, loaded under
/// [`HOT_KB_BUDGET_BYTES`]. Each member is `None` when its file is
/// missing, blank, or unreadable (absence renders absent).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HotKb {
    /// `kb/MEMORY.md` — curated long-term memory.
    pub memory: Option<String>,
    /// `kb/CONVENTIONS.md` — the shared behavioral rulebook.
    pub conventions: Option<String>,
    /// `kb/index.md` — the map of the kb tree.
    pub index: Option<String>,
    /// `kb/groups/<channel>.md` — the run's binding note.
    pub binding_note: Option<String>,
    /// `kb/roles/<role_name>.md` — the named role's standing note.
    pub role_note: Option<String>,
    /// Every memory file under `kb/memory/` that has a description,
    /// sorted by name (ADR-129). [`HotKb::memory_list`] caps it.
    pub memories: Vec<MemoryEntry>,
}

/// One memory file as the list shows it (ADR-129 D1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    /// The file's path under `kb/memory/`, without `.md`, `/`-separated.
    pub name: String,
    /// Its frontmatter `description`, on one line.
    pub description: String,
    /// When the file last changed; decides what is listed past the cap.
    pub modified: SystemTime,
}

impl MemoryEntry {
    /// The list line: `- <name>: <description>`.
    #[must_use]
    pub fn line(&self) -> String {
        format!("- {}: {}", self.name, self.description)
    }
}

/// The memories a prompt lists, after the cap (ADR-129 D2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryList {
    /// One line per listed memory, sorted by name.
    pub lines: Vec<String>,
    /// Memories left out because the lines passed
    /// [`MEMORY_LIST_MAX_BYTES`].
    pub hidden: usize,
}

impl HotKb {
    /// Load the hot set for a run in `workspace`, bound to `channel`
    /// and named `role_name`.
    ///
    /// The four small members load first, each capped at
    /// [`HOT_KB_SECTION_MAX_BYTES`]; `MEMORY.md` then gets the rest of
    /// [`HOT_KB_BUDGET_BYTES`]. An oversized file is cut at a line
    /// boundary and ends with a notice telling the agent to curate it.
    /// Channel and role names that are not a single safe path segment
    /// load no note.
    #[must_use]
    pub fn load(workspace: &Path, channel: &str, role_name: &str) -> Self {
        let kb = workspace.join(KB_DIR);
        let note = |dir: &str, name: &str| {
            is_safe_segment(name)
                .then(|| {
                    read_capped(
                        &kb.join(dir).join(format!("{name}.md")),
                        HOT_KB_SECTION_MAX_BYTES,
                    )
                })
                .flatten()
        };
        let conventions = read_capped(
            &kb.join(PRINCIPAL_CONVENTIONS_FILE),
            HOT_KB_SECTION_MAX_BYTES,
        );
        let index = read_capped(&kb.join(KB_INDEX_FILE), HOT_KB_SECTION_MAX_BYTES);
        let binding_note = note(KB_GROUPS_DIR, channel);
        let role_note = note(KB_ROLES_DIR, role_name);

        let memories = scan_memories(&kb.join(KB_MEMORY_DIR));

        let used: usize = [&conventions, &index, &binding_note, &role_note]
            .into_iter()
            .flatten()
            .map(String::len)
            .sum::<usize>()
            + cap_memories(&memories).bytes();
        let memory = read_capped(
            &kb.join(PRINCIPAL_MEMORY_FILE),
            HOT_KB_BUDGET_BYTES.saturating_sub(used),
        );

        Self {
            memory,
            conventions,
            index,
            binding_note,
            role_note,
            memories,
        }
    }

    /// The memories this run's prompt lists: [`Self::memories`] under
    /// [`MEMORY_LIST_MAX_BYTES`]. Call it after [`Self::restricted_to`],
    /// so the cap and the hidden count cover only what the run may read.
    #[must_use]
    pub fn memory_list(&self) -> MemoryList {
        cap_memories(&self.memories)
    }

    /// The hot set a run may see under `scope`, the `kb_scope` prompt
    /// section (ADR-113 D6). Empty scope: all of it. Otherwise the first
    /// line is [`KB_SCOPE_RESTRICTED`] and each further line is a
    /// workspace-relative path the run may read; `MEMORY.md`,
    /// `CONVENTIONS.md` and `index.md` stay only when such a path names
    /// or contains them, and a memory is listed only when such a path
    /// names its file or a directory above it. The binding and role notes
    /// are the run's own conversation's and role's, and always stay.
    #[must_use]
    pub fn restricted_to(mut self, scope: &str) -> Self {
        let mut lines = scope.lines().map(str::trim).filter(|l| !l.is_empty());
        if lines.next().is_none() {
            return self;
        }
        let paths: Vec<&str> = lines.collect();
        let may_read = |file: &str| {
            let relative = format!("{KB_DIR}/{file}");
            paths
                .iter()
                .any(|path| relative == *path || relative.starts_with(&format!("{path}/")))
        };
        if !may_read(PRINCIPAL_MEMORY_FILE) {
            self.memory = None;
        }
        if !may_read(PRINCIPAL_CONVENTIONS_FILE) {
            self.conventions = None;
        }
        if !may_read(KB_INDEX_FILE) {
            self.index = None;
        }
        // ADR-129 D2: a line is shown only for a file the run may read.
        self.memories
            .retain(|entry| may_read(&format!("{KB_MEMORY_DIR}/{}.md", entry.name)));
        self
    }

    /// Total bytes of the loaded hot set (never above
    /// [`HOT_KB_BUDGET_BYTES`]).
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        [
            &self.memory,
            &self.conventions,
            &self.index,
            &self.binding_note,
            &self.role_note,
        ]
        .into_iter()
        .flatten()
        .map(String::len)
        .sum::<usize>()
            + self.memory_list().bytes()
    }
}

impl MemoryList {
    /// Bytes the lines take in a prompt, one newline after each.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.lines.iter().map(|line| line.len() + 1).sum()
    }

    /// Whether there is nothing to show.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.hidden == 0
    }
}

/// `entries` under [`MEMORY_LIST_MAX_BYTES`]: all of them when they fit,
/// else the most recently changed ones that do, still sorted by name.
fn cap_memories(entries: &[MemoryEntry]) -> MemoryList {
    let size = |entry: &MemoryEntry| entry.line().len() + 1;
    if entries.iter().map(size).sum::<usize>() <= MEMORY_LIST_MAX_BYTES {
        return MemoryList {
            lines: entries.iter().map(MemoryEntry::line).collect(),
            hidden: 0,
        };
    }
    let mut newest: Vec<&MemoryEntry> = entries.iter().collect();
    newest.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.name.cmp(&b.name)));
    let mut room = MEMORY_LIST_MAX_BYTES;
    let mut kept: Vec<&MemoryEntry> = Vec::new();
    for entry in newest {
        let Some(left) = room.checked_sub(size(entry)) else {
            break;
        };
        room = left;
        kept.push(entry);
    }
    kept.sort_by(|a, b| a.name.cmp(&b.name));
    MemoryList {
        hidden: entries.len() - kept.len(),
        lines: kept.into_iter().map(MemoryEntry::line).collect(),
    }
}

/// What one scan remembers of a file: its stat, and the description it
/// had then (`None` when it had none).
type ScannedFile = (SystemTime, u64, Option<String>);

/// The last scan of each `kb/memory/` directory, keyed by the
/// directory. A file is read again only when its `(mtime, len)` changed
/// (ADR-129 D2), so a render pays one stat per memory.
fn scan_cache() -> &'static Mutex<HashMap<PathBuf, HashMap<PathBuf, ScannedFile>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, HashMap<PathBuf, ScannedFile>>>> =
        OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// Every memory file under `dir` that has a description, sorted by
/// name. Walks the tree each call; symbolic links are not followed, so
/// nothing outside the tree is listed. A missing directory lists
/// nothing.
fn scan_memories(dir: &Path) -> Vec<MemoryEntry> {
    let mut files: Vec<(PathBuf, String, SystemTime, u64)> = Vec::new();
    walk_memory_dir(dir, "", 0, &mut files);
    if files.is_empty() {
        scan_cache()
            .lock()
            .expect("memory scan cache poisoned")
            .remove(dir);
        return Vec::new();
    }
    let previous = scan_cache()
        .lock()
        .expect("memory scan cache poisoned")
        .get(dir)
        .cloned()
        .unwrap_or_default();
    let mut scanned = HashMap::with_capacity(files.len());
    let mut entries = Vec::new();
    for (path, name, modified, len) in files {
        let description = match previous.get(&path) {
            Some((was_modified, was_len, description))
                if *was_modified == modified && *was_len == len =>
            {
                description.clone()
            }
            _ => read_description(&path),
        };
        if let Some(description) = &description {
            entries.push(MemoryEntry {
                name,
                description: description.clone(),
                modified,
            });
        }
        scanned.insert(path, (modified, len, description));
    }
    scan_cache()
        .lock()
        .expect("memory scan cache poisoned")
        .insert(dir.to_path_buf(), scanned);
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Collect the `.md` files under `dir` as `(path, name, mtime, len)`,
/// `name` being the path under the memory root without `.md`.
fn walk_memory_dir(
    dir: &Path,
    prefix: &str,
    depth: usize,
    files: &mut Vec<(PathBuf, String, SystemTime, u64)>,
) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        if files.len() >= MEMORY_SCAN_MAX_FILES {
            return;
        }
        let file_name = entry.file_name();
        let Some(segment) = file_name.to_str() else {
            continue;
        };
        if segment.starts_with('.') || !is_safe_segment(segment) {
            continue;
        }
        // `DirEntry::file_type` does not follow a symbolic link.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth + 1 < MEMORY_SCAN_MAX_DEPTH {
                walk_memory_dir(
                    &entry.path(),
                    &format!("{prefix}{segment}/"),
                    depth + 1,
                    files,
                );
            }
        } else if kind.is_file() {
            let Some(stem) = segment.strip_suffix(".md") else {
                continue;
            };
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            files.push((
                entry.path(),
                format!("{prefix}{stem}"),
                modified,
                meta.len(),
            ));
        }
    }
}

/// The `description` in `path`'s frontmatter, on one line and under
/// [`MEMORY_DESCRIPTION_MAX_BYTES`]. `None` when the file has no
/// frontmatter, no description, or a blank one.
fn read_description(path: &Path) -> Option<String> {
    use std::io::Read as _;
    let mut head = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MEMORY_FRONTMATTER_READ_BYTES)
        .read_to_end(&mut head)
        .ok()?;
    description_of(&String::from_utf8_lossy(&head))
}

/// The frontmatter `description` of a memory file's text. See
/// [`read_description`].
#[must_use]
pub fn description_of(text: &str) -> Option<String> {
    let value = frontmatter_field(text, "description")?;
    let mut line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return None;
    }
    if line.len() > MEMORY_DESCRIPTION_MAX_BYTES {
        let mut end = MEMORY_DESCRIPTION_MAX_BYTES - '…'.len_utf8();
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push('…');
    }
    Some(line)
}

/// The value of `key` in the `---` frontmatter block that opens `text`,
/// with surrounding quotes removed. `None` without a frontmatter block
/// or the key. Only single-line values are read.
#[must_use]
pub fn frontmatter_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    for line in lines {
        if line.trim_end() == "---" {
            return None;
        }
        let Some(value) = line
            .strip_prefix(key)
            .and_then(|rest| rest.strip_prefix(':'))
        else {
            continue;
        };
        let value = value.trim();
        let unquoted = ['"', '\'']
            .into_iter()
            .find_map(|quote| value.strip_prefix(quote)?.strip_suffix(quote))
            .unwrap_or(value);
        return Some(unquoted);
    }
    None
}

/// The text of a memory file after its frontmatter block; the whole
/// text when it has none.
#[must_use]
pub fn frontmatter_body(text: &str) -> &str {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return text;
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        if line.trim_end() == "---" {
            return &rest[offset..];
        }
    }
    text
}

/// Validate a single path segment for the kb scope-note loaders:
/// non-empty, not a dot-segment, no separators. Prevents a channel id
/// or role name from escaping the kb tree.
fn is_safe_segment(segment: &str) -> bool {
    !segment.is_empty() && segment != "." && segment != ".." && !segment.contains(['/', '\\'])
}

/// Read `path` and cap it at `max_bytes` (notice included). `None` for
/// a missing, unreadable, or whitespace-only file.
fn read_capped(path: &Path, max_bytes: usize) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(truncate_with_notice(raw, path, max_bytes))
}

/// Walk up from `start` looking for the nearest `AGENTS.md`. Stops
/// after checking a directory that contains a `.git` entry (the repo
/// root is checked, but nothing above it), or at the filesystem root.
///
/// There is NO principal-workspace constraint: agents typically work
/// on repos outside the principal workspace, so provenance is
/// communicated via the rendered section's label (the file's own path)
/// and an explicit environment-provided authority note instead of a
/// path cap. Returns the file's path and its contents (non-empty
/// only), truncated at [`PROJECT_INSTRUCTIONS_MAX_BYTES`] with a
/// notice.
#[must_use]
pub fn discover_project_instructions(start: &Path) -> Option<(PathBuf, String)> {
    // Walk the path AS GIVEN — no canonicalize — so the returned label
    // preserves the form the agent actually typed (`/tmp/...` stays
    // `/tmp/...`, not the `/private/tmp/...` macOS resolves it to —
    // found by the tiered-prompt-explore e2e experiment). Symlinks and
    // `..` still work: `is_file` / `read_to_string` follow them, and
    // `parent()` walks upward regardless.
    let mut current: PathBuf = start.to_path_buf();
    if current.is_file() {
        current = current.parent()?.to_path_buf();
    }
    loop {
        let candidate = current.join(SHARED_CONTEXT_FILE);
        if candidate.is_file() {
            if let Some(content) = read_capped(&candidate, PROJECT_INSTRUCTIONS_MAX_BYTES) {
                return Some((candidate, content));
            }
        }

        // Repo boundary: check the directory that owns `.git`, then
        // stop — an AGENTS.md above the repo root does not describe
        // this project.
        if current.join(".git").exists() {
            return None;
        }

        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return None,
        }
    }
}

/// Extract a directory from a tool-call parameter dict, if a
/// recognisable path-style parameter is present.
///
/// Returns the directory portion of common path-bearing parameters
/// (file_path, path, directory, cwd). Relative paths are resolved
/// against `default_root` (typically the principal's workspace).
#[must_use]
pub fn directory_from_tool_params(
    tool_name: &str,
    params: &serde_json::Value,
    default_root: &Path,
) -> Option<PathBuf> {
    let key = match tool_name {
        "Read" | "Write" | "Edit" => "file_path",
        "Glob" => "directory",
        "Grep" => "path",
        "Bash" => "cwd",
        _ => return None,
    };
    let value = params.get(key)?.as_str()?;
    let raw = PathBuf::from(value);
    let resolved = if raw.is_absolute() {
        raw
    } else {
        default_root.join(raw)
    };
    // If it's a file path (has a filename), strip to its parent.
    let dir = if resolved.is_file() || resolved.extension().is_some() {
        resolved.parent()?.to_path_buf()
    } else {
        resolved
    };
    Some(dir)
}

/// Cap `raw` at `max_bytes` UTF-8 bytes, notice included. The kept
/// text ends at the last line break that fits (or at a char boundary
/// when a single line is longer than the room left).
fn truncate_with_notice(raw: String, path: &Path, max_bytes: usize) -> String {
    let len = raw.len();
    if len <= max_bytes {
        return raw;
    }
    let notice = format!(
        "\n\n<!-- truncated: {} is {len} bytes and only the part above fits \
         its prompt budget; curate the file — the rest is not in your prompt -->\n",
        path.display()
    );
    let room = max_bytes.saturating_sub(notice.len());
    let mut end = room.min(len);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(newline) = raw[..end].rfind('\n') {
        end = newline;
    }
    format!("{}{notice}", &raw[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb(files: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for (relative, body) in files {
            let path = tmp.path().join(KB_DIR).join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        tmp
    }

    /// ADR-113 D6: a restricted run sees only the hot files it may read.
    #[test]
    fn a_scope_drops_the_hot_files_it_does_not_name() {
        let full = || HotKb {
            memory: Some("m".into()),
            conventions: Some("c".into()),
            index: Some("i".into()),
            binding_note: Some("b".into()),
            role_note: Some("r".into()),
            memories: Vec::new(),
        };
        assert_eq!(full().restricted_to(""), full());
        assert_eq!(full().restricted_to("  \n"), full());
        let seeded = full().restricted_to("restricted\npublic\nkb/CONVENTIONS.md\n");
        assert_eq!(seeded.memory, None);
        assert_eq!(seeded.index, None);
        assert_eq!(seeded.conventions.as_deref(), Some("c"));
        assert_eq!(seeded.binding_note.as_deref(), Some("b"));
        assert_eq!(seeded.role_note.as_deref(), Some("r"));
        let nothing = full().restricted_to("restricted");
        assert_eq!(
            (nothing.memory, nothing.conventions, nothing.index),
            (None, None, None)
        );
        let whole_kb = full().restricted_to("restricted\nkb");
        assert_eq!(whole_kb.memory.as_deref(), Some("m"));
        // A prefix of a name is no match.
        assert_eq!(full().restricted_to("restricted\nkb/MEM").memory, None);
    }

    #[test]
    fn hot_kb_is_empty_without_files() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            HotKb::load(tmp.path(), "chan_a", "worker"),
            HotKb::default()
        );
    }

    #[test]
    fn hot_kb_loads_every_member() {
        let tmp = kb(&[
            ("MEMORY.md", "I prefer tabs."),
            ("CONVENTIONS.md", "Journal daily."),
            ("index.md", "- people/ — who I know"),
            ("groups/chan_abc.md", "The room's language is German."),
            ("roles/channel-comm.md", "You greet new members."),
        ]);
        let hot = HotKb::load(tmp.path(), "chan_abc", "channel-comm");
        assert_eq!(hot.memory.as_deref(), Some("I prefer tabs."));
        assert_eq!(hot.conventions.as_deref(), Some("Journal daily."));
        assert!(hot.index.unwrap().contains("who I know"));
        assert!(hot.binding_note.unwrap().contains("German"));
        assert!(hot.role_note.unwrap().contains("greet new members"));
    }

    #[test]
    fn hot_kb_skips_blank_files_and_unmatched_notes() {
        let tmp = kb(&[("MEMORY.md", "  \n"), ("groups/chan_other.md", "x")]);
        let hot = HotKb::load(tmp.path(), "chan_absent", "unnamed");
        assert_eq!(hot, HotKb::default());
    }

    #[test]
    fn hot_kb_rejects_unsafe_note_segments() {
        let tmp = kb(&[("MEMORY.md", "mem"), ("groups/x.md", "note")]);
        for bad in ["", ".", "..", "a/b", "a\\b", "../MEMORY"] {
            let hot = HotKb::load(tmp.path(), bad, bad);
            assert!(hot.binding_note.is_none(), "segment: {bad}");
            assert!(hot.role_note.is_none(), "segment: {bad}");
        }
    }

    #[test]
    fn hot_kb_caps_small_members_per_section() {
        let big = "y\n".repeat(HOT_KB_SECTION_MAX_BYTES);
        let tmp = kb(&[
            ("CONVENTIONS.md", &big),
            ("index.md", &big),
            ("groups/chan_a.md", &big),
            ("roles/worker.md", &big),
        ]);
        let hot = HotKb::load(tmp.path(), "chan_a", "worker");
        for member in [
            &hot.conventions,
            &hot.index,
            &hot.binding_note,
            &hot.role_note,
        ] {
            let text = member.as_deref().unwrap();
            assert!(
                text.len() <= HOT_KB_SECTION_MAX_BYTES,
                "{} bytes",
                text.len()
            );
            assert!(text.contains("truncated:"));
        }
    }

    #[test]
    fn memory_gets_the_whole_budget_when_alone() {
        let big = "m\n".repeat(HOT_KB_BUDGET_BYTES);
        let tmp = kb(&[("MEMORY.md", &big)]);
        let memory = HotKb::load(tmp.path(), "cli", "root").memory.unwrap();
        assert!(memory.len() <= HOT_KB_BUDGET_BYTES);
        assert!(
            memory.len() > HOT_KB_BUDGET_BYTES - 512,
            "{} bytes",
            memory.len()
        );
        assert!(memory.contains("truncated:"));
    }

    #[test]
    fn memory_keeps_its_floor_when_every_other_member_is_full() {
        let big = "z\n".repeat(HOT_KB_BUDGET_BYTES);
        let tmp = kb(&[
            ("MEMORY.md", &big),
            ("CONVENTIONS.md", &big),
            ("index.md", &big),
            ("groups/chan_a.md", &big),
            ("roles/worker.md", &big),
        ]);
        let hot = HotKb::load(tmp.path(), "chan_a", "worker");
        assert!(
            hot.total_bytes() <= HOT_KB_BUDGET_BYTES,
            "{}",
            hot.total_bytes()
        );
        assert!(hot.memory.unwrap().len() >= HOT_KB_MEMORY_FLOOR_BYTES - 512);
    }

    fn memory_file(description: &str, body: &str) -> String {
        format!("---\ndescription: {description}\nsource: /local-user\n---\n{body}\n")
    }

    fn names(hot: &HotKb) -> Vec<&str> {
        hot.memories.iter().map(|e| e.name.as_str()).collect()
    }

    /// ADR-129 D1/D2: a memory with a description is listed by its path
    /// under `kb/memory/`, in any directory there; a file without one,
    /// a non-`.md` file and a dot-file are not.
    #[test]
    fn memories_are_listed_from_the_whole_tree_by_description() {
        let tmp = kb(&[
            ("memory/zeta.md", &memory_file("Last by name", "z")),
            (
                "memory/alpha.md",
                &memory_file("\"Quoted   and\tspaced\"", "a"),
            ),
            (
                "memory/projects/atlas.md",
                &memory_file("In a directory", "p"),
            ),
            ("memory/README.md", "# memory/\n\nNo frontmatter here.\n"),
            ("memory/blank.md", "---\ndescription:   \n---\nbody\n"),
            ("memory/notes.txt", &memory_file("Not markdown", "n")),
            ("memory/.hidden.md", &memory_file("Dot file", "h")),
            ("people/maya.md", &memory_file("Outside the tree", "m")),
        ]);
        let hot = HotKb::load(tmp.path(), "", "");
        assert_eq!(names(&hot), ["alpha", "projects/atlas", "zeta"]);
        assert_eq!(
            hot.memory_list().lines,
            [
                "- alpha: Quoted and spaced",
                "- projects/atlas: In a directory",
                "- zeta: Last by name",
            ]
        );
        assert_eq!(hot.memory_list().hidden, 0);
    }

    /// ADR-129 D2: a memory written, changed or deleted between two
    /// renders shows in the second, a new directory included.
    #[test]
    fn the_list_follows_the_files_between_renders() {
        let tmp = kb(&[("memory/one.md", &memory_file("First", "1"))]);
        assert_eq!(names(&HotKb::load(tmp.path(), "", "")), ["one"]);

        let dir = tmp.path().join("kb/memory/shop");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hours.md"), memory_file("Opening hours", "2")).unwrap();
        assert_eq!(
            names(&HotKb::load(tmp.path(), "", "")),
            ["one", "shop/hours"]
        );

        // Same length, later mtime: the cache must not answer.
        let one = tmp.path().join("kb/memory/one.md");
        std::fs::write(&one, memory_file("Third", "1")).unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&one)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let hot = HotKb::load(tmp.path(), "", "");
        assert_eq!(hot.memories[0].description, "Third");

        std::fs::remove_file(&one).unwrap();
        assert_eq!(names(&HotKb::load(tmp.path(), "", "")), ["shop/hours"]);
    }

    /// ADR-129 D2: nothing outside the tree is listed through a link.
    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_not_followed() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), memory_file("Secret", "s")).unwrap();
        let tmp = kb(&[("memory/real.md", &memory_file("Real", "r"))]);
        let memory = tmp.path().join("kb/memory");
        std::os::unix::fs::symlink(outside.path(), memory.join("linked")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.md"), memory.join("file.md"))
            .unwrap();
        assert_eq!(names(&HotKb::load(tmp.path(), "", "")), ["real"]);
    }

    /// ADR-129 D2: past the cap the most recently changed memories are
    /// listed, sorted by name, and the rest are counted.
    #[test]
    fn past_the_cap_the_newest_memories_are_listed() {
        let description = "d".repeat(150);
        let entries: Vec<MemoryEntry> = (0..100)
            .map(|i| MemoryEntry {
                name: format!("m{i:03}"),
                description: description.clone(),
                modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(i),
            })
            .collect();
        let list = cap_memories(&entries);
        assert!(list.bytes() <= MEMORY_LIST_MAX_BYTES, "{}", list.bytes());
        assert!(list.hidden > 0);
        assert_eq!(list.lines.len() + list.hidden, 100);
        // The newest is kept and the oldest dropped; lines stay sorted.
        assert!(list.lines.last().unwrap().starts_with("- m099:"));
        assert!(!list.lines[0].starts_with("- m000:"));
        let mut sorted = list.lines.clone();
        sorted.sort();
        assert_eq!(sorted, list.lines);
    }

    /// ADR-129 D2: a run is shown a line only for a file it may read,
    /// and the cap counts only those.
    #[test]
    fn a_scope_lists_only_the_memories_it_names() {
        let tmp = kb(&[
            ("memory/private.md", &memory_file("Private", "p")),
            ("memory/shop/hours.md", &memory_file("Hours", "h")),
            ("memory/shop/prices.md", &memory_file("Prices", "$")),
            ("memory/shopping.md", &memory_file("Not under shop/", "s")),
        ]);
        let load = || HotKb::load(tmp.path(), "", "");
        assert_eq!(load().restricted_to("").memories.len(), 4);
        assert!(load().restricted_to("restricted").memories.is_empty());
        assert!(load()
            .restricted_to("restricted\npublic")
            .memories
            .is_empty());
        assert_eq!(
            names(&load().restricted_to("restricted\nkb/memory/shop")),
            ["shop/hours", "shop/prices"]
        );
        assert_eq!(
            names(&load().restricted_to("restricted\nkb/memory/private.md")),
            ["private"]
        );
        assert_eq!(load().restricted_to("restricted\nkb").memories.len(), 4);
        let scoped = load().restricted_to("restricted\nkb/memory/shop");
        assert_eq!(scoped.memory_list().hidden, 0);
    }

    #[test]
    fn a_long_description_is_cut_on_one_line() {
        let text = format!("---\ndescription: {}\n---\n", "é".repeat(300));
        let description = description_of(&text).unwrap();
        assert!(description.len() <= MEMORY_DESCRIPTION_MAX_BYTES);
        assert!(description.ends_with('…'));
        assert_eq!(description_of("no frontmatter\ndescription: x\n"), None);
        assert_eq!(
            description_of("---\nsource: a\n---\ndescription: x\n"),
            None
        );
    }

    #[test]
    fn frontmatter_fields_and_body_are_read() {
        let text = "---\ndescription: 'One'\nsource: chan_1#4, /local-user\n---\nBody.\n";
        assert_eq!(
            frontmatter_field(text, "source"),
            Some("chan_1#4, /local-user")
        );
        assert_eq!(frontmatter_field(text, "description"), Some("One"));
        assert_eq!(frontmatter_field(text, "sources"), None);
        assert_eq!(frontmatter_body(text), "Body.\n");
        assert_eq!(frontmatter_body("plain\n"), "plain\n");
        assert_eq!(
            frontmatter_body("---\nnever closed\n"),
            "---\nnever closed\n"
        );
    }

    #[test]
    fn small_hot_set_is_untouched() {
        let tmp = kb(&[("MEMORY.md", "a\nb\n"), ("index.md", "map")]);
        let hot = HotKb::load(tmp.path(), "cli", "root");
        assert_eq!(hot.memory.as_deref(), Some("a\nb\n"));
        assert_eq!(hot.total_bytes(), 7);
    }

    #[test]
    fn truncation_counts_bytes_not_chars() {
        // 3-byte chars: a char-count cap would keep three times the
        // byte budget.
        let raw = "記".repeat(HOT_KB_SECTION_MAX_BYTES);
        let out = truncate_with_notice(raw, Path::new("kb/index.md"), HOT_KB_SECTION_MAX_BYTES);
        assert!(out.len() <= HOT_KB_SECTION_MAX_BYTES, "{} bytes", out.len());
        assert!(out.starts_with('記'));
    }

    #[test]
    fn truncation_cuts_at_a_line_boundary() {
        let raw = format!("{}\n{}", "a".repeat(100), "b".repeat(400));
        let out = truncate_with_notice(raw, Path::new("kb/MEMORY.md"), 400);
        assert!(out.starts_with(&"a".repeat(100)));
        assert!(!out.contains("bb"), "kept a partial line: {out}");
        assert!(out.contains("truncated: kb/MEMORY.md is 501 bytes"));
    }

    #[test]
    fn discover_project_instructions_finds_nearest_from_nested_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let nested = project.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(project.join("AGENTS.md"), "repo rules").unwrap();

        let (path, content) = discover_project_instructions(&nested).unwrap();
        assert_eq!(content, "repo rules");
        assert!(path.ends_with("AGENTS.md"), "path was: {path:?}");
        assert!(path.parent().unwrap().ends_with("project"));
    }

    #[test]
    fn discover_project_instructions_prefers_nearest_over_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("outer");
        let inner = outer.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(outer.join("AGENTS.md"), "outer rules").unwrap();
        std::fs::write(inner.join("AGENTS.md"), "inner rules").unwrap();

        let (path, content) = discover_project_instructions(&inner).unwrap();
        assert_eq!(content, "inner rules");
        assert!(path.parent().unwrap().ends_with("inner"));
    }

    #[test]
    fn discover_project_instructions_does_not_cross_git_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        // AGENTS.md ABOVE the repo root must not be found.
        std::fs::write(tmp.path().join("AGENTS.md"), "above the repo").unwrap();
        let repo = tmp.path().join("repo");
        let nested = repo.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();

        assert!(
            discover_project_instructions(&nested).is_none(),
            "discovery must stop at the .git boundary"
        );
    }

    #[test]
    fn discover_project_instructions_checks_git_root_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();
        std::fs::write(repo.join("AGENTS.md"), "repo rules").unwrap();

        let (_, content) = discover_project_instructions(&repo).unwrap();
        assert_eq!(content, "repo rules");
    }

    #[test]
    fn discover_project_instructions_ignores_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("AGENTS.md"), "  \n  ").unwrap();
        std::fs::create_dir(project.join(".git")).unwrap();

        assert!(
            discover_project_instructions(&project).is_none(),
            "whitespace-only AGENTS.md must be ignored"
        );
    }

    #[test]
    fn discover_project_instructions_truncates_at_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let big = "x".repeat(PROJECT_INSTRUCTIONS_MAX_BYTES + 4096);
        std::fs::write(project.join("AGENTS.md"), big).unwrap();

        let (_, content) = discover_project_instructions(&project).unwrap();
        assert!(
            content.len() < PROJECT_INSTRUCTIONS_MAX_BYTES + 4096,
            "content was not capped: {} bytes",
            content.len()
        );
        assert!(
            content.contains("truncated:"),
            "truncation notice missing: {}",
            &content[content.len() - 200..]
        );
    }

    #[test]
    fn directory_from_tool_params_resolves_relative_paths() {
        let root = PathBuf::from("/workspaces/agent/personal");
        let dir = directory_from_tool_params(
            "Read",
            &serde_json::json!({"file_path": "src/main.rs"}),
            &root,
        )
        .unwrap();
        assert_eq!(dir, root.join("src"));
    }

    #[test]
    fn directory_from_tool_params_keeps_absolute_paths() {
        let root = PathBuf::from("/workspaces/agent/personal");
        let dir =
            directory_from_tool_params("Bash", &serde_json::json!({"cwd": "/tmp/build"}), &root)
                .unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/build"));
    }

    #[test]
    fn directory_from_tool_params_returns_none_for_unknown_tool() {
        let root = PathBuf::from("/workspaces/agent/personal");
        let dir = directory_from_tool_params("Async", &serde_json::json!({"action":"list"}), &root);
        assert!(dir.is_none());
    }
}
