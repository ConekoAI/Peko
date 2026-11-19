//! `ACCESS.toml` (ADR-113): what the owner lets visitors reach.
//!
//! A visitor is anyone a run acts for who is not the owner. The file
//! lists the built-in tools, MCP servers, and workspace paths a visitor's
//! run gets, with one `[contact."<address>"]` section per actor (or group
//! channel) that differs. Nothing is reachable unless listed, and a
//! contact section replaces `[visitors]` whole.
//!
//! Everything here is a pure function of the file, the stamp, and the
//! path; the dispatcher applies it to every call of an untrusted run,
//! before the reviewer and with no model involved. The floor
//! ([`super::floor`]) still applies on top: the file cannot grant `Bash`
//! or `Plan`, the runtime's own state, or a protected write.
//!
//! A file that does not parse is refused whole: visitors get
//! [`Access::minimum`] until the owner fixes it, and
//! [`Access::error`] says why.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use peko_tools_core::{ActingFor, Compartment};
use serde::Deserialize;

use super::floor;

/// The access file, at the workspace root.
pub const ACCESS_FILE: &str = "ACCESS.toml";

/// The file a conversation's folder shows the peko in every turn.
pub const NOTES_FILE: &str = "notes.md";

/// Most of [`NOTES_FILE`] one turn carries.
pub const NOTES_MAX_BYTES: usize = 4 * 1024;

/// What a conversation's folder may hold when the file names no cap.
pub const DEFAULT_OWN_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// The placeholder an `own` template carries for the conversation.
const CONVERSATION: &str = "{conversation}";

const SEED: &str = include_str!("../resources/ACCESS.toml");

/// Tools a broken file leaves a visitor: the conversation itself and its
/// own folder.
const MINIMUM_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Glob",
    "Grep",
    "Task",
    "ChannelRead",
    "ChannelSend",
];

const DEFAULT_OWN: &str = "contacts/{conversation}/";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    visitors: RawRule,
    #[serde(default)]
    contact: BTreeMap<String, RawRule>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    mcp: Vec<String>,
    #[serde(default)]
    read: Vec<String>,
    #[serde(default)]
    write: Vec<String>,
    own: Option<String>,
    own_max_bytes: Option<u64>,
    #[serde(default)]
    ask_owner: bool,
}

/// What one visitor reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Built-in tool names.
    pub tools: Vec<String>,
    /// MCP server ids.
    pub mcp: Vec<String>,
    /// Workspace-relative paths the visitor reads.
    pub read: Vec<String>,
    /// Workspace-relative paths the visitor reads and writes.
    pub write: Vec<String>,
    /// The folder each conversation owns: everything before
    /// `{conversation}`, workspace-relative, without a trailing slash.
    pub own_base: Option<String>,
    /// The most one conversation's folder may hold, in bytes.
    pub own_max_bytes: u64,
    /// Whether what this visitor's run may not do is put to the owner:
    /// a call this rule does not list, a call the reviewer finds
    /// unclear, a change to a protected file. Off: each is refused.
    pub ask_owner: bool,
}

impl Rule {
    fn from_raw(section: &str, raw: RawRule) -> Result<Self, String> {
        for tool in &raw.tools {
            if floor::OWNER_ONLY_TOOLS.contains(&tool.as_str()) {
                return Err(format!(
                    "{section}: `{tool}` is the owner's alone and cannot be listed in `tools`"
                ));
            }
        }
        let read = paths(section, "read", raw.read)?;
        let write = paths(section, "write", raw.write)?;
        let own_base = raw.own.map(|own| own_base(section, &own)).transpose()?;
        Ok(Self {
            tools: raw.tools,
            mcp: raw.mcp,
            read,
            write,
            own_base,
            own_max_bytes: raw.own_max_bytes.unwrap_or(DEFAULT_OWN_MAX_BYTES),
            ask_owner: raw.ask_owner,
        })
    }

    /// Whether the visitor may call the built-in `tool`.
    #[must_use]
    pub fn allows_tool(&self, tool: &str) -> bool {
        !floor::OWNER_ONLY_TOOLS.contains(&tool) && self.tools.iter().any(|t| t == tool)
    }

    /// Whether the visitor may call methods of the MCP server `server`.
    #[must_use]
    pub fn allows_mcp(&self, server: &str) -> bool {
        self.mcp.iter().any(|s| s == server)
    }

    /// The folder `compartment` owns under this rule, workspace-relative.
    /// `None` outside a conversation, or when the rule gives none.
    #[must_use]
    pub fn own_dir(&self, compartment: &Compartment) -> Option<String> {
        let base = self.own_base.as_deref()?;
        let name = conversation_dir_name(compartment.channel()?)?;
        Some(format!("{base}/{name}"))
    }
}

