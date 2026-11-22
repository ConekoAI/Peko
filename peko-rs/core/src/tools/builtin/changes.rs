//! `Changes` — the files recent turns changed, and undoing one (ADR-117).
//!
//! Owner runs only (the trust floor): so "undo what you just did" works
//! when the owner asks in the chat.

use std::sync::Arc;

use async_trait::async_trait;
use peko_tools_core::{Tool, ToolContext};
use serde_json::json;

use crate::tools::changes::{ChangeLog, KEEP_DAYS};

/// Wire name.
pub const CHANGES_TOOL_NAME: &str = "Changes";

/// Turns listed by default, and at most.
const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 50;

/// Lists recent turns' file changes and undoes one.
pub struct ChangesTool {
    log: Arc<ChangeLog>,
}

impl ChangesTool {
    #[must_use]
    pub fn new(log: Arc<ChangeLog>) -> Self {
        Self { log }
    }
}

#[async_trait]
impl Tool for ChangesTool {
    fn name(&self) -> &'static str {
        CHANGES_TOOL_NAME
    }

    fn description(&self) -> String {
        format!(
            r#"The files your recent turns changed, and undoing a turn when your owner asks ("undo that", "put my notes back").

- `list`: recent turns that changed files, newest first; in a conversation, those in this conversation unless `all`. Each names its files (`added` / `modified` / `deleted`), whether it ran shell commands, and whether it was undone. Your current turn is marked `current`.
- `undo` with `turn`: puts each file that turn changed (and the tasks it delegated changed) back as it was, if it is still as the turn left it; a file changed since is left alone and named. Only when your owner asks for it; tell them what was put back and what was not.

Recorded: what `Write` and `Edit` changed anywhere, and what shell commands changed inside your workspace. Not recorded: what shell commands changed outside it. Turns are kept {KEEP_DAYS} days.

## Result
`list`: `{{turns: [{{turn, current, updated_at, files: [{{path, change}}], shell, undone_at?}}]}}`. `undo`: `{{restored: [path], skipped: [{{path, why}}]}}`."#
        )
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "undo"],
                    "description": "`list` recent turns' changes; `undo` one turn."
                },
                "turn": {
                    "type": "string",
                    "description": "undo: the turn, from `list`."
                },
                "all": {
                    "type": "boolean",
                    "description": "list: every conversation, not only this one."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_LIMIT,
                    "description": "list: how many turns (default 5)."
                }
            },
            "required": ["action"]
        })
    }

    fn parallelizable(&self) -> bool {
        // Undo writes files: calls apply in order.
        false
    }

    async fn execute(&self, _params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        Err(anyhow::anyhow!(
            "Changes requires a ToolContext (use execute_with_context)"
        ))
    }

    async fn execute_with_context(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<serde_json::Value> {
        anyhow::ensure!(
            ctx.acting_for().is_owner(),
            "Changes is only for work done for the owner"
        );
        match params
            .get("action")
            .and_then(|a| a.as_str())
            .unwrap_or_default()
        {
            "list" => {
                let all = params.get("all").and_then(serde_json::Value::as_bool) == Some(true);
                let channel = ctx.acting_for().compartment.channel().map(str::to_string);
                let limit = params
                    .get("limit")
                    .and_then(serde_json::Value::as_u64)
                    .map_or(DEFAULT_LIMIT, |l| (l as usize).clamp(1, MAX_LIMIT));
                let log = Arc::clone(&self.log);
                let scope = if all { None } else { channel };
                let turns = tokio::task::spawn_blocking(move || {
                    log.turns(scope.as_deref(), limit)
                        .iter()
                        .map(|t| log.summary(t))
                        .collect::<Vec<_>>()
                })
                .await?;
                let turns: Vec<serde_json::Value> = turns
                    .into_iter()
                    .map(|t| {
                        let mut value = serde_json::to_value(&t).unwrap_or_default();
                        value["current"] = json!(t.turn == ctx.run_id);
                        value
                    })
                    .collect();
                Ok(json!({ "success": true, "turns": turns }))
            }
            "undo" => {
                let turn = params
                    .get("turn")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Changes undo needs `turn` (from `list`)"))?
                    .to_string();
                let log = Arc::clone(&self.log);
                let undone = tokio::task::spawn_blocking(move || log.undo(&turn)).await??;
                Ok(json!({
                    "success": true,
                    "restored": undone.restored.iter().map(|p| self.log.shown_path(p)).collect::<Vec<_>>(),
                    "skipped": undone.skipped.iter().map(|s| json!({
                        "path": self.log.shown_path(&s.path),
                        "why": s.why,
                    })).collect::<Vec<_>>(),
                }))
            }
            other => anyhow::bail!("Changes: unknown action `{other}` (list or undo)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tools::builtin::test_harness::{Caller, ToolHarness};
    use serde_json::json;

    /// ADR-117 through the dispatcher: a turn's `Write`, `Edit` and
    /// shell changes are recorded and undone; another turn's are not.
    #[tokio::test]
    async fn a_turns_writes_and_shell_changes_are_undone() {
        let h = ToolHarness::new().await;
        h.write("kb/notes.md", "one");
        h.write("kb/keep.md", "keep");

        h.call(
            "Edit",
            json!({"file_path": "kb/notes.md", "old_string": "one", "new_string": "two"}),
        )
        .await
        .ok();
        h.call("Write", json!({"file_path": "kb/new.md", "content": "new"}))
            .await
            .ok();
        if cfg!(unix) {
            h.call("Bash", json!({"command": "printf shell > kb/by-shell.md"}))
                .await
                .ok();
        }
        // Another turn's change is not this turn's.
        let other = Caller {
            run: "harness-run-2".into(),
            ..Caller::default()
        };
        h.call_as(
            &other,
            "Write",
            json!({"file_path": "kb/keep.md", "content": "changed"}),
        )
        .await
        .ok();

        let listed = h
            .call("Changes", json!({"action": "list", "all": true}))
            .await
            .ok();
        let turns = listed["turns"].as_array().unwrap();
        assert_eq!(turns.len(), 2, "{listed}");
        let mine = turns.iter().find(|t| t["turn"] == "harness-run-1").unwrap();
        assert_eq!(mine["current"], true);
        let files: Vec<&str> = mine["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert!(
            files.contains(&"kb/notes.md") && files.contains(&"kb/new.md"),
            "{files:?}"
        );
        if cfg!(unix) {
            assert!(files.contains(&"kb/by-shell.md"), "{files:?}");
            assert_eq!(mine["shell"], true);
        }

        let undone = h
            .call(
                "Changes",
                json!({"action": "undo", "turn": "harness-run-1"}),
            )
            .await
            .ok();
        assert!(undone["skipped"].as_array().unwrap().is_empty(), "{undone}");
        let ws = h.workspace();
        assert_eq!(
            std::fs::read_to_string(ws.join("kb/notes.md")).unwrap(),
            "one"
        );
        assert!(!ws.join("kb/new.md").exists());
        assert!(!ws.join("kb/by-shell.md").exists());
        assert_eq!(
            std::fs::read_to_string(ws.join("kb/keep.md")).unwrap(),
            "changed"
        );
    }

    #[tokio::test]
    async fn only_owner_runs_get_changes() {
        let h = ToolHarness::new().await;
        let visitor = Caller::acting_for(peko_tools_core::ActingFor {
            actor: "visitor:local:x".into(),
            name: None,
            trust: peko_tools_core::Trust::Untrusted,
            compartment: peko_tools_core::Compartment::Internal,
            origin: peko_tools_core::Origin::Runtime,
            owner_request: false,
        });
        h.call_as(&visitor, "Changes", json!({"action": "list"}))
            .await
            .err("Changes");
    }
}
