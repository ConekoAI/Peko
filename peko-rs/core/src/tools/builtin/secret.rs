//! `Secret` — ask the owner for a secret (ADR-116).
//!
//! The owner answers on a card in the Peko app or with `peko secret
//! set`; the value goes to the vault, never into the conversation, the
//! model's context, or the ledger. The session that asked is told when
//! it is saved or declined. Owner runs only (the trust floor).

use async_trait::async_trait;
use peko_tools_core::{Tool, ToolContext};
use serde_json::json;

use crate::principal::secret_requests::{
    self, Target, MAX_WAITING, MAX_WHY_CHARS, SECRET_REQUEST_TTL_HOURS, SECRET_TOOL_NAME,
};

/// Ask the owner for a secret; list the ones the peko has.
pub struct SecretTool;

impl SecretTool {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for SecretTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SecretTool {
    fn name(&self) -> &'static str {
        SECRET_TOOL_NAME
    }

    fn description(&self) -> String {
        format!(
            r#"Ask your owner for a secret (a token, password, or API key) without it ever passing through the conversation. Never ask for one in a message, and never accept one pasted there: ask with this tool instead.

- `request` with `name` and `why`: your owner gets a card to enter it, and a note in your conversation with them. Nothing waits: end your turn after it, saying in a line what you need it for. When they save or decline it, you are told in this session.
  - Without `server`, `name` is an environment variable name (e.g. `GITHUB_TOKEN`). Once saved, pass `secrets: ["GITHUB_TOKEN"]` to the `Bash` calls that need it: it is set as that variable for them only, and its value is taken out of their output. Never print it, write it to a file, or put it in a message.
  - With `server`, `name` is a credential slot of that MCP server in your workspace (`{{"credential": "<slot>"}}` in its manifest); the server reads it when it next starts.
- `list`: the secrets you have and the requests still waiting. Never their values.

`why` is shown to the owner (at most {MAX_WHY_CHARS} characters). Asking again for a secret already waiting returns that request. At most {MAX_WAITING} wait at once; a request lapses after {SECRET_REQUEST_TTL_HOURS} hours.

## Result
`request`: `{{id, secret, status: "waiting"}}`. `list`: `{{secrets: [name], waiting: [{{id, secret, why, expires_at}}]}}`."#
        )
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["request", "list"],
                    "description": "`request` asks the owner for one secret; `list` shows what you have."
                },
                "name": {
                    "type": "string",
                    "description": "request: an environment variable name (GITHUB_TOKEN), or with `server`, the MCP credential slot."
                },
                "why": {
                    "type": "string",
                    "description": "request: what you need it for, shown to the owner."
                },
                "server": {
                    "type": "string",
                    "description": "request: the MCP server (in your workspace's mcp/) whose credential slot `name` is."
                }
            },
            "required": ["action"]
        })
    }

    fn parallelizable(&self) -> bool {
        // Files requests and reads the vault: calls apply in order.
        false
    }

    async fn execute(&self, _params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        Err(anyhow::anyhow!(
            "Secret requires a ToolContext (use execute_with_context)"
        ))
    }

    async fn execute_with_context(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<serde_json::Value> {
        anyhow::ensure!(
            ctx.acting_for().is_owner(),
            "Secret is only for work done for the owner"
        );
        let principal_id = ctx
            .principal_id
            .as_deref()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Secret requires a principal"))?;
        let text = |key: &str| params.get(key).and_then(serde_json::Value::as_str);
        let action = text("action").unwrap_or_default();
        // What was asked for is checked before anything is looked up.
        let target = match action {
            "request" => {
                let name =
                    text("name").ok_or_else(|| anyhow::anyhow!("Secret request needs `name`"))?;
                Some(Target::parse(name, text("server"))?)
            }
            "list" => None,
            other => anyhow::bail!("Secret: unknown action `{other}` (request or list)"),
        };
        let desk = secret_requests::secret_desk()
            .ok_or_else(|| anyhow::anyhow!("secrets are not available in this runtime"))?;
        let principal = desk
            .principal(principal_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("principal not loaded"))?;

        match target {
            Some(target) => {
                let record = secret_requests::request(
                    &principal,
                    target,
                    text("why").unwrap_or_default(),
                    ctx.session_id.clone(),
                )
                .await?;
                Ok(json!({
                    "success": true,
                    "id": record.id,
                    "secret": record.target.label(),
                    "status": "waiting",
                    "expires_at": record.expires_at.to_rfc3339(),
                }))
            }
            None => Ok(json!({
                "success": true,
                "secrets": desk.secret_names(principal_id),
                "waiting": secret_requests::waiting(&principal)
                    .iter()
                    .map(|r| json!({
                        "id": r.id,
                        "secret": r.target.label(),
                        "why": r.why,
                        "expires_at": r.expires_at.to_rfc3339(),
                    }))
                    .collect::<Vec<_>>(),
            })),
        }
    }
}