/// A parsed `ACCESS.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    /// The rule for every visitor without a section of their own.
    pub visitors: Rule,
    /// `(actor address or channel id, rule)` per contact section.
    pub contacts: Vec<(String, Rule)>,
    /// Why the file on disk was refused, when it was.
    pub error: Option<String>,
}

impl Access {
    /// Parse `text`, or say why it is refused.
    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: RawFile = toml::from_str(text).map_err(|e| e.to_string())?;
        let visitors = Rule::from_raw("[visitors]", raw.visitors)?;
        let mut contacts = Vec::with_capacity(raw.contact.len());
        for (address, rule) in raw.contact {
            let section = format!("[contact.\"{address}\"]");
            contacts.push((address, Rule::from_raw(&section, rule)?));
        }
        let access = Self {
            visitors,
            contacts,
            error: None,
        };
        // A recursive search from a shared folder would read every
        // conversation's folder inside it.
        let rules = std::iter::once(("[visitors]".to_string(), &access.visitors)).chain(
            access
                .contacts
                .iter()
                .map(|(address, rule)| (format!("[contact.\"{address}\"]"), rule)),
        );
        for (section, rule) in rules {
            for path in rule.read.iter().chain(&rule.write) {
                if let Some(base) = access.own_bases().find(|base| under(base, path)) {
                    return Err(format!(
                        "{section}: `{path}` contains `{base}/`, which holds each \
                         conversation's own folder and cannot be shared"
                    ));
                }
            }
        }
        Ok(access)
    }

    /// The seeded file: what a peko without an `ACCESS.toml` applies.
    #[must_use]
    pub fn seeded() -> Self {
        Self::parse(SEED).expect("the seeded ACCESS.toml parses")
    }

    /// What visitors get while the file on disk is refused: the
    /// conversation and its own folder, nothing shared.
    #[must_use]
    pub fn minimum(error: String) -> Self {
        Self {
            visitors: Rule {
                tools: MINIMUM_TOOLS.iter().map(ToString::to_string).collect(),
                mcp: Vec::new(),
                read: Vec::new(),
                write: Vec::new(),
                own_base: Some(
                    own_base("[visitors]", DEFAULT_OWN).expect("the default own folder parses"),
                ),
                own_max_bytes: DEFAULT_OWN_MAX_BYTES,
                ask_owner: false,
            },
            contacts: Vec::new(),
            error: Some(error),
        }
    }

    /// The file in `workspace`: the seeded default when there is none,
    /// [`Self::minimum`] when it cannot be read or parsed.
    #[must_use]
    pub fn load(workspace: &Path) -> Self {
        match std::fs::read_to_string(workspace.join(ACCESS_FILE)) {
            Ok(text) => Self::parse(&text).unwrap_or_else(Self::minimum),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::seeded(),
            Err(e) => Self::minimum(e.to_string()),
        }
    }

    /// The rule for `actor`: its contact section under any alias
    /// (`same_actor` is the runtime's identity comparison, ADR-081 D1),
    /// else `[visitors]`. A group channel matches its section by exact id.
    #[must_use]
    pub fn rule_for(&self, actor: &str, same_actor: &dyn Fn(&str, &str) -> bool) -> &Rule {
        self.contacts
            .iter()
            .find(|(address, _)| address == actor || same_actor(address, actor))
            .map_or(&self.visitors, |(_, rule)| rule)
    }

    /// Every folder that holds conversations' own folders, under any rule.
    fn own_bases(&self) -> impl Iterator<Item = &str> {
        std::iter::once(&self.visitors)
            .chain(self.contacts.iter().map(|(_, rule)| rule))
            .filter_map(|rule| rule.own_base.as_deref())
    }

    /// What `stamp` reaches under this file.
    #[must_use]
    pub fn for_stamp(&self, stamp: &ActingFor, same_actor: &dyn Fn(&str, &str) -> bool) -> Reach {
        let rule = self.rule_for(&stamp.actor, same_actor).clone();
        let own_dir = rule.own_dir(&stamp.compartment);
        let mut own_bases: Vec<String> = self.own_bases().map(str::to_string).collect();
        own_bases.sort();
        own_bases.dedup();
        Reach {
            rule,
            own_dir,
            own_bases,
            error: self.error.clone(),
        }
    }
}

/// What one visitor run reaches: its rule, resolved for its conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reach {
    pub rule: Rule,
    /// This conversation's own folder, workspace-relative.
    pub own_dir: Option<String>,
    /// Every folder holding conversations' own folders. Nothing under
    /// one is reachable except [`Self::own_dir`].
    pub own_bases: Vec<String>,
    /// Why the file on disk was refused, when it was.
    pub error: Option<String>,
}

