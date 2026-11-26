//! The seed a peko grew from (ADR-127 D10).
//!
//! At creation the runtime writes the exact seed text to
//! `seed/origin.seed.toml` in the new peko's workspace, and beside it
//! `seed/origin.toml`: its SHA-256, where it came from, the runtime
//! version, and when. A peko created without a seed gets what `peko seed`
//! would have written for it at that moment. Only the runtime writes
//! `seed/` (the floor refuses every run's file-tool write there, and no
//! seed may fill it), and only once.
//!
//! The origin is what `peko seed --diff` compares a peko with, and what
//! an exported seed names as `[grown_from]`.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::seed::{parse_seed, SeedFile, SEED_EXPORT_DIRS};

/// The folder the origin lives in, relative to the workspace.
pub const ORIGIN_DIR: &str = "seed";
/// The exact seed text the peko grew from.
pub const ORIGIN_SEED_FILE: &str = "seed/origin.seed.toml";
/// What the runtime knows about it.
pub const ORIGIN_INFO_FILE: &str = "seed/origin.toml";
/// The table an exported seed names its origin in.
pub const GROWN_FROM_KEY: &str = "grown_from";

/// `sha256:<hex>` of `text` as UTF-8, the digest PekoHub keeps for each
/// published seed version.
#[must_use]
pub fn seed_digest(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{}", hex::encode(Sha256::digest(text.as_bytes())))
}

/// Where a peko's seed came from, as the client that created it said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OriginSource {
    /// No seed: the runtime's default definition.
    Default,
    /// A seed file on this computer (`peko create -s <file>`).
    File { path: String },
    /// A seed published on PekoHub, by its owner's handle, name and version.
    Pekohub {
        owner: String,
        name: String,
        version: String,
    },
    /// Seed text a client sent without saying where it came from.
    Client {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
}

impl OriginSource {
    /// The source a client named (`PrincipalCreate` `seed_from`): a
    /// PekoHub reference `pekohub:<owner>/<name>@<version>`, else a path.
    /// Nothing about it is checked; it is provenance as the client gave it.
    #[must_use]
    pub fn named(from: Option<&str>) -> Self {
        let Some(from) = from.map(str::trim).filter(|f| !f.is_empty()) else {
            return Self::Client { from: None };
        };
        if let Some(reference) = from.strip_prefix("pekohub:") {
            if let Some((path, version)) = reference.rsplit_once('@') {
                if let Some((owner, name)) = path.split_once('/') {
                    if [owner, name, version].iter().all(|part| !part.is_empty()) {
                        return Self::Pekohub {
                            owner: owner.to_string(),
                            name: name.to_string(),
                            version: version.to_string(),
                        };
                    }
                }
            }
            return Self::Client {
                from: Some(from.to_string()),
            };
        }
        Self::File {
            path: from.to_string(),
        }
    }
}

/// `seed/origin.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginInfo {
    /// [`seed_digest`] of `seed/origin.seed.toml`.
    pub digest: String,
    /// The runtime version that created the peko.
    pub runtime_version: String,
    /// When, RFC 3339.
    pub created_at: String,
    pub source: OriginSource,
}

impl OriginInfo {
    /// The `[grown_from]` table an exported seed carries: the origin's
    /// digest, and its PekoHub name and version when it came from there.
    #[must_use]
    pub fn grown_from(&self) -> toml::Table {
        let mut table = toml::Table::new();
        table.insert("digest".into(), self.digest.clone().into());
        if let OriginSource::Pekohub {
            owner,
            name,
            version,
        } = &self.source
        {
            table.insert("seed".into(), format!("{owner}/{name}").into());
            table.insert("version".into(), version.clone().into());
        }
        table
    }
}

