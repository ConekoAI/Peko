//! Workspace-scanning prompt handler for the `skills` prompt section
//! (Part B: dynamic per-turn workspace catalog). The section rides the
//! tail `<runtime-context>` message, not the frozen system prompt.
//!
//! Registered **once** per core (see `principal/context.rs`). At invoke
//! time the handler resolves the workspace from the hook context's
//! `ToolRuntimeContext`, scans `<workspace>/skills/<name>/SKILL.md`,
//! parses each skill's frontmatter, and renders one catalog line per
//! skill: `- {name}: {description} (skills/{name}/SKILL.md)`.
//!
//! Presence in the workspace = visible (ADR-047): there is deliberately
//! **no** capability or active-extension filter. The skill `name` is the
//! directory name — the same key `WorkspaceSkillRuntime::resolve_skill`
//! uses — so every rendered line is invocable via the `Skill` tool.
//!
//! The scan result is cached in a `Mutex` keyed on a per-file
//! workspace path and a `(mtime, len)` fingerprint of every `<name>/SKILL.md` — each call
//! `stat`s the scanned files (a handful; cheap) and only re-reads them
//! when the fingerprint changed. Unlike the previous dir-mtime key,
//! in-place edits to an existing SKILL.md invalidate the catalog on
//! the next iteration (ADR-052 D2). That keeps the handler well within
//! the renderer's 2-second hook timeout.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use async_trait::async_trait;
use tracing::warn;

use super::bundled::BUNDLED_SKILLS;
use super::reader::{is_skill_directory, WorkspaceSkillRuntime};
use crate::tools::builtin::skill::{
    parse_yaml_frontmatter_typed, SkillFrontmatter, SkillRuntime, SkillSource,
};

/// Default priority for the skills-catalog prompt section.
pub const SKILL_CATALOG_HOOK_PRIORITY: i32 = 90;

/// Hard cap on the rendered skills catalog. Keeps a pathological
/// workspace (hundreds of skills) from blowing up the system prompt;
/// on overflow whole lines are truncated from the end and a pointer to
/// the on-disk directory is appended instead.
const SKILLS_CATALOG_MAX_BYTES: usize = 8 * 1024;

/// Cache key: per-file `(relative_path, mtime, len)` stats for every
/// scanned file in the catalog dir, sorted by path for determinism.
/// File-level stats (not dir mtime) so in-place content edits
/// invalidate the cache (ADR-052 D2).
type DirFingerprint = Vec<(String, SystemTime, u64)>;

#[derive(Debug, PartialEq, Eq)]
struct CatalogCacheKey {
    workspace: PathBuf,
    files: DirFingerprint,
}

/// Workspace-scanning handler for the `skills` prompt section.
///
/// See the module doc for the scanning/caching contract. A missing
/// workspace, missing `skills/` dir, or empty catalog yields
/// [`HookResult::PassThrough`] so the section is stripped from the
/// prompt.
#[derive(Debug, Default)]
pub struct WorkspaceSkillsPromptHandler {
    cache: Mutex<Option<(CatalogCacheKey, Vec<String>)>>,
}

impl WorkspaceSkillsPromptHandler {
    /// Create a new workspace-scanning skills handler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Render the skills catalog for `workspace`: its own skills, then
    /// the runtime's bundled skills it does not replace (those marked
    /// owner-only only when `owner`). Returns `None` when there is
    /// nothing to render.
    fn render_catalog(&self, workspace: &str, owner: bool) -> Option<String> {
        let skills_dir = Path::new(workspace).join("skills");
        let mut lines = self.workspace_lines(workspace, &skills_dir);
        lines.extend(bundled_lines(&skills_dir, owner));
        lines.sort();
        let text = render_lines(&lines, workspace);
        (!text.is_empty()).then_some(text)
    }

    /// The workspace's catalog lines, using the fingerprint-keyed cache.
    ///
    /// The cache key folds each scanned `<name>/SKILL.md` file's
    /// `(mtime, len)` in, so in-place content edits invalidate the
    /// catalog on the next call — the previous `(dir_mtime,
    /// child_count)` key only caught added/removed entries (dir mtime
    /// doesn't move on content writes).
    fn workspace_lines(&self, workspace: &str, skills_dir: &Path) -> Vec<String> {
        let Some(files) = skills_dir_fingerprint(skills_dir) else {
            return Vec::new();
        };
        let key = CatalogCacheKey {
            workspace: PathBuf::from(workspace),
            files,
        };

        {
            let cache = self.cache.lock().expect("skills catalog cache poisoned");
            if let Some((cached_key, lines)) = &*cache {
                if *cached_key == key {
                    return lines.clone();
                }
            }
        }

        let lines = scan_skills_dir(skills_dir);

        let mut cache = self.cache.lock().expect("skills catalog cache poisoned");
        *cache = Some((key, lines.clone()));
        lines
    }
}