/// Why a visitor's run does not reach a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unreached {
    /// The owner's file does not list it. The owner may be asked, when
    /// the rule says so; `relative` is the path to list.
    Unlisted { reason: String, relative: String },
    /// No visitor reaches it, whatever the file or the owner says.
    Never(String),
}

impl Unreached {
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Unlisted { reason, .. } | Self::Never(reason) => reason,
        }
    }
}

/// One thing the owner may add to a visitor's rule (ADR-113 D10): what
/// an approval request for an unlisted call asks for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Grant {
    /// A built-in tool.
    Tool(String),
    /// An MCP server, by id.
    Mcp(String),
    /// A workspace-relative path, to read.
    Read(String),
    /// A workspace-relative path, to read and write.
    Write(String),
}

impl Grant {
    /// The `ACCESS.toml` key the grant extends.
    #[must_use]
    pub fn key(&self) -> &'static str {
        match self {
            Self::Tool(_) => "tools",
            Self::Mcp(_) => "mcp",
            Self::Read(_) => "read",
            Self::Write(_) => "write",
        }
    }

    #[must_use]
    pub fn value(&self) -> &str {
        match self {
            Self::Tool(v) | Self::Mcp(v) | Self::Read(v) | Self::Write(v) => v,
        }
    }

    /// The grant, as the owner reads it.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Tool(tool) => format!("the `{tool}` tool"),
            Self::Mcp(server) => format!("the `{server}` MCP server"),
            Self::Read(path) => format!("reading `{path}`"),
            Self::Write(path) => format!("writing `{path}`"),
        }
    }
}

impl Reach {
    /// What a run with no workspace reaches: nothing shared.
    #[must_use]
    pub fn none() -> Self {
        Access::minimum("no workspace is bound".into())
            .for_stamp(&ActingFor::unattributed(), &|a: &str, b: &str| a == b)
    }

