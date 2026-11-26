//! `Skill` tool — invoke a SKILL.md body with argument substitution.
//!
//! Provides `Skill` so the agent can invoke a SKILL.md body on-demand
//! with argument substitution. Workspace presence determines availability;
//! every principal can invoke its installed skills. Locations resolve through the
//! injected [`SkillRuntime`] port (production uses the workspace-scanning
//! `WorkspaceSkillRuntime` from `crate::extensions::skill::reader`).
//!
//! Argument substitution matches Claude Code's skill syntax:
//! - `$ARGUMENTS` — the full args array joined with single spaces
//! - `$0`, `$1`, … `$N` — positional, 0-indexed
//! - `$name` — declared in the SKILL.md frontmatter `arguments:` list;
//!   names map to positions in order. A `$name` whose name is not in the
//!   list is left unsubstituted (Claude-compatible behavior).
//!
//! Escape: `\$` is left literal (so `\$issue` in the body renders as
//! `$issue` rather than substituting).
//!
//! Dynamic context: inline `` !`command` `` and fenced `` ```! `` blocks
//! in the body are resolved by [`body::preprocess_dynamic_context`]
//! before argument substitution, so skills can author live-state-aware
//! bodies (git status, env vars, process lists, …) without forcing the
//! LLM to call `Bash` first. See `body` for the shell runner,
//! the glob allowlist, and the failure-handling rules.

use std::path::PathBuf;

use async_trait::async_trait;
use peko_tools_core::{Tool, ToolContext};
use serde_json::json;

use crate::tools::builtin::skill::body::preprocess_dynamic_context;
use crate::tools::builtin::skill::frontmatter::{
    parse_yaml_frontmatter_typed, PageAudience, SkillFrontmatter,
};
use crate::tools::builtin::skill::{
    append_skill_text, SharedSkillRuntime, SkillRuntime, MAX_SKILL_BODY_BYTES,
};

/// Error variant returned when a skill's SKILL.md cannot be read or parsed.
const SKILL_UNREADABLE: &str = "skill_unreadable";

/// What a run that is not the owner's gets for an `owner` page (ADR-127
/// D3): the page is its owner's to act on, and how to raise it.
pub const OWNER_PAGE_ANSWER: &str = "This page is for your owner's own work: what it covers is \
done by your owner, or by you when you work for them, not in this conversation.
If the person you are talking with needs it, say plainly that you cannot do it here and offer \
to raise it with your owner.
How to raise it: Skill with name \"peko\" and page \"standing\".";

/// Legacy escape marker retained for source compatibility. The renderer now
/// scans escapes directly and never inserts this sentinel.
pub const ESCAPE_SENTINEL: &str = "\x00ESCAPED_DOLLAR\x00";

/// Tool for invoking a SKILL.md body with argument substitution.
///
/// The `Skill` tool resolves skill bodies via the injected
/// [`SharedSkillRuntime`] port. Production wiring uses the
/// workspace-scanning `WorkspaceSkillRuntime`
/// (`crate::extensions::skill::reader`), which resolves skills from
/// the principal's workspace `skills/` directory. Tests use the same
/// runtime pointed at a tempdir.
pub struct SkillTool {
    runtime: SharedSkillRuntime,
}