/// Fingerprint every `<name>/SKILL.md` under `skills_dir` as
/// `(relative_path, mtime, len)` entries, sorted by path. `None` when
/// the dir doesn't exist. Files whose metadata can't be read are
/// skipped, mirroring the scanner's skip-unreadable behavior.
fn skills_dir_fingerprint(skills_dir: &Path) -> Option<DirFingerprint> {
    if !is_skill_directory(skills_dir) {
        return None;
    }
    let mut stats = Vec::new();
    let runtime = WorkspaceSkillRuntime::new(skills_dir.to_path_buf());
    for name in runtime.list_skills() {
        let Some(SkillSource::File(skill_md)) = runtime.resolve_skill(&name).map(|e| e.source)
        else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&skill_md) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let rel = skill_md
            .strip_prefix(skills_dir)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        stats.push((rel, mtime, meta.len()));
    }
    stats.sort();
    Some(stats)
}

#[async_trait]
impl crate::tools::prompt_sections::PromptSectionProvider for WorkspaceSkillsPromptHandler {
    fn section(&self) -> &'static str {
        "skills"
    }

    fn priority(&self) -> i32 {
        SKILL_CATALOG_HOOK_PRIORITY
    }

    async fn render(
        &self,
        input: &crate::tools::prompt_sections::PromptSectionInput,
    ) -> Option<String> {
        let workspace = input.workspace.to_string_lossy().to_string();
        if workspace.is_empty() {
            return None;
        }
        let owner = input
            .acting_for
            .as_ref()
            .is_some_and(peko_tools_core::ActingFor::is_owner);
        self.render_catalog(&workspace, owner)
    }
}

/// Scan `<workspace>/skills/<name>/SKILL.md` into sorted catalog lines.
/// Skills whose frontmatter fails to parse are skipped with a warning.
fn scan_skills_dir(skills_dir: &Path) -> Vec<String> {
    let runtime = WorkspaceSkillRuntime::new(skills_dir.to_path_buf());
    let mut lines = Vec::new();
    for name in runtime.list_skills() {
        let Some(entry) = runtime.resolve_skill(&name) else {
            continue;
        };
        let content = match entry.read() {
            Ok(content) => content,
            Err(e) => {
                warn!("Failed to read skill {name}: {e}; skipping skill");
                continue;
            }
        };
        match parse_yaml_frontmatter_typed::<SkillFrontmatter>(&content) {
            Ok((fm, _body)) => {
                lines.push(format!(
                    "- {name}: {} (skills/{name}/SKILL.md)",
                    fm.description
                ));
            }
            Err(e) => {
                warn!("Failed to parse frontmatter of skill {name}: {e}; skipping skill");
            }
        }
    }
    // Deterministic ordering — directory iteration order is
    // platform-dependent.
    lines.sort();
    lines
}

/// Catalog lines for the bundled skills the workspace does not replace;
/// an owner-only one only when `owner`.
fn bundled_lines(skills_dir: &Path, owner: bool) -> Vec<String> {
    let workspace = WorkspaceSkillRuntime::new(skills_dir.to_path_buf());
    BUNDLED_SKILLS
        .iter()
        .filter(|skill| owner || !skill.owner_only)
        .filter(|skill| workspace.resolve_skill(skill.name).is_none())
        .filter_map(|skill| {
            let (fm, _body) = parse_yaml_frontmatter_typed::<SkillFrontmatter>(skill.text).ok()?;
            Some(format!(
                "- {}: {} (ships with the runtime)",
                skill.name, fm.description
            ))
        })
        .collect()
}

