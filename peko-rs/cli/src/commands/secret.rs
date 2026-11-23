//! `peko secret`: what your pekos asked you for (ADR-116).
//!
//! A peko asks for a token or key with its `Secret` tool instead of in
//! the chat. Give it here (or on the card in the Peko app): the value is
//! read without echo, goes to the vault, and never reaches the
//! conversation or the model. The peko is told it was saved.

use crate::commands::GlobalPaths;
use clap::Subcommand;
use peko_core::ipc::{DaemonClient, ResponsePacket};

/// Secret subcommands
#[derive(Subcommand)]
#[command(disable_version_flag = true)]
pub enum SecretCommands {
    /// List a peko's secrets and the requests waiting for you
    List {
        /// The peko
        peko: String,
    },

    /// Give a peko a secret it asked for, or a new one
    ///
    /// The value is read without echo (or from stdin when piped).
    Set {
        /// The peko
        peko: String,
        /// The request (`sec_…`) or the secret's name (e.g. GITHUB_TOKEN)
        id: String,
    },

    /// Decline a secret request
    Decline {
        /// The peko
        peko: String,
        /// The request (`sec_…`) or the secret's name
        id: String,
    },

    /// Delete one of a peko's secrets
    Delete {
        /// The peko
        peko: String,
        /// The secret's name
        name: String,
    },
}

/// The value, typed without echo, or the first line of stdin when it
/// is piped.
fn read_secret(prompt: &str) -> anyhow::Result<String> {
    use std::io::{BufRead, IsTerminal};
    if std::io::stdin().is_terminal() {
        return rpassword::prompt_password(prompt)
            .map_err(|e| anyhow::anyhow!("failed to read the secret: {e}"));
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn unexpected(packet: ResponsePacket) -> anyhow::Error {
    match packet {
        ResponsePacket::Error { message, .. } => anyhow::anyhow!("{message}"),
        other => anyhow::anyhow!("unexpected answer: {}", other.variant_name()),
    }
}

fn done(packet: ResponsePacket, json: bool) -> anyhow::Result<()> {
    match packet {
        ResponsePacket::SecretDone { message, .. } => {
            if json {
                println!("{}", serde_json::json!({ "message": message }));
            } else {
                println!("{message}");
            }
            Ok(())
        }
        other => Err(unexpected(other)),
    }
}

/// Handle `peko secret`
pub async fn handle_secret(
    cmd: SecretCommands,
    _paths: &GlobalPaths,
    json: bool,
) -> anyhow::Result<()> {
    let client = DaemonClient::connect()
        .await
        .map_err(|_| anyhow::anyhow!("Peko is not running. Start it with `peko engine start`."))?;
    match cmd {
        SecretCommands::List { peko } => match client.secret_list(&peko).await? {
            ResponsePacket::SecretList {
                secrets, waiting, ..
            } => {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({ "secrets": secrets, "waiting": waiting })
                    );
                    return Ok(());
                }
                if !secrets.is_empty() {
                    println!("{peko}'s secrets: {}", secrets.join(", "));
                } else if waiting.is_empty() {
                    println!("{peko} has no secrets.");
                }
                if !waiting.is_empty() {
                    println!("{peko} is waiting for:");
                }
                for request in &waiting {
                    println!(
                        "  {}  {} — {} (until {})",
                        request.id, request.secret, request.why, request.expires_at
                    );
                }
                if let Some(first) = waiting.first() {
                    println!("Give one with `peko secret set {peko} {}`.", first.id);
                }
                Ok(())
            }
            other => Err(unexpected(other)),
        },
        SecretCommands::Set { peko, id } => {
            let value = read_secret(&format!("{id}: "))?;
            anyhow::ensure!(!value.trim().is_empty(), "nothing entered");
            done(client.secret_provide(&peko, &id, value).await?, json)
        }
        SecretCommands::Decline { peko, id } => {
            done(client.secret_decline(&peko, &id).await?, json)
        }
        SecretCommands::Delete { peko, name } => {
            done(client.secret_delete(&peko, &name).await?, json)
        }
    }
}