    /// Why this run may not touch `target`, if it may not. Both paths
    /// must already be [`floor::resolve`]d.
    #[must_use]
    pub fn path_refusal(&self, workspace: &Path, target: &Path, write: bool) -> Option<Unreached> {
        let Ok(relative) = target.strip_prefix(workspace) else {
            return Some(Unreached::Never(format!(
                "{} is outside this peko's workspace. {}",
                target.display(),
                self.reachable()
            )));
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if self
            .own_dir
            .as_deref()
            .is_some_and(|own| under(&relative, own))
        {
            return None;
        }
        // The folder holding conversations' folders, anything in it, and
        // anything above it (a search from there would read them all).
        if self
            .own_bases
            .iter()
            .any(|base| under(&relative, base) || relative.is_empty() || under(base, &relative))
        {
            let what = if self.own_bases.iter().any(|base| under(&relative, base)) {
                format!("{relative} belongs to another conversation")
            } else {
                "that folder holds other conversations' folders".to_string()
            };
            return Some(Unreached::Never(format!("{what}. {}", self.reachable())));
        }
        let listed = |paths: &[String]| paths.iter().any(|path| under(&relative, path));
        if listed(&self.rule.write) || (!write && listed(&self.rule.read)) {
            return None;
        }
        let reason = if write && listed(&self.rule.read) {
            format!(
                "{relative} is read-only in this conversation. {}",
                self.reachable()
            )
        } else {
            format!(
                "{relative} is not shared with this conversation. {}",
                self.reachable()
            )
        };
        Some(Unreached::Unlisted { reason, relative })
    }

    /// This reach with `grant` added: what an owner-approved call runs
    /// under.
    #[must_use]
    pub fn with_grant(&self, grant: &Grant) -> Self {
        let mut reach = self.clone();
        let list = match grant {
            Grant::Tool(_) => &mut reach.rule.tools,
            Grant::Mcp(_) => &mut reach.rule.mcp,
            Grant::Read(_) => &mut reach.rule.read,
            Grant::Write(_) => &mut reach.rule.write,
        };
        list.push(grant.value().to_string());
        reach
    }

    /// One sentence naming what the run can reach, for refusals.
    #[must_use]
    pub fn reachable(&self) -> String {
        let mut parts = Vec::new();
        if let Some(own) = &self.own_dir {
            parts.push(format!(
                "this conversation's own folder `{own}/` (read and write)"
            ));
        }
        if !self.rule.write.is_empty() {
            parts.push(format!("read and write: {}", list(&self.rule.write)));
        }
        if !self.rule.read.is_empty() {
            parts.push(format!("read only: {}", list(&self.rule.read)));
        }
        if parts.is_empty() {
            "No workspace files are shared with this conversation.".into()
        } else {
            format!("Reachable here: {}.", parts.join("; "))
        }
    }
}

fn list(paths: &[String]) -> String {
    paths
        .iter()
        .map(|p| format!("`{p}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether workspace-relative `path` is `base` or inside it.
fn under(path: &str, base: &str) -> bool {
    path == base || path.starts_with(&format!("{base}/"))
}

/// `raw` as a clean workspace-relative path: no root, no `..`, no `~`.
fn clean(section: &str, key: &str, raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let bad = |why: &str| Err(format!("{section}: `{key}` entry `{raw}` {why}"));
    if trimmed.is_empty() || trimmed == "." {
        return bad("names the whole workspace; list the folders to share");
    }
    if trimmed.starts_with('~') {
        return bad("must be a path inside the workspace");
    }
    let mut parts = Vec::new();
    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return bad("must be a path inside the workspace, without `..`"),
        }
    }
    if parts.is_empty() {
        return bad("names the whole workspace; list the folders to share");
    }
    Ok(parts.join("/"))
}

fn paths(section: &str, key: &str, raw: Vec<String>) -> Result<Vec<String>, String> {
    raw.iter().map(|path| clean(section, key, path)).collect()
}

/// The folder an `own` template keeps conversations in: the template
/// must be `<folder>/{conversation}/`, outside the protected set.
fn own_base(section: &str, template: &str) -> Result<String, String> {
    let trimmed = template.trim().trim_end_matches('/');
    let Some(base) = trimmed
        .strip_suffix(CONVERSATION)
        .and_then(|base| base.strip_suffix('/'))
    else {
        return Err(format!(
            "{section}: `own` must be `<folder>/{CONVERSATION}/`, got `{template}`"
        ));
    };
    let base = clean(section, "own", base)?;
    let protected = floor::PROTECTED_FILES.contains(&base.as_str())
        || floor::PROTECTED_DIRS
            .iter()
            .chain(std::iter::once(&floor::INBOX_DIR))
            .any(|dir| under(&base, dir) || under(dir, &base));
    if protected {
        return Err(format!(
            "{section}: `own` folder `{base}` overlaps files only the owner writes"
        ));
    }
    Ok(base)
}

/// The folder name for conversation `channel`: its id, kept to
/// characters safe in a path. `None` when nothing usable is left.
#[must_use]
pub fn conversation_dir_name(channel: &str) -> Option<String> {
    let name: String = channel
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    (!name.is_empty() && name.chars().any(|c| c.is_ascii_alphanumeric())).then_some(name)
}

/// What the file in `workspace` gives `stamp`, compared by `keys`. `None`
/// for a run the file does not bind (owner and trusted runs).
#[must_use]
pub fn reach_by_keys(
    workspace: &Path,
    stamp: &ActingFor,
    keys: &dyn peko_subject::ActorKeys,
) -> Option<Reach> {
    if !stamp.is_untrusted() {
        return None;
    }
    let same_actor = |a: &str, b: &str| match (
        a.parse::<peko_subject::ActorRef>(),
        b.parse::<peko_subject::ActorRef>(),
    ) {
        (Ok(a), Ok(b)) => keys.same_actor(&a, &b),
        _ => a == b,
    };
    Some(Access::load(workspace).for_stamp(stamp, &same_actor))
}

/// Add `grant` to `actor`'s section of the `ACCESS.toml` in `workspace`
/// (the owner's *Always*), keeping the file's comments. An actor without
/// a section gets one that starts as a copy of `[visitors]`, since a
/// section replaces the default whole. The result is written only if it
/// still parses.
pub fn grant_always(
    workspace: &Path,
    actor: &str,
    same_actor: &dyn Fn(&str, &str) -> bool,
    grant: &Grant,
) -> Result<(), String> {
    use toml_edit::{Array, DocumentMut, Item, Table};

    let path = workspace.join(ACCESS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            seed_text(&legacy_visitor_servers(workspace))
        }
        Err(e) => return Err(e.to_string()),
    };
    // A file that is refused must be fixed by hand first: adding to it
    // would not take effect, and could hide what is wrong.
    Access::parse(&text).map_err(|e| format!("ACCESS.toml is not valid ({e})"))?;
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| e.to_string())?;