/// Join catalog `lines`, capped at [`SKILLS_CATALOG_MAX_BYTES`].
fn render_lines(lines: &[String], workspace: &str) -> String {
    let notice = format!("(more skills in {workspace}/skills/ — list the directory to see all)");
    crate::tools::prompt_sections::render_capped_lines(lines, SKILLS_CATALOG_MAX_BYTES, &notice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::prompt_sections::PromptSectionProvider;

    use tempfile::TempDir;

    #[tokio::test]
    async fn oversized_definition_is_not_advertised_or_invocable() {
        use crate::tools::builtin::skill::{SkillTool, MAX_SKILL_FILE_BYTES};
        use peko_tools_core::Tool;
        let workspace = TempDir::new().unwrap();
        let skills = workspace.path().join("skills");
        make_skill(&skills, "large", "Large skill");
        std::fs::OpenOptions::new()
            .write(true)
            .open(skills.join("large/SKILL.md"))
            .unwrap()
            .set_len((MAX_SKILL_FILE_BYTES + 1) as u64)
            .unwrap();
        assert!(WorkspaceSkillsPromptHandler::new()
            .render(&skills_input(workspace.path()))
            .await
            .is_none());
        let tool = SkillTool::new(std::sync::Arc::new(WorkspaceSkillRuntime::new(skills)));
        assert!(tool
            .execute(serde_json::json!({"name":"large"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("skill_unreadable"));
    }

    fn make_skill(skills_dir: &Path, name: &str, description: &str) {
        let skill_dir = skills_dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n"),
        )
        .unwrap();
    }

    /// Build the provider input for `workspace`.
    fn skills_input(workspace: &Path) -> crate::tools::prompt_sections::PromptSectionInput {
        crate::tools::prompt_sections::PromptSectionInput {
            principal_id: "test-principal".to_string(),
            workspace: workspace.to_path_buf(),
            session_id: String::new(),
            channel_port: None,
            acting_for: None,
        }
    }

    #[tokio::test]
    async fn workspace_skills_handler_renders_catalog() {
        let temp = TempDir::new().unwrap();
        let skills_dir = temp.path().join("skills");
        std::fs::create_dir(&skills_dir).unwrap();
        make_skill(&skills_dir, "docker", "Docker ops");
        make_skill(&skills_dir, "git", "Git workflow");
        // Unparseable frontmatter → skipped (does not break the catalog).
        let bad_dir = skills_dir.join("broken");
        std::fs::create_dir(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("SKILL.md"), "no frontmatter here").unwrap();

        let handler = WorkspaceSkillsPromptHandler::new();
        let text = handler
            .render(&skills_input(temp.path()))
            .await
            .expect("expected catalog text");

        assert!(
            text.contains("- docker: Docker ops (skills/docker/SKILL.md)"),
            "got: {text}"
        );
        assert!(
            text.contains("- git: Git workflow (skills/git/SKILL.md)"),
            "got: {text}"
        );
        assert!(!text.contains("broken"), "got: {text}");
    }

    #[tokio::test]
    async fn workspace_skills_handler_passes_through_without_workspace() {
        let temp = TempDir::new().unwrap();
        let handler = WorkspaceSkillsPromptHandler::new();
        let result = handler.render(&skills_input(temp.path())).await;
        assert!(result.is_none(), "Expected no section, got {result:?}");
    }

    #[tokio::test]
    async fn workspace_skills_handler_passes_through_on_empty_dir() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir(temp.path().join("skills")).unwrap();

        let handler = WorkspaceSkillsPromptHandler::new();
        let result = handler.render(&skills_input(temp.path())).await;
        assert!(
            result.is_none(),
            "Expected no section for empty skills dir, got {result:?}"
        );
    }

    #[tokio::test]
    async fn workspace_skills_handler_rescans_on_dir_mtime_change() {
        let temp = TempDir::new().unwrap();
        let skills_dir = temp.path().join("skills");
        std::fs::create_dir(&skills_dir).unwrap();
        make_skill(&skills_dir, "docker", "Docker ops");

        let handler = WorkspaceSkillsPromptHandler::new();
        let _ws = temp.path().to_string_lossy().to_string();

        // First call scans and caches.
        let first = handler
            .render(&skills_input(temp.path()))
            .await
            .expect("expected catalog text");
        assert!(first.contains("docker"), "got: {first}");
        assert!(!first.contains("git"), "got: {first}");

        // Adding a skill bumps the `skills/` dir mtime → the next call
        // must re-scan rather than serve the cached catalog.
        make_skill(&skills_dir, "git", "Git workflow");

        let second = handler
            .render(&skills_input(temp.path()))
            .await
            .expect("expected catalog text");
        assert!(second.contains("docker"), "got: {second}");
        assert!(second.contains("git"), "got: {second}");
    }

    /// ADR-052 D2: an in-place CONTENT edit to an existing SKILL.md
    /// (same filename, no dir entry added/removed, so the `skills/`
    /// dir mtime is untouched) must invalidate the catalog on the next
    /// call. The previous `(dir_mtime, child_count)` key served the
    /// stale catalog here.
    #[tokio::test]
    async fn workspace_skills_handler_rescans_on_in_place_content_edit() {
        let temp = TempDir::new().unwrap();
        let skills_dir = temp.path().join("skills");
        std::fs::create_dir(&skills_dir).unwrap();
        make_skill(&skills_dir, "docker", "Docker ops");

        let handler = WorkspaceSkillsPromptHandler::new();
        let _ws = temp.path().to_string_lossy().to_string();

        let first = handler
            .render(&skills_input(temp.path()))
            .await
            .expect("expected catalog text");
        assert!(first.contains("Docker ops"), "got: {first}");

        // Rewrite the same file with a longer description — len
        // changes; dir mtime does not.
        make_skill(&skills_dir, "docker", "Docker ops, compose, and swarm");

        let second = handler
            .render(&skills_input(temp.path()))
            .await
            .expect("expected updated catalog text");
        assert!(
            second.contains("Docker ops, compose, and swarm"),
            "got: {second}"
        );
    }

    #[test]
    fn scan_truncates_catalog_at_byte_cap() {
        let temp = TempDir::new().unwrap();
        let skills_dir = temp.path().join("skills");
        std::fs::create_dir(&skills_dir).unwrap();
        // ~200 skills × ~90 bytes each ≈ 18 KB of catalog — well over
        // the 8 KB cap.
        for i in 0..200 {
            make_skill(
                &skills_dir,
                &format!("skill-{i:03}"),
                "A description long enough to fill the catalog quickly",
            );
        }

        let ws = temp.path().to_string_lossy().to_string();
        let out = render_lines(&scan_skills_dir(&skills_dir), &ws);

        assert!(out.len() <= SKILLS_CATALOG_MAX_BYTES, "len: {}", out.len());
        assert!(
            out.contains("(more skills in "),
            "expected truncation notice, got: {out}"
        );
        assert!(out.contains("list the directory to see all"), "got: {out}");
        // Whole-line truncation: the first (sorted) skill survived.
        assert!(out.contains("- skill-000:"), "got: {out}");
    }

    #[test]
    fn scan_under_cap_has_no_truncation_notice() {
        let temp = TempDir::new().unwrap();
        let skills_dir = temp.path().join("skills");
        std::fs::create_dir(&skills_dir).unwrap();
        make_skill(&skills_dir, "docker", "Docker ops");

        let ws = temp.path().to_string_lossy().to_string();
        let out = render_lines(&scan_skills_dir(&skills_dir), &ws);

        assert_eq!(out, "- docker: Docker ops (skills/docker/SKILL.md)");
    }

    #[tokio::test]
    async fn catalog_cache_is_scoped_even_when_file_metadata_matches() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        make_skill(&a.path().join("skills"), "same", "Owner A");
        make_skill(&b.path().join("skills"), "same", "Owner B");
        let stamp = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_234_567);
        for root in [a.path(), b.path()] {
            std::fs::OpenOptions::new()
                .write(true)
                .open(root.join("skills/same/SKILL.md"))
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(stamp))
                .unwrap();
        }
        let handler = WorkspaceSkillsPromptHandler::new();
        assert!(handler
            .render(&skills_input(a.path()))
            .await
            .unwrap()
            .contains("Owner A"));
        let result = handler.render(&skills_input(b.path())).await.unwrap();
        assert!(result.contains("Owner B"), "principal B received: {result}");
    }

    fn owner_input(workspace: &Path) -> crate::tools::prompt_sections::PromptSectionInput {
        crate::tools::prompt_sections::PromptSectionInput {
            acting_for: Some(peko_tools_core::ActingFor::runtime()),
            ..skills_input(workspace)
        }
    }

    #[tokio::test]
    async fn owner_runs_see_bundled_skills_even_without_a_skills_dir() {
        let temp = TempDir::new().unwrap();
        let text = WorkspaceSkillsPromptHandler::new()
            .render(&owner_input(temp.path()))
            .await
            .expect("bundled skills listed");
        assert!(
            text.starts_with("- coding-agents: ") && text.ends_with(" (ships with the runtime)"),
            "got: {text}"
        );
    }

    #[tokio::test]
    async fn owner_only_bundled_skills_are_hidden_from_other_runs() {
        let temp = TempDir::new().unwrap();
        make_skill(&temp.path().join("skills"), "docker", "Docker ops");
        let text = WorkspaceSkillsPromptHandler::new()
            .render(&skills_input(temp.path()))
            .await
            .unwrap();
        assert_eq!(text, "- docker: Docker ops (skills/docker/SKILL.md)");
    }

    #[tokio::test]
    async fn a_workspace_skill_replaces_the_bundled_line() {
        let temp = TempDir::new().unwrap();
        make_skill(&temp.path().join("skills"), "coding-agents", "Owner's way");
        make_skill(&temp.path().join("skills"), "docker", "Docker ops");
        let text = WorkspaceSkillsPromptHandler::new()
            .render(&owner_input(temp.path()))
            .await
            .unwrap();
        assert_eq!(
            text,
            "- coding-agents: Owner's way (skills/coding-agents/SKILL.md)\n- docker: Docker ops (skills/docker/SKILL.md)"
        );
    }
}