/// Write a new peko's origin: the exact `text` and what is known of it.
/// Fails when either file exists: the origin is written once.
///
/// # Errors
/// When the folder or a file cannot be written, or already exists.
pub fn write_origin(
    workspace: &Path,
    text: &str,
    source: OriginSource,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<OriginInfo> {
    let info = OriginInfo {
        digest: seed_digest(text),
        runtime_version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: now.to_rfc3339(),
        source,
    };
    let dir = workspace.join(ORIGIN_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let header = "# Written by the Peko runtime when this peko was created (ADR-127).\n\
                  # `seed/origin.seed.toml` is the seed it grew from; only the runtime writes here.\n\n";
    let info_text = format!(
        "{header}{}",
        toml::to_string_pretty(&info).context("write the origin")?
    );
    for (relative, contents) in [(ORIGIN_SEED_FILE, text), (ORIGIN_INFO_FILE, &info_text)] {
        use std::io::Write as _;
        let path = workspace.join(relative);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .and_then(|mut file| file.write_all(contents.as_bytes()))
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(info)
}

/// A peko's origin, when it has one: what is known of it, and the seed
/// text. `None` for a peko created before origins were kept.
///
/// # Errors
/// When a file exists but cannot be read, `origin.toml` does not parse, or
/// the text does not match its digest.
pub fn read_origin(workspace: &Path) -> Result<Option<(OriginInfo, String)>> {
    let info_path = workspace.join(ORIGIN_INFO_FILE);
    let info = match std::fs::read_to_string(&info_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", info_path.display())),
    };
    let info: OriginInfo =
        toml::from_str(&info).with_context(|| format!("parse {}", info_path.display()))?;
    let seed_path = workspace.join(ORIGIN_SEED_FILE);
    let text = std::fs::read_to_string(&seed_path)
        .with_context(|| format!("read {}", seed_path.display()))?;
    anyhow::ensure!(
        seed_digest(&text) == info.digest,
        "{} does not match the digest in {}: it was changed after the peko was created",
        seed_path.display(),
        info_path.display()
    );
    Ok(Some((info, text)))
}

/// How one part of a seed differs between the origin and now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartDiff {
    /// `peko.toml`, `persona`, or a file's path.
    pub part: String,
    pub change: Change,
    /// The lines, each prefixed `  `, `- ` (origin) or `+ ` (now).
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Added,
    Removed,
    Changed,
}

impl Change {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Changed => "changed",
        }
    }
}

/// What `peko seed --diff` reports.
#[derive(Debug, Clone, Default)]
pub struct OriginDiff {
    pub parts: Vec<PartDiff>,
    /// Files the origin carried that an export never does (a seed may
    /// fill any workspace file; `peko seed` exports only capabilities).
    pub not_exported: Vec<String>,
}

/// How `current` (what `peko seed` writes today) differs from `origin`
/// (the seed the peko grew from), part by part: the definition, the
/// persona, and each file. Both are read the same way, so what creation
/// sets for every peko (its name, its model, defaults filled in) is no
/// difference. MCP files are compared only `with_mcp`, as `peko seed`
/// carries them only then.
///
/// # Errors
/// When either text is not a seed.
pub fn origin_diff(origin: &str, current: &str, with_mcp: bool) -> Result<OriginDiff> {
    let origin = parse_seed(origin).context("the origin seed")?;
    let current = parse_seed(current).context("the seed of this peko today")?;
    let mut diff = OriginDiff::default();

    let definition = |config: &super::config::PrincipalConfig| -> Result<String> {
        let mut table = toml::Table::try_from(config).context("read the definition")?;
        for key in
            super::seed::EXISTENCE_KEYS
                .iter()
                .chain(&["quota", "name", "preferred_model_id"])
        {
            table.remove(*key);
        }
        if let Some(toml::Value::Table(routing)) = table.get_mut("routing") {
            routing.remove("root_prompt");
        }
        Ok(toml::to_string_pretty(&table)?)
    };
    push_part(
        &mut diff,
        "peko.toml",
        Some(definition(&origin.config)?),
        Some(definition(&current.config)?),
    );
    let persona = |p: &Option<String>| p.as_deref().map(str::trim).map(str::to_string);
    push_part(
        &mut diff,
        "persona",
        persona(&origin.persona).filter(|p| !p.is_empty()),
        persona(&current.persona).filter(|p| !p.is_empty()),
    );

    let exported = |file: &SeedFile| {
        let top = file.path.split('/').next().unwrap_or_default();
        file.sets_visitor_rules() || (SEED_EXPORT_DIRS.contains(&top) && (with_mcp || top != "mcp"))
    };
    let defaults = crate::principal::genesis::default_role_files();
    let mut origin_files = std::collections::BTreeMap::new();
    for file in &origin.files {
        if !exported(file) {
            if with_mcp || !file.path.starts_with("mcp/") {
                diff.not_exported.push(file.path.clone());
            }
            continue;
        }
        // As an export reads them: default roles and rules are left out,
        // and rules lose their sections for particular actors.
        let content = match file.path.as_str() {
            crate::security::access::ACCESS_FILE => {
                crate::security::access::for_seed(&file.content)
            }
            crate::security::policy::POLICY_FILE => {
                crate::security::policy::for_seed(&file.content)
            }
            path if defaults
                .iter()
                .any(|(p, d)| p == path && *d == file.content) =>
            {
                None
            }
            _ => Some(file.content.clone()),
        };
        if let Some(content) = content {
            origin_files.insert(file.path.clone(), content);
        }
    }
    let mut current_files: std::collections::BTreeMap<String, String> = current
        .files
        .iter()
        .filter(|file| exported(file))
        .map(|file| (file.path.clone(), file.content.clone()))
        .collect();
    let mut paths: Vec<String> = origin_files
        .keys()
        .chain(current_files.keys())
        .cloned()
        .collect();
    paths.sort();
    paths.dedup();
    for path in paths {
        let before = origin_files.remove(&path);
        let after = current_files.remove(&path);
        push_part(&mut diff, &path, before, after);
    }
    Ok(diff)
}