impl SkillTool {
    /// Build a `SkillTool` bound to the given runtime port.
    #[must_use]
    pub fn new(runtime: SharedSkillRuntime) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        "Skill"
    }

    fn description(&self) -> String {
        r#"Run a skill: returns `{name, dir, body}`, its SKILL.md body with dynamic context resolved and arguments substituted. `name` is the skill's directory name; `args` are positional strings. Files the body mentions are relative to `dir` (null for a skill that ships with the runtime); its commands run in the workspace. An unknown name lists the available skills; an unreadable or invalid skill file is an error.

A skill may have pages: the body then ends with the pages for you, and `pages` lists them. `page` returns `{name, page, body}`, that page's text (plain, no arguments). A page for your owner's runs read from another run returns a short answer instead.

Asked to learn a skill, or to do something the same way every time: write `skills/<name>/SKILL.md` in your workspace (frontmatter `name` and `description`, which says when to use it, then the instructions) and say you did. A memory note is not a skill.

Writing a SKILL.md: `$ARGUMENTS` is all args joined by spaces; `$0`, `$1`, … are positional; `$name` maps by position to the frontmatter `arguments:` list (undeclared names stay as written; `\$` keeps a placeholder literal). `!` plus a backticked command, at a line start or after whitespace, or a fenced block opened with ```!, is replaced by the command's stdout in one pass; a failure appends `stderr: …`, and a command times out after 5000 ms. Frontmatter `allowed-tools: ["git *", …]` limits commands by globs anchored to the whole command (empty: all); `shell: bash` is the only shell."#
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name (must match a discovered SKILL.md directory name)"
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Positional arguments. Maps to $0..$N and $ARGUMENTS in the body."
                },
                "page": {
                    "type": "string",
                    "description": "One of the skill's pages, by name (the skill's body lists them)"
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        // Backward-compatible test entrypoint: run with an empty ToolContext.
        // Production always routes through execute_with_context, which carries
        // the principal_id needed to resolve per-principal skill state.
        let ctx = peko_tools_core::ToolContext::default_for_tool("Skill");
        self.execute_with_context(params, &ctx).await
    }

    async fn execute_with_context(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<serde_json::Value> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing required parameter 'name'"))?
            .to_string();

        let args: Vec<String> = params
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // ADR-066 P2: presence = executability — a skill resolvable in
        // the workspace is invocable; there is no grant check.
        let Some(entry) = self.runtime.resolve_skill(&name) else {
            let available = self.runtime.list_skills();
            anyhow::bail!(
                "unknown skill '{name}' — available skills: {}",
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            );
        };

        // ADR-127 D3: pages are listed for the run's standing.
        let owner = ctx
            .acting_for
            .as_ref()
            .is_some_and(peko_tools_core::ActingFor::is_owner);
        if let Some(page) = params.get("page").and_then(|v| v.as_str()) {
            return read_page(self.runtime.as_ref(), &name, page, owner);
        }

        let content = entry.read().map_err(|e| {
            anyhow::anyhow!("{SKILL_UNREADABLE}: failed to read SKILL.md for skill {name}: {e}")
        })?;

        let (frontmatter, body): (SkillFrontmatter, String) =
            parse_yaml_frontmatter_typed(&content).map_err(|e| {
                anyhow::anyhow!(
                "{SKILL_UNREADABLE}: failed to parse frontmatter in SKILL.md for skill {name}: {e}"
            )
            })?;

        // `parse_yaml_frontmatter` includes the newline that follows the
        // closing `---` in the body. Trim it so the rendered output
        // matches what the SKILL.md author wrote.
        let body = body.strip_prefix('\n').unwrap_or(&body);

        // Resolve inline `` !`cmd` `` and fenced `` ```! `` blocks against
        // the principal's workspace before argument substitution runs.
        let workspace = ctx
            .workspace
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let mut abort = ctx.abort_signal();
        anyhow::ensure!(!ctx.is_aborted(), "Skill invocation aborted");
        let body = tokio::select! {
            result = preprocess_dynamic_context(body, &frontmatter, &workspace) => result?,
            () = async {
                if abort.wait_for(|aborted| *aborted).await.is_err() {
                    std::future::pending::<()>().await;
                }
            } => anyhow::bail!("Skill invocation aborted"),
        };

        let mut rendered = substitute_args(&body, &frontmatter.arguments, &args)?;
        let pages = visible_pages(self.runtime.as_ref(), &name, owner);
        if !pages.is_empty() {
            append_skill_text(&mut rendered, &page_list(&name, &pages))?;
        }

        // Files a skill mentions (scripts, references) live beside its
        // SKILL.md; the caller needs the directory to open them.
        // A bundled skill has no directory: `dir` is null.
        let dir = entry.dir().map(|dir| dir.display().to_string());
        let mut out = json!({
            "name": frontmatter.name,
            "dir": dir,
            "body": rendered,
        });
        if !pages.is_empty() {
            out["pages"] = json!(pages.iter().map(|(page, _)| page).collect::<Vec<_>>());
        }
        Ok(out)
    }
}

/// The pages of `skill` a run sees, with their descriptions: all of
/// them for an owner run, the `all` ones otherwise. A page whose
/// frontmatter does not parse is left out.
fn visible_pages(runtime: &dyn SkillRuntime, skill: &str, owner: bool) -> Vec<(String, String)> {
    runtime
        .list_pages(skill)
        .into_iter()
        .filter_map(|name| {
            let (frontmatter, _) = runtime.resolve_page(skill, &name)?.parse().ok()?;
            frontmatter
                .audience
                .shown_to(owner)
                .then(|| (name, frontmatter.description.trim().to_string()))
        })
        .collect()
}

/// The page list a skill's body ends with.
fn page_list(skill: &str, pages: &[(String, String)]) -> String {
    let mut text = format!(
        "\n\n## Pages\n\nOpen one with Skill, name \"{skill}\" and page set to its name:\n"
    );
    for (page, description) in pages {
        text.push_str(&format!("- `{page}`: {description}\n"));
    }
    text
}