    let existing = doc
        .get("contact")
        .and_then(Item::as_table)
        .and_then(|contacts| {
            contacts
                .iter()
                .map(|(key, _)| key.to_string())
                .find(|key| key == actor || same_actor(key, actor))
        });
    let key = match existing {
        Some(key) => key,
        None => {
            let template = doc
                .get("visitors")
                .and_then(Item::as_table)
                .cloned()
                .unwrap_or_default();
            let mut section = Table::new();
            for (name, item) in &template {
                if let Some(value) = item.as_value() {
                    let mut value = value.clone();
                    value.decor_mut().clear();
                    section.insert(name, Item::Value(value));
                }
            }
            if !doc.contains_key("contact") {
                let mut contacts = Table::new();
                contacts.set_implicit(true);
                doc.insert("contact", Item::Table(contacts));
            }
            let contacts = doc["contact"]
                .as_table_mut()
                .ok_or("`contact` in ACCESS.toml is not a table")?;
            contacts.insert(actor, Item::Table(section));
            actor.to_string()
        }
    };
    let section = doc["contact"][key.as_str()]
        .as_table_mut()
        .ok_or("the contact's section in ACCESS.toml is not a table")?;
    let list = section
        .entry(grant.key())
        .or_insert(Item::Value(Array::new().into()))
        .as_array_mut()
        .ok_or_else(|| format!("`{}` in ACCESS.toml is not a list", grant.key()))?;
    if !list.iter().any(|v| v.as_str() == Some(grant.value())) {
        list.push(grant.value());
    }

    let updated = doc.to_string();
    Access::parse(&updated).map_err(|e| format!("the change would not be valid ({e})"))?;
    std::fs::write(&path, updated).map_err(|e| e.to_string())
}

/// The bytes under `dir`, following no links. Missing is zero.
#[must_use]
pub fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => dir_size(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

/// A conversation's own folder under `workspace`, absolute.
#[must_use]
pub fn own_path(workspace: &Path, reach: &Reach) -> Option<PathBuf> {
    reach.own_dir.as_deref().map(|own| workspace.join(own))
}

/// MCP servers under `workspace/mcp/` whose definition carries the
/// retired `untrusted_ok: true` (ADR-078 D6): what the owner had already
/// opened to visitors before `ACCESS.toml` listed them.
#[must_use]
pub fn legacy_visitor_servers(workspace: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(workspace.join("mcp")) else {
        return Vec::new();
    };
    let marked = |path: &Path| -> bool {
        let Ok(text) = std::fs::read_to_string(path) else {
            return false;
        };
        let value: Option<serde_json::Value> = if path.extension().is_some_and(|e| e == "json") {
            serde_json::from_str(&text).ok()
        } else {
            serde_yaml::from_str(&text).ok()
        };
        value.is_some_and(|v| {
            v.get("untrusted_ok").and_then(serde_json::Value::as_bool) == Some(true)
        })
    };
    let mut servers: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            if path.is_dir() {
                let found = ["server.json", "manifest.yaml"]
                    .iter()
                    .any(|file| marked(&path.join(file)));
                return found.then_some(name);
            }
            let stem = path.file_stem()?.to_string_lossy().into_owned();
            let definition = matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("json" | "yaml" | "yml")
            );
            (definition && marked(&path)).then_some(stem)
        })
        .filter(|id| {
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        })
        .collect();
    servers.sort();
    servers.dedup();
    servers
}

/// The seeded file's text, with `servers` listed for visitors.
fn seed_text(servers: &[String]) -> String {
    if servers.is_empty() {
        return SEED.to_string();
    }
    let list = servers
        .iter()
        .map(|id| format!("\"{id}\""))
        .collect::<Vec<_>>()
        .join(", ");
    SEED.replacen("mcp = []", &format!("mcp = [{list}]"), 1)
}