fn push_part(diff: &mut OriginDiff, part: &str, before: Option<String>, after: Option<String>) {
    let change = match (&before, &after) {
        (None, None) => return,
        (Some(b), Some(a)) if b == a => return,
        (None, Some(_)) => Change::Added,
        (Some(_), None) => Change::Removed,
        (Some(_), Some(_)) => Change::Changed,
    };
    diff.parts.push(PartDiff {
        part: part.to_string(),
        change,
        lines: line_diff(
            before.as_deref().unwrap_or_default(),
            after.as_deref().unwrap_or_default(),
        ),
    });
}

/// The most line pairs [`line_diff`] compares one by one; past it, the
/// whole old text is shown removed and the new one added.
const LINE_DIFF_MAX_CELLS: usize = 4_000_000;

/// Lines that differ between `before` and `after`, with two lines of
/// context around each change and `…` between distant ones.
fn line_diff(before: &str, after: &str) -> Vec<String> {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    // Each line marked: ' ' kept, '-' only in before, '+' only in after.
    let mut marked: Vec<(char, &str)> = Vec::new();
    if old.len().saturating_mul(new.len()) > LINE_DIFF_MAX_CELLS {
        marked.extend(old.iter().map(|l| ('-', *l)));
        marked.extend(new.iter().map(|l| ('+', *l)));
    } else {
        // Longest common subsequence, from the end.
        let (n, m) = (old.len(), new.len());
        let mut lcs = vec![vec![0u32; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i][j] = if old[i] == new[j] {
                    lcs[i + 1][j + 1] + 1
                } else {
                    lcs[i + 1][j].max(lcs[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && old[i] == new[j] {
                marked.push((' ', old[i]));
                i += 1;
                j += 1;
            } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
                marked.push(('-', old[i]));
                i += 1;
            } else {
                marked.push(('+', new[j]));
                j += 1;
            }
        }
    }
    const CONTEXT: usize = 2;
    let changed: Vec<usize> = marked
        .iter()
        .enumerate()
        .filter(|(_, (mark, _))| *mark != ' ')
        .map(|(index, _)| index)
        .collect();
    let near = |index: usize| {
        changed
            .iter()
            .any(|c| index + CONTEXT >= *c && index <= c + CONTEXT)
    };
    let mut out = Vec::new();
    let mut skipped = false;
    for (index, (mark, line)) in marked.iter().enumerate() {
        if near(index) {
            if skipped && !out.is_empty() {
                out.push("  …".to_string());
            }
            skipped = false;
            out.push(format!("{mark} {line}").trim_end().to_string());
        } else {
            skipped = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_origin_is_written_once_and_read_back_by_its_digest() {
        let ws = tempfile::tempdir().unwrap();
        let text = "name = \"ada\"\npersona = \"Kind.\"\n";
        let info = write_origin(
            ws.path(),
            text,
            OriginSource::named(Some("pekohub:maya/ada@1.2.0")),
            chrono::Utc::now(),
        )
        .unwrap();
        assert_eq!(info.digest, seed_digest(text));
        assert!(info.digest.starts_with("sha256:") && info.digest.len() == 7 + 64);
        let (read, back) = read_origin(ws.path()).unwrap().unwrap();
        assert_eq!((read, back.as_str()), (info.clone(), text));
        assert_eq!(
            info.grown_from().get("seed").and_then(toml::Value::as_str),
            Some("maya/ada")
        );
        // Once only.
        assert!(write_origin(
            ws.path(),
            "name = \"x\"\n",
            OriginSource::Default,
            chrono::Utc::now()
        )
        .is_err());
        // A changed text no longer matches its digest.
        std::fs::write(ws.path().join(ORIGIN_SEED_FILE), "name = \"eve\"\n").unwrap();
        assert!(read_origin(ws.path()).is_err());
        // No origin at all reads as none.
        assert!(read_origin(tempfile::tempdir().unwrap().path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_named_source_is_kept_as_the_client_said() {
        assert_eq!(
            OriginSource::named(Some("/tmp/ada.seed.toml")),
            OriginSource::File {
                path: "/tmp/ada.seed.toml".into()
            }
        );
        assert_eq!(
            OriginSource::named(Some("pekohub:broken")),
            OriginSource::Client {
                from: Some("pekohub:broken".into())
            }
        );
        assert_eq!(
            OriginSource::named(None),
            OriginSource::Client { from: None }
        );
    }

    #[test]
    fn the_diff_shows_only_what_changed_part_by_part() {
        let origin = r#"
name = "ada"
preferred_model_id = "someone-elses"
persona = "Kind."

[identity]
description = "Reads papers"

[files]
"roles/researcher.md" = "---\nname: researcher\ndescription: r\n---\nCite sources.\nBe brief.\n"
"kb/faq.md" = "Q: A\n"
"mcp/x/server.json" = "{}"
"#;
        // Creation sets the name and the model: no difference.
        let same = r#"
name = "sprout"
preferred_model_id = "mine"
persona = "Kind."

[identity]
description = "Reads papers"

[files]
"roles/researcher.md" = "---\nname: researcher\ndescription: r\n---\nCite sources.\nBe brief.\n"
"#;
        let diff = origin_diff(origin, same, false).unwrap();
        assert!(diff.parts.is_empty(), "{:?}", diff.parts);
        assert_eq!(diff.not_exported, ["kb/faq.md"]);

        let grown = r#"
name = "sprout"
persona = "Kind, and quick."

[identity]
description = "Reads papers"

[files]
"roles/researcher.md" = "---\nname: researcher\ndescription: r\n---\nCite sources.\nQuote the page.\n"
"skills/greet/SKILL.md" = "---\nname: greet\ndescription: g\n---\nHello\n"
"#;
        let diff = origin_diff(origin, grown, false).unwrap();
        let parts: Vec<(&str, Change)> = diff
            .parts
            .iter()
            .map(|p| (p.part.as_str(), p.change))
            .collect();
        assert_eq!(
            parts,
            [
                ("persona", Change::Changed),
                ("roles/researcher.md", Change::Changed),
                ("skills/greet/SKILL.md", Change::Added),
            ]
        );
        assert_eq!(
            diff.parts[1].lines,
            [
                "  ---",
                "  Cite sources.",
                "- Be brief.",
                "+ Quote the page."
            ]
        );
    }

    #[test]
    fn line_diff_keeps_two_lines_of_context_and_marks_gaps() {
        let before = "a\nb\nc\nd\ne\nf\ng\nh\ni\n";
        let after = "a\nB\nc\nd\ne\nf\ng\nh\nI\n";
        assert_eq!(
            line_diff(before, after),
            ["  a", "- b", "+ B", "  c", "  d", "  …", "  g", "  h", "- i", "+ I"]
        );
    }
}