/// `Skill` with `page` (ADR-127 D2, D3).
fn read_page(
    runtime: &dyn SkillRuntime,
    skill: &str,
    page: &str,
    owner: bool,
) -> anyhow::Result<serde_json::Value> {
    let Some(found) = runtime.resolve_page(skill, page) else {
        let pages = visible_pages(runtime, skill, owner);
        if pages.is_empty() {
            anyhow::bail!("skill '{skill}' has no pages");
        }
        let names: Vec<&str> = pages.iter().map(|(name, _)| name.as_str()).collect();
        anyhow::bail!(
            "skill '{skill}' has no page '{page}' — its pages: {}",
            names.join(", ")
        );
    };
    let (frontmatter, body) = found
        .parse()
        .map_err(|e| anyhow::anyhow!("{SKILL_UNREADABLE}: page {page} of skill {skill}: {e:#}"))?;
    if !frontmatter.audience.shown_to(owner) {
        return Ok(json!({
            "name": skill,
            "page": page,
            "audience": PageAudience::Owner.as_str(),
            "body": OWNER_PAGE_ANSWER,
        }));
    }
    Ok(json!({
        "name": skill,
        "page": page,
        "audience": frontmatter.audience.as_str(),
        "body": body,
    }))
}

/// Substitute placeholders from the source once. Replacement values are
/// literal, undeclared names and missing positions survive unchanged, and
/// work is bounded by source length rather than the numeric value of `$N`.
fn substitute_args(body: &str, named: &[String], args: &[String]) -> anyhow::Result<String> {
    let mut out = String::with_capacity(body.len().min(MAX_SKILL_BODY_BYTES));
    let mut cursor = 0;
    while cursor < body.len() {
        let rest = &body[cursor..];
        if rest.starts_with("\\$") {
            append_skill_text(&mut out, "$")?;
            cursor += 2;
            continue;
        }
        if rest.starts_with('$') {
            let suffix = &rest[1..];
            let length = if suffix.starts_with(|c: char| c.is_ascii_digit()) {
                suffix.bytes().take_while(u8::is_ascii_digit).count()
            } else {
                suffix
                    .char_indices()
                    .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
                    .map(|(i, c)| i + c.len_utf8())
                    .last()
                    .unwrap_or(0)
            };
            if length > 0 {
                let token = &suffix[..length];
                if token == "ARGUMENTS" {
                    for (i, value) in args.iter().enumerate() {
                        if i > 0 {
                            append_skill_text(&mut out, " ")?;
                        }
                        append_skill_text(&mut out, value)?;
                    }
                } else {
                    let index = if token.starts_with(|c: char| c.is_ascii_digit()) {
                        token.parse::<usize>().ok()
                    } else {
                        named.iter().position(|name| name == token)
                    };
                    match index.and_then(|i| args.get(i)) {
                        Some(value) => append_skill_text(&mut out, value)?,
                        None => append_skill_text(&mut out, &rest[..=length])?,
                    }
                }
                cursor += length + 1;
                continue;
            }
        }
        let ch = rest.chars().next().expect("cursor is before end of body");
        append_skill_text(&mut out, &rest[..ch.len_utf8()])?;
        cursor += ch.len_utf8();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn render_args(body: &str, named: &[String], args: &[String]) -> String {
        super::substitute_args(body, named, args).unwrap()
    }

    #[test]
    fn argument_expansion_is_bounded_before_allocating_the_result() {
        let value = "a".repeat(MAX_SKILL_BODY_BYTES / 2 + 1);
        let error = super::substitute_args("$0$0", &[], &[value]).unwrap_err();
        assert!(error.to_string().contains("rendered skill body exceeds"));
    }

    #[test]
    fn replacement_values_are_literal_even_when_they_look_like_placeholders() {
        assert_eq!(
            render_args(
                "$first $0 $1 $ARGUMENTS",
                &["first".into()],
                &["$1".into(), "$ARGUMENTS".into()]
            ),
            "$1 $1 $ARGUMENTS $1 $ARGUMENTS"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_skill_kills_dynamic_context_process_tree() {
        use crate::tools::builtin::process::tests::{
            long_lived_child, still_running_after_grace, wait_for_pid_file,
        };
        let ws = tempfile::tempdir().unwrap();
        let dir = ws.path().join("skills/slow");
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = ws.path().join("child.pid");
        let body = format!(
            "---\nname: slow\ndescription: slow\n---\n```!\n{}\n```",
            long_lived_child(&pid_file)
        );
        std::fs::write(dir.join("SKILL.md"), body).unwrap();
        let tool = SkillTool::new(Arc::new(WorkspaceSkillRuntime::new(
            ws.path().join("skills"),
        )));
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let context = ToolContext::default_for_tool("Skill")
            .with_workspace(ws.path().to_str().unwrap())
            .with_abort_signal(receiver);
        let run = tokio::spawn(async move {
            tool.execute_with_context(json!({"name":"slow"}), &context)
                .await
        });
        wait_for_pid_file(&pid_file).await;
        sender.send(true).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), run)
            .await
            .unwrap()
            .unwrap();
        assert!(result.unwrap_err().to_string().contains("aborted"));
        assert!(!still_running_after_grace(&pid_file).await);
    }

    use crate::extensions::skill::WorkspaceSkillRuntime;
    use peko_tools_core::ToolContext;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::TempDir;

    #[test]
    fn substitute_no_args_no_placeholders() {
        assert_eq!(render_args("hello world", &[], &[]), "hello world");
    }

    #[test]
    fn substitute_arguments_full() {
        assert_eq!(
            render_args("Full: $ARGUMENTS", &[], &["a".into(), "b".into()]),
            "Full: a b"
        );
    }

    #[test]
    fn substitute_positional() {
        assert_eq!(
            render_args("Issue $0 on $1", &[], &["42".into(), "main".into()]),
            "Issue 42 on main"
        );
    }

    #[test]
    fn substitute_positional_high_to_low_avoids_greedy_match() {
        // $10 must not become "<value of $1>0". With args of length 11,
        // $10 should map to args[10] and $1 to args[1].
        let args: Vec<String> = (0..11).map(|i| format!("v{i}")).collect();
        let rendered = render_args("$10 then $1", &[], &args);
        assert_eq!(rendered, "v10 then v1");
        // When the user only passes 2 args, $10 should be left literal
        // (no positional at index 10) and $1 should map to args[1].
        let rendered = render_args("$10 then $1", &[], &["a".into(), "b".into()]);
        assert_eq!(rendered, "$10 then b");
    }

    #[test]
    fn substitute_named_by_position() {
        // `arguments: [issue, branch]` → $issue = args[0], $branch = args[1].
        let named = vec!["issue".to_string(), "branch".to_string()];
        let rendered = render_args(
            "Open $issue on $branch.",
            &named,
            &["42".into(), "main".into()],
        );
        assert_eq!(rendered, "Open 42 on main.");
    }

    #[test]
    fn substitute_undeclared_name_left_literal() {
        // `$issue` is not in the named list → left unsubstituted.
        // The body has only $issue and $0; named=["issue"] but with one
        // arg passed, so $issue maps to args[0] = "42".
        let named = vec!["issue".to_string()];
        let rendered = render_args("Issue $issue and again $issue", &named, &["42".into()]);
        assert_eq!(rendered, "Issue 42 and again 42");
    }

    #[test]
    fn substitute_named_position_out_of_range_leaves_literal() {
        // `arguments: [issue, branch]` says there are two named slots,
        // but the caller passes only one arg. `$branch` (named[1]) maps
        // to args[1] which doesn't exist → left literal. `$issue`
        // (named[0]) maps to args[0] = "42".
        let named = vec!["issue".to_string(), "branch".to_string()];
        let rendered = render_args("Open $issue on $branch.", &named, &["42".into()]);
        assert_eq!(rendered, "Open 42 on $branch.");
    }

    #[test]
    fn substitute_escape_preserves_literal_dollar() {
        // `\$1` is the literal string "$1" — no substitution.
        // `$0` does substitute because args has length 1.
        let rendered = render_args("Cost \\$1.00 or $0", &[], &["free".into()]);
        assert_eq!(rendered, "Cost $1.00 or free");
    }

    #[test]
    fn substitute_escape_preserves_named_and_arguments() {
        let named = vec!["price".to_string()];
        let rendered = render_args(
            "Cost \\$price, actual $price, full: $ARGUMENTS",
            &named,
            &["9.99".into()],
        );
        assert_eq!(rendered, "Cost $price, actual 9.99, full: 9.99");
    }

    #[test]
    fn substitute_missing_arg_leaves_named_literal() {
        // Only one arg provided but two named slots — the second
        // ($branch, named[1]) has no args[1] to map to, so it is
        // left literal (Claude-compatible).
        let named = vec!["issue".to_string(), "branch".to_string()];
        let rendered = render_args("Open $issue on $branch.", &named, &["42".into()]);
        assert_eq!(rendered, "Open 42 on $branch.");
    }

    // ---- SkillTool::execute tests (filesystem-backed) ----

    static TEST_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn next_test_id() -> String {
        format!(
            "prin_test_{}",
            TEST_ID_COUNTER.fetch_add(1, Ordering::SeqCst)
        )
    }

    fn test_ctx(workspace: &std::path::Path) -> ToolContext {
        ToolContext::for_hook_run("hook_run", "hook", "Skill")
            .with_workspace(workspace.to_string_lossy().to_string())
    }

    /// Build a `SkillTool` backed by the production workspace runtime
    /// rooted at `skills_root` — tests write `<skills_root>/<name>/SKILL.md`
    /// fixtures and resolve them through the same code path production
    /// uses (`WorkspaceSkillRuntime`).
    fn tool_with_workspace(skills_root: &std::path::Path) -> SkillTool {
        SkillTool::new(std::sync::Arc::new(WorkspaceSkillRuntime::new(
            skills_root.to_path_buf(),
        )))
    }

    #[tokio::test]
    async fn bundled_skill_runs_with_no_dir_and_unknown_lists_it() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SkillTool::new(std::sync::Arc::new(WorkspaceSkillRuntime::with_bundled(
            tmp.path().join("skills"),
        )));
        let out = tool
            .execute(json!({"name": "coding-agents"}))
            .await
            .unwrap();
        assert_eq!(out["name"], "coding-agents");
        assert!(out["dir"].is_null(), "got {out}");
        let body = out["body"].as_str().unwrap();
        assert!(
            !body.starts_with("---") && body.contains("claude -p"),
            "got {body}"
        );
        let err = tool.execute(json!({"name": "nope"})).await.unwrap_err();
        assert!(err.to_string().contains("coding-agents"), "got {err}");
    }

    fn stamped(trust: peko_tools_core::Trust) -> ToolContext {
        let mut stamp = peko_tools_core::ActingFor::runtime();
        stamp.trust = trust;
        if trust != peko_tools_core::Trust::Owner {
            stamp.actor = "visitor:local:sam".into();
            stamp.compartment = peko_tools_core::Compartment::Conversation {
                channel: "chan_x".into(),
            };
        }
        ToolContext::default_for_tool("Skill").with_acting_for(stamp)
    }

    fn manual_tool(skills: &std::path::Path) -> SkillTool {
        SkillTool::new(Arc::new(WorkspaceSkillRuntime::with_bundled(
            skills.to_path_buf(),
        )))
    }

    /// ADR-127 D1, D3: the map lists the pages for the run's standing,
    /// and stays within 2 KB for the owner, who sees them all.
    #[tokio::test]
    async fn the_manual_map_lists_the_pages_for_the_runs_standing() {
        use crate::tools::builtin::skill::MAX_SKILL_MAP_BYTES;
        use peko_tools_core::Trust;
        let tmp = TempDir::new().unwrap();
        let tool = manual_tool(&tmp.path().join("skills"));
        let owner = tool
            .execute_with_context(json!({"name": "peko"}), &stamped(Trust::Owner))
            .await
            .unwrap();
        let body = owner["body"].as_str().unwrap();
        assert!(
            body.len() <= MAX_SKILL_MAP_BYTES,
            "{} bytes:\n{body}",
            body.len()
        );
        assert_eq!(owner["dir"], serde_json::Value::Null);
        assert_eq!(
            owner["pages"],
            json!([
                "changing-yourself",
                "how-you-work",
                "improving-peko",
                "operating-peko",
                "seeds",
                "standing"
            ])
        );
        for page in ["`changing-yourself`", "`seeds`", "`standing`"] {
            assert!(body.contains(page), "{page} missing:\n{body}");
        }

        let visitor = tool
            .execute_with_context(json!({"name": "peko"}), &stamped(Trust::Untrusted))
            .await
            .unwrap();
        assert_eq!(visitor["pages"], json!(["how-you-work", "standing"]));
        let body = visitor["body"].as_str().unwrap();
        for page in [
            "changing-yourself",
            "operating-peko",
            "improving-peko",
            "`seeds`",
        ] {
            assert!(!body.contains(page), "{page} shown to a visitor:\n{body}");
        }
        // A run with no stamp is not the owner's.
        let bare = tool.execute(json!({"name": "peko"})).await.unwrap();
        assert_eq!(bare["pages"], visitor["pages"]);
    }

    /// ADR-127 D3: another run asking for an owner page gets three lines
    /// pointing at `standing`; an `all` page reads the same for everyone.
    #[tokio::test]
    async fn an_owner_page_asked_by_another_run_gets_the_short_answer() {
        use peko_tools_core::Trust;
        let tmp = TempDir::new().unwrap();
        let tool = manual_tool(&tmp.path().join("skills"));
        let ask = |trust| {
            let tool = &tool;
            async move {
                tool.execute_with_context(
                    json!({"name": "peko", "page": "operating-peko"}),
                    &stamped(trust),
                )
                .await
                .unwrap()
            }
        };
        let visitor = ask(Trust::Untrusted).await;
        assert_eq!(visitor["body"], OWNER_PAGE_ANSWER);
        assert_eq!(visitor["audience"], "owner");
        assert_eq!(OWNER_PAGE_ANSWER.lines().count(), 3);
        assert!(OWNER_PAGE_ANSWER.contains("\"standing\""));
        let owner = ask(Trust::Owner).await;
        assert!(owner["body"].as_str().unwrap().contains("peko create"));
        assert!(!owner["body"].as_str().unwrap().starts_with("---"));

        let standing = tool
            .execute_with_context(
                json!({"name": "peko", "page": "standing"}),
                &stamped(Trust::Untrusted),
            )
            .await
            .unwrap();
        assert_eq!(standing["audience"], "all");
        assert!(standing["body"]
            .as_str()
            .unwrap()
            .contains("Putting something to your owner"));
    }

    #[tokio::test]
    async fn an_unknown_page_lists_the_pages_for_the_run() {
        use peko_tools_core::Trust;
        let tmp = TempDir::new().unwrap();
        let tool = manual_tool(&tmp.path().join("skills"));
        let err = tool
            .execute_with_context(
                json!({"name": "peko", "page": "../SKILL"}),
                &stamped(Trust::Untrusted),
            )
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "skill 'peko' has no page '../SKILL' — its pages: how-you-work, standing"
        );
        let err = tool
            .execute(json!({"name": "triage", "page": "x"}))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "skill 'triage' has no pages");
    }

    /// ADR-127 D2: a page file in the workspace adds a page to the
    /// bundled skill or replaces the one of its name; a workspace skill
    /// of its own has only its own pages; a page past 8 KB is refused.
    #[tokio::test]
    async fn workspace_pages_add_to_or_replace_bundled_ones() {
        use peko_tools_core::Trust;
        let tmp = TempDir::new().unwrap();
        let skills = tmp.path().join("skills");
        let pages = skills.join("peko/pages");
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::write(
            pages.join("standing.md"),
            "---\ndescription: Ours\naudience: all\n---\nOur own standing.\n",
        )
        .unwrap();
        std::fs::write(
            pages.join("house-rules.md"),
            "---\ndescription: House rules\n---\nWipe your paws.\n",
        )
        .unwrap();
        let tool = manual_tool(&skills);
        let visitor = stamped(Trust::Untrusted);
        let map = tool
            .execute_with_context(json!({"name": "peko"}), &visitor)
            .await
            .unwrap();
        assert_eq!(
            map["pages"],
            json!(["house-rules", "how-you-work", "standing"])
        );
        let page = tool
            .execute_with_context(json!({"name": "peko", "page": "standing"}), &visitor)
            .await
            .unwrap();
        assert_eq!(page["body"], "Our own standing.");
        let page = tool
            .execute_with_context(json!({"name": "peko", "page": "house-rules"}), &visitor)
            .await
            .unwrap();
        assert_eq!(page["audience"], "all", "the default audience");

        std::fs::write(
            pages.join("huge.md"),
            format!("---\ndescription: Huge\n---\n{}", "x".repeat(9000)),
        )
        .unwrap();
        let err = tool
            .execute_with_context(json!({"name": "peko", "page": "huge"}), &visitor)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exceeds 8 KB"), "{err}");

        // A workspace skill with its own SKILL.md replaces the bundled
        // one entirely: only its pages.
        std::fs::write(
            skills.join("peko/SKILL.md"),
            "---\nname: peko\ndescription: Ours\n---\nOur manual.\n",
        )
        .unwrap();
        let ours = tool
            .execute_with_context(json!({"name": "peko"}), &stamped(Trust::Owner))
            .await
            .unwrap();
        assert_eq!(ours["pages"], json!(["house-rules", "standing"]));
        assert!(tool
            .execute_with_context(json!({"name": "peko", "page": "seeds"}), &visitor)
            .await
            .is_err());
    }

    fn write_skill(dir: &std::path::Path, name: &str, body: &str, args: &[&str]) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let args_yaml = if args.is_empty() {
            String::new()
        } else {
            let quoted: Vec<String> = args.iter().map(|a| format!("\"{a}\"")).collect();
            format!("arguments: [{}]\n", quoted.join(", "))
        };
        let content =
            format!("---\nname: {name}\ndescription: A test skill\n{args_yaml}---\n\n{body}\n");
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    fn write_skill_with_frontmatter(
        dir: &std::path::Path,
        name: &str,
        frontmatter_extra: &str,
        body: &str,
    ) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let content = format!(
            "---\nname: {name}\ndescription: A test skill\n{frontmatter_extra}---\n\n{body}\n"
        );
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    #[tokio::test]
    async fn execute_returns_substituted_body() {
        let _id = next_test_id();
        let tmp = TempDir::new().unwrap();
        write_skill(
            tmp.path(),
            "fix",
            "Open issue $issue on branch $branch.\nFull: $ARGUMENTS",
            &["issue", "branch"],
        );
        let tool = tool_with_workspace(tmp.path());
        let result = tool
            .execute_with_context(
                json!({
                    "name": "fix",
                    "args": ["42", "main"],
                }),
                &test_ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert_eq!(result["name"], "fix");
        assert_eq!(
            result["body"],
            "Open issue 42 on branch main.\nFull: 42 main"
        );
    }

    #[tokio::test]
    async fn execute_runs_skill_present_in_workspace() {
        // ADR-066 P2: presence = executability — no grant context needed.
        let _id = next_test_id();
        let tmp = TempDir::new().unwrap();
        write_skill(tmp.path(), "fix", "body", &[]);
        let tool = tool_with_workspace(tmp.path());
        let result = tool
            .execute_with_context(json!({ "name": "fix" }), &test_ctx(tmp.path()))
            .await
            .unwrap();
        assert_eq!(result["name"], "fix");
        assert_eq!(result["body"], "body");
        // The directory holding SKILL.md, so files the body names resolve.
        let dir = std::path::PathBuf::from(result["dir"].as_str().unwrap());
        assert_eq!(
            dir.canonicalize().unwrap(),
            tmp.path().join("fix").canonicalize().unwrap()
        );
    }

    #[tokio::test]
    async fn execute_rejects_unknown_skill() {
        let _id = next_test_id();
        let tmp = TempDir::new().unwrap();
        let tool = tool_with_workspace(tmp.path());
        let result = tool
            .execute_with_context(json!({ "name": "docker" }), &test_ctx(tmp.path()))
            .await
            .unwrap_err();
        assert_eq!(
            result.to_string(),
            "unknown skill 'docker' — available skills: none"
        );
    }

    #[tokio::test]
    async fn execute_rejects_missing_name_param() {
        let _id = next_test_id();
        let tmp = TempDir::new().unwrap();
        let tool = tool_with_workspace(tmp.path());
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(err
            .to_string()
            .contains("missing required parameter 'name'"));
    }

    #[tokio::test]
    async fn execute_handles_missing_arguments_frontmatter() {
        let _id = next_test_id();
        let tmp = TempDir::new().unwrap();
        // No `arguments:` frontmatter; body still uses $issue.
        write_skill(tmp.path(), "minimal", "Issue $issue", &[]);
        let tool = tool_with_workspace(tmp.path());
        let result = tool
            .execute_with_context(
                json!({
                    "name": "minimal",
                    "args": ["42"],
                }),
                &test_ctx(tmp.path()),
            )
            .await
            .unwrap();
        // $issue is undeclared → left literal.
        assert_eq!(result["body"], "Issue $issue");
    }

    #[tokio::test]
    async fn execute_is_case_insensitive_on_allowlist() {
        let _id = next_test_id();
        let tmp = TempDir::new().unwrap();
        write_skill(tmp.path(), "Docker", "Docker body", &[]);
        let tool = tool_with_workspace(tmp.path());
        let result = tool
            .execute_with_context(json!({ "name": "Docker" }), &test_ctx(tmp.path()))
            .await
            .unwrap();
        assert_eq!(result["name"], "Docker");
        assert_eq!(result["body"], "Docker body");
    }

    #[tokio::test]
    async fn execute_runs_dynamic_context_inline_form() {
        let _id = next_test_id();
        let tmp = TempDir::new().unwrap();
        write_skill(tmp.path(), "live", "CWD: !`pwd`", &[]);
        let tool = tool_with_workspace(tmp.path());
        let result = tool
            .execute_with_context(json!({ "name": "live" }), &test_ctx(tmp.path()))
            .await
            .unwrap();
        // `pwd` resolves to the workspace dir; the exact path varies by
        // platform, but the body should no longer contain the
        // `!`pwd`` placeholder.
        let body = result["body"].as_str().unwrap();
        assert!(
            !body.contains("!`pwd`"),
            "placeholder should be gone, got: {body}"
        );
        assert!(body.starts_with("CWD: "));
        assert!(body.len() > "CWD: ".len());
    }

    #[tokio::test]
    async fn execute_runs_dynamic_context_fenced_form() {
        let _id = next_test_id();
        let tmp = TempDir::new().unwrap();
        // Opener on its own line per the plan's exact-match rule.
        let body_str = "Intro\n```!\necho alpha\n```\nOutro";
        write_skill(tmp.path(), "fence", body_str, &[]);
        let tool = tool_with_workspace(tmp.path());
        let result = tool
            .execute_with_context(json!({ "name": "fence" }), &test_ctx(tmp.path()))
            .await
            .unwrap();
        let body = result["body"].as_str().unwrap();
        assert!(
            body.contains("alpha"),
            "fenced output should contain echo'd text, got: {body}"
        );
        assert!(!body.contains("```"), "fence markers should be gone");
    }

    #[tokio::test]
    async fn execute_blocks_command_not_in_allowed_tools() {
        let _id = next_test_id();
        let tmp = TempDir::new().unwrap();
        write_skill_with_frontmatter(
            tmp.path(),
            "guarded_block",
            "allowed-tools:\n  - \"echo *\"\n",
            "Got: !`ls /`",
        );
        let tool = tool_with_workspace(tmp.path());
        let result = tool
            .execute_with_context(json!({ "name": "guarded_block" }), &test_ctx(tmp.path()))
            .await
            .unwrap();
        let body = result["body"].as_str().unwrap();
        // `ls /` is not in the allowlist → placeholder is left literal
        // and the blocked marker is prepended.
        assert!(body.contains("[shell blocked: command not in allowed-tools]"));
        assert!(body.contains("!`ls /`"));
    }

    #[tokio::test]
    async fn execute_allows_command_in_allowed_tools_glob() {
        let _id = next_test_id();
        let tmp = TempDir::new().unwrap();
        write_skill_with_frontmatter(
            tmp.path(),
            "guarded_allow",
            "allowed-tools:\n  - \"echo *\"\n",
            "Got: !`echo hello`",
        );
        let tool = tool_with_workspace(tmp.path());
        let result = tool
            .execute_with_context(json!({ "name": "guarded_allow" }), &test_ctx(tmp.path()))
            .await
            .unwrap();
        assert_eq!(result["body"], "Got: hello\n");
    }

    #[tokio::test]
    async fn execute_runs_skill_without_principal_context() {
        // ADR-066 P2: no capability gate — a skill present in the
        // workspace runs even when the caller context carries no
        // principal identity at all.
        let _id = next_test_id();
        let tmp = TempDir::new().unwrap();
        write_skill(tmp.path(), "fix", "body", &[]);
        let tool = tool_with_workspace(tmp.path());
        let ctx = ToolContext::for_hook_run("hook_run", "hook", "Skill");
        let result = tool
            .execute_with_context(json!({ "name": "fix" }), &ctx)
            .await
            .unwrap();
        assert_eq!(result["name"], "fix");
        assert_eq!(result["body"], "body");
    }

    #[tokio::test]
    async fn execute_rejects_traversal_name_as_unknown_skill() {
        // `Skill {name: "../secret"}` must not escape the workspace
        // skills dir. The workspace runtime
        // refuses any name that is not a single plain path component,
        // so the tool reports an unknown skill.
        let _id = next_test_id();
        let tmp = TempDir::new().unwrap();
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        // The traversal target lives OUTSIDE the skills dir.
        write_skill(tmp.path(), "secret", "s3cret", &[]);
        let tool = tool_with_workspace(&skills_dir);

        let result = tool
            .execute_with_context(json!({ "name": "../secret" }), &test_ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(
            result.to_string().starts_with("unknown skill '../secret'"),
            "{result}"
        );
    }

    #[test]
    fn declared_names_do_not_replace_longer_undeclared_names() {
        assert_eq!(
            render_args("$issue_id", &["issue".into()], &["42".into()]),
            "$issue_id"
        );
    }
    #[test]
    fn huge_numeric_placeholders_remain_literal_without_index_iteration() {
        assert_eq!(render_args("Budget $100000", &[], &[]), "Budget $100000");
        assert_eq!(
            render_args("$18446744073709551615 $999999999999999999999999", &[], &[]),
            "$18446744073709551615 $999999999999999999999999"
        );
    }
}