/// Create `ACCESS.toml` when missing (ADR-113 D9), listing the MCP
/// servers the owner had marked `untrusted_ok`. An existing file is the
/// owner's and is never touched. Returns whether it was written.
pub fn seed(workspace: &Path) -> std::io::Result<bool> {
    let path = workspace.join(ACCESS_FILE);
    if path.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(workspace)?;
    std::fs::write(path, seed_text(&legacy_visitor_servers(workspace)))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use peko_tools_core::{Origin, Trust};

    fn exact(a: &str, b: &str) -> bool {
        a == b
    }

    fn visitor(actor: &str, channel: &str) -> ActingFor {
        ActingFor {
            actor: actor.into(),
            name: None,
            trust: Trust::Untrusted,
            compartment: Compartment::Conversation {
                channel: channel.into(),
            },
            origin: Origin::Ingress { channel: None },
            owner_request: false,
        }
    }

    const SAMPLE: &str = r#"
[visitors]
tools = ["Read", "Fetch"]
read = ["public/", "kb/CONVENTIONS.md"]
own = "contacts/{conversation}/"

[contact."user:hub:alice"]
tools = ["Read", "Write", "MCP"]
mcp = ["orders"]
read = ["public", "playbooks/"]
write = ["shared/drafts"]
own = "contacts/{conversation}/"
own_max_bytes = 1000
"#;

    #[test]
    fn the_seeded_file_parses_and_grants_no_floor_tool() {
        let access = Access::seeded();
        assert!(access.error.is_none());
        assert!(access.visitors.allows_tool("Read"));
        assert!(!access.visitors.allows_tool("Bash"));
        assert!(!access.visitors.allows_tool("Plan"));
        assert!(!access.visitors.allows_tool("MCP"));
        assert_eq!(access.visitors.own_base.as_deref(), Some("contacts"));
        assert_eq!(access.visitors.own_max_bytes, DEFAULT_OWN_MAX_BYTES);
    }

    #[test]
    fn a_contact_section_replaces_the_default_whole() {
        let access = Access::parse(SAMPLE).unwrap();
        let alice = access.rule_for("user:hub:alice", &exact);
        assert!(alice.allows_tool("MCP"));
        assert!(alice.allows_mcp("orders"));
        assert!(!alice.allows_tool("Fetch"), "sections do not merge");
        assert_eq!(alice.own_max_bytes, 1000);
        let other = access.rule_for("user:hub:bob", &exact);
        assert!(other.allows_tool("Fetch"));
        assert!(!other.allows_mcp("orders"));
    }

    #[test]
    fn a_contact_section_matches_under_any_alias() {
        let access = Access::parse(SAMPLE).unwrap();
        let alias = |a: &str, b: &str| a == "user:hub:alice" && b == "user:local:al";
        assert!(access.rule_for("user:local:al", &alias).allows_tool("MCP"));
    }

    #[test]
    fn floor_tools_and_unknown_keys_refuse_the_whole_file() {
        let bash = "[visitors]\ntools = [\"Bash\"]\n";
        assert!(Access::parse(bash).unwrap_err().contains("Bash"));
        let plan = "[contact.\"user:hub:a\"]\ntools = [\"Plan\"]\n";
        assert!(Access::parse(plan).unwrap_err().contains("Plan"));
        assert!(Access::parse("[visitors]\ntool = [\"Read\"]\n").is_err());
        assert!(Access::parse("[guests]\n").is_err());
    }

    #[test]
    fn paths_must_stay_inside_the_workspace() {
        for bad in ["/etc", "../x", "a/../../b", "~/x", ".", ""] {
            let text = format!("[visitors]\nread = [\"{bad}\"]\n");
            assert!(Access::parse(&text).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn own_must_be_a_folder_per_conversation_outside_the_protected_set() {
        for bad in [
            "contacts/",
            "{conversation}/",
            "contacts/{conversation}/x",
            "kb/inbox/{conversation}/",
            "roles/{conversation}/",
            "kb/{conversation}/",
        ] {
            let text = format!("[visitors]\nown = \"{bad}\"\n");
            assert!(Access::parse(&text).is_err(), "{bad} should be refused");
        }
        let ok = Access::parse("[visitors]\nown = \"people/notes/{conversation}\"\n").unwrap();
        assert_eq!(ok.visitors.own_base.as_deref(), Some("people/notes"));
    }

    #[test]
    fn a_visitor_reaches_its_own_folder_and_the_listed_paths_only() {
        let access = Access::parse(SAMPLE).unwrap();
        let reach = access.for_stamp(&visitor("user:hub:bob", "chan_1"), &exact);
        let ws = Path::new("/ws");
        let at = |p: &str| ws.join(p);
        assert_eq!(reach.own_dir.as_deref(), Some("contacts/chan_1"));
        assert!(reach
            .path_refusal(ws, &at("contacts/chan_1/notes.md"), true)
            .is_none());
        assert!(reach
            .path_refusal(ws, &at("public/faq.md"), false)
            .is_none());
        assert!(reach
            .path_refusal(ws, &at("kb/CONVENTIONS.md"), false)
            .is_none());
        let read_only = reach.path_refusal(ws, &at("public/faq.md"), true).unwrap();
        assert!(read_only.reason().contains("read-only"), "{read_only:?}");
        assert!(matches!(
            reach.path_refusal(ws, &at("kb/MEMORY.md"), false),
            Some(Unreached::Unlisted { relative, .. }) if relative == "kb/MEMORY.md"
        ));
        // Outside the workspace, and the workspace root (a search from
        // it would read every conversation's folder): never, whoever asks.
        for never in [Path::new("/etc/hosts"), ws] {
            assert!(
                matches!(
                    reach.path_refusal(ws, never, false),
                    Some(Unreached::Never(_))
                ),
                "{}",
                never.display()
            );
        }
        assert!(reach.path_refusal(ws, &at("kb/MEMORY.md"), false).is_some());
        assert!(reach.path_refusal(ws, &at("publicity/x"), false).is_some());
        assert!(reach.path_refusal(ws, ws, false).is_some());
        assert!(reach
            .path_refusal(ws, Path::new("/etc/hosts"), false)
            .is_some());
    }

    #[test]
    fn another_conversations_folder_is_never_reachable() {
        let access = Access::parse(SAMPLE).unwrap();
        let reach = access.for_stamp(&visitor("user:hub:bob", "chan_1"), &exact);
        let ws = Path::new("/ws");
        let other = reach
            .path_refusal(ws, &ws.join("contacts/chan_2/notes.md"), false)
            .unwrap();
        assert!(
            matches!(&other, Unreached::Never(reason) if reason.contains("another conversation")),
            "{other:?}"
        );
        // The owner's approval of one path never opens them either.
        let granted = reach.with_grant(&Grant::Read("contacts/chan_2/notes.md".into()));
        assert!(matches!(
            granted.path_refusal(ws, &ws.join("contacts/chan_2/notes.md"), false),
            Some(Unreached::Never(_))
        ));
        let granted = reach.with_grant(&Grant::Read("kb/MEMORY.md".into()));
        assert!(granted
            .path_refusal(ws, &ws.join("kb/MEMORY.md"), false)
            .is_none());
        assert!(reach
            .path_refusal(ws, &ws.join("contacts"), false)
            .is_some());
        assert!(reach
            .path_refusal(ws, &ws.join("contacts/chan_1/a"), true)
            .is_none());
    }

    #[test]
    fn a_shared_path_may_not_contain_the_conversations_folders() {
        // Sharing the folder that holds them, or one above it, in any
        // section: a recursive search from there would read them all.
        for shared in ["contacts", "people"] {
            let text = format!(
                "[visitors]\nread = [\"{shared}\"]\nown = \"contacts/{{conversation}}/\"\n\n\
                 [contact.\"user:hub:a\"]\nown = \"people/vip/{{conversation}}/\"\n"
            );
            let error = Access::parse(&text).unwrap_err();
            assert!(error.contains("cannot be shared"), "{error}");
        }
    }

    #[test]
    fn a_run_outside_a_conversation_has_no_folder() {
        let access = Access::seeded();
        let mut stamp = visitor("user:hub:bob", "chan_1");
        stamp.compartment = Compartment::Detached;
        assert!(access.for_stamp(&stamp, &exact).own_dir.is_none());
    }

    #[test]
    fn conversation_folder_names_are_path_safe() {
        assert_eq!(
            conversation_dir_name("chan_ab-12").as_deref(),
            Some("chan_ab-12")
        );
        assert_eq!(conversation_dir_name("../x").as_deref(), Some("___x"));
        assert_eq!(conversation_dir_name(".."), None);
        assert_eq!(conversation_dir_name(""), None);
    }

    #[test]
    fn a_broken_file_leaves_the_minimum_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Access::load(dir.path()).error.is_none(),
            "missing is seeded"
        );
        std::fs::write(
            dir.path().join(ACCESS_FILE),
            "[visitors]\ntools = [\"Bash\"]\n",
        )
        .unwrap();
        let access = Access::load(dir.path());
        assert!(access.error.as_deref().unwrap().contains("Bash"));
        assert!(access.visitors.allows_tool("ChannelSend"));
        assert!(!access.visitors.allows_tool("Fetch"));
        assert!(access.visitors.read.is_empty());
        assert_eq!(access.visitors.own_base.as_deref(), Some("contacts"));
    }

    #[test]
    fn seeding_is_create_once() {
        let dir = tempfile::tempdir().unwrap();
        assert!(seed(dir.path()).unwrap());
        std::fs::write(dir.path().join(ACCESS_FILE), "[visitors]\n").unwrap();
        assert!(!seed(dir.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(ACCESS_FILE)).unwrap(),
            "[visitors]\n"
        );
    }

    /// ADR-113 D9: servers the owner had opened to visitors stay open.
    #[test]
    fn seeding_carries_over_servers_marked_for_visitors() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = dir.path().join("mcp");
        for (id, body) in [
            ("docs", r#"{"schema_version":1,"untrusted_ok":true}"#),
            ("mail", r#"{"schema_version":1,"untrusted_ok":false}"#),
            ("crm", r#"{"schema_version":1}"#),
        ] {
            std::fs::create_dir_all(mcp.join(id)).unwrap();
            std::fs::write(mcp.join(id).join("server.json"), body).unwrap();
        }
        std::fs::create_dir_all(mcp.join("faq")).unwrap();
        std::fs::write(
            mcp.join("faq/manifest.yaml"),
            "schema_version: 1\nuntrusted_ok: true\n",
        )
        .unwrap();
        std::fs::write(mcp.join("flat.json"), r#"{"untrusted_ok":true}"#).unwrap();
        assert_eq!(legacy_visitor_servers(dir.path()), ["docs", "faq", "flat"]);

        assert!(seed(dir.path()).unwrap());
        let access = Access::load(dir.path());
        assert!(access.error.is_none());
        assert_eq!(access.visitors.mcp, ["docs", "faq", "flat"]);
        assert!(access.visitors.allows_tool("Read"));
        assert!(!access.visitors.allows_mcp("mail"));
    }

    /// ADR-113 D10: requests reach the owner only where a section says so.
    #[test]
    fn ask_owner_is_off_unless_a_section_turns_it_on() {
        assert!(!Access::seeded().visitors.ask_owner);
        assert!(!Access::minimum("x".into()).visitors.ask_owner);
        let access = Access::parse(
            "[visitors]\ntools = [\"Read\"]\n\n[contact.\"user:hub:alice\"]\nask_owner = true\n",
        )
        .unwrap();
        assert!(!access.visitors.ask_owner);
        assert!(access.rule_for("user:hub:alice", &exact).ask_owner);
        assert!(Access::parse("[visitors]\nask_owner = \"yes\"\n").is_err());
    }

    /// ADR-113 D10: *Always* adds to the actor's own section, keeps the
    /// owner's comments, and starts a new section from `[visitors]`.
    #[test]
    fn always_adds_the_grant_to_the_actors_section() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::write(
            ws.join(ACCESS_FILE),
            "# my rules\n[visitors]\ntools = [\"Read\", \"Task\"] # basics\nread = [\"public\"]\n\
             own = \"contacts/{conversation}/\"\nask_owner = true\n\n\
             [contact.\"user:hub:alice\"]\ntools = [\"Read\"]\n",
        )
        .unwrap();

        // An existing section, found under an alias.
        let alias = |a: &str, b: &str| a == "user:hub:alice" && b == "user:local:al";
        grant_always(ws, "user:local:al", &alias, &Grant::Tool("Fetch".into())).unwrap();
        grant_always(
            ws,
            "user:local:al",
            &alias,
            &Grant::Read("playbooks".into()),
        )
        .unwrap();
        grant_always(ws, "user:local:al", &alias, &Grant::Tool("Fetch".into())).unwrap();
        let access = Access::load(ws);
        assert!(access.error.is_none(), "{:?}", access.error);
        let alice = access.rule_for("user:hub:alice", &exact);
        assert_eq!(alice.tools, ["Read", "Fetch"]);
        assert_eq!(alice.read, ["playbooks"]);
        assert_eq!(access.contacts.len(), 1, "no second section for the alias");

        // A new section starts as what the visitor already had.
        grant_always(ws, "user:hub:bob", &exact, &Grant::Mcp("orders".into())).unwrap();
        grant_always(ws, "user:hub:bob", &exact, &Grant::Write("drafts".into())).unwrap();
        let access = Access::load(ws);
        assert!(access.error.is_none(), "{:?}", access.error);
        let bob = access.rule_for("user:hub:bob", &exact);
        assert_eq!(bob.tools, ["Read", "Task"]);
        assert_eq!(bob.read, ["public"]);
        assert_eq!(bob.mcp, ["orders"]);
        assert_eq!(bob.write, ["drafts"]);
        assert_eq!(bob.own_base.as_deref(), Some("contacts"));
        assert!(bob.ask_owner);
        // Everyone else is as before.
        assert_eq!(access.visitors.tools, ["Read", "Task"]);
        assert!(access.visitors.mcp.is_empty());

        let text = std::fs::read_to_string(ws.join(ACCESS_FILE)).unwrap();
        assert!(text.starts_with("# my rules\n[visitors]\n"), "{text}");
        assert!(text.contains("# basics"), "{text}");
        assert!(text.contains("[contact.\"user:hub:bob\"]"), "{text}");
    }

    #[test]
    fn always_starts_from_the_seeded_file_and_refuses_a_broken_one() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        grant_always(ws, "chan_team", &exact, &Grant::Read("handbook".into())).unwrap();
        let access = Access::load(ws);
        assert!(access.error.is_none(), "{:?}", access.error);
        let team = access.rule_for("chan_team", &exact);
        assert_eq!(team.read, ["public", "kb/CONVENTIONS.md", "handbook"]);
        assert!(team.allows_tool("Read"));

        // A grant that would not be valid is not written.
        let before = std::fs::read_to_string(ws.join(ACCESS_FILE)).unwrap();
        let refused = grant_always(ws, "chan_team", &exact, &Grant::Tool("Bash".into()));
        assert!(refused.unwrap_err().contains("Bash"));
        let refused = grant_always(ws, "chan_team", &exact, &Grant::Read("contacts".into()));
        assert!(refused.is_err());
        assert_eq!(
            std::fs::read_to_string(ws.join(ACCESS_FILE)).unwrap(),
            before
        );

        std::fs::write(ws.join(ACCESS_FILE), "[visitors]\ntools = [\"Bash\"]\n").unwrap();
        let broken = grant_always(ws, "chan_team", &exact, &Grant::Tool("Fetch".into()));
        assert!(broken.unwrap_err().contains("not valid"));
    }
}
