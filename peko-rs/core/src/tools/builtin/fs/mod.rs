//! Filesystem tools
//!
//! Granular filesystem operations for agents:
//! - `Read`: Read file contents
//! - `Write`: Write file contents
//! - `glob`: Find files matching patterns
//! - `grep`: Search file contents
//! - `Edit`: Targeted string replacement in files

pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod write;

pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use write::WriteTool;

/// One entry found by [`walk`].
pub(crate) struct WalkEntry {
    pub path: std::path::PathBuf,
    /// Metadata of the entry itself (symlinks are not followed).
    pub metadata: std::fs::Metadata,
}

/// The entries under `root` in path order, the way an agent expects a
/// code search to see a project: names starting with `.` are skipped unless
/// `include_hidden`, and anything excluded by `.gitignore` / `.ignore` files
/// (or git's exclude and global ignore lists) is skipped unless
/// `include_ignored` — inside a git repository or not. `max_depth: Some(1)`
/// lists only `root`'s children; `root` itself is never returned.
pub(crate) async fn walk(
    root: std::path::PathBuf,
    max_depth: Option<usize>,
    include_hidden: bool,
    include_ignored: bool,
    scope: Option<Vec<std::path::PathBuf>>,
) -> anyhow::Result<Vec<WalkEntry>> {
    use anyhow::Context as _;
    std::fs::read_dir(&root)
        .with_context(|| format!("Failed to read directory: {}", root.display()))?;
    tokio::task::spawn_blocking(move || {
        let respect = !include_ignored;
        let walker = ignore::WalkBuilder::new(&root)
            .hidden(!include_hidden)
            .ignore(respect)
            .git_ignore(respect)
            .git_global(respect)
            .git_exclude(respect)
            .parents(respect)
            .require_git(false)
            .follow_links(false)
            .max_depth(max_depth)
            .sort_by_file_path(|a, b| a.cmp(b))
            // A visitor's search walks only what it may reach and the
            // folders leading there (ADR-113); nothing else is listed or
            // opened.
            .filter_entry({
                // Scope roots are resolved paths (`/private/var/…` on
                // macOS); entries are compared in the same form, below the
                // walk's own resolved start.
                let start = root.clone();
                let resolved_start = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
                move |entry| match &scope {
                    None => true,
                    Some(scope) => {
                        let path = entry.path().strip_prefix(&start).map_or_else(
                            |_| entry.path().to_path_buf(),
                            |rel| resolved_start.join(rel),
                        );
                        in_scope(scope, &path, entry.depth() == 0)
                    }
                }
            })
            .build();
        walker
            .filter_map(Result::ok)
            .filter(|entry| entry.depth() > 0)
            .filter_map(|entry| {
                let metadata = entry.metadata().ok()?;
                Some(WalkEntry {
                    path: entry.into_path(),
                    metadata,
                })
            })
            .collect()
    })
    .await
    .context("directory walk panicked")
}

/// Whether a walk limited to `scope` goes through `path`: it lies in a
/// scope root, or is a folder on the way to one. `is_root` is the walk's
/// own start, always entered.
fn in_scope(scope: &[std::path::PathBuf], path: &std::path::Path, is_root: bool) -> bool {
    is_root
        || scope
            .iter()
            .any(|root| path.starts_with(root) || root.starts_with(path))
}

/// The parameter the dispatcher sets on a visitor's `Glob` / `Grep` to
/// limit the walk (see [`walk`]): resolved paths. Never taken from the
/// model.
pub(crate) const VISITOR_SCOPE_PARAM: &str = "__visitor_scope";

#[cfg(test)]
mod scope_tests {
    use super::*;

    /// A walk limited to `public/` and one conversation's folder lists
    /// those and the folders leading to them, and nothing else.
    #[tokio::test]
    async fn a_scoped_walk_lists_only_what_is_in_scope() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        for (path, body) in [
            ("public/faq.md", "hours"),
            ("kb/MEMORY.md", "secret"),
            ("contacts/dm_a/notes.md", "mine"),
            ("contacts/dm_b/notes.md", "theirs"),
            ("top.md", "x"),
        ] {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
        // Scope roots come resolved, as the dispatcher sends them.
        let resolved = std::fs::canonicalize(root).unwrap();
        let scope = vec![resolved.join("public"), resolved.join("contacts/dm_a")];
        let found = walk(root.to_path_buf(), None, false, false, Some(scope))
            .await
            .unwrap();
        let mut names: Vec<String> = found
            .iter()
            .map(|e| {
                e.path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "contacts",
                "contacts/dm_a",
                "contacts/dm_a/notes.md",
                "public",
                "public/faq.md"
            ]
        );
    }
}
