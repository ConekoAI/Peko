//! `peko undo`: put back what a peko's turn changed (ADR-117).

use crate::commands::GlobalPaths;
use clap::Args;
use peko_core::ipc::{DaemonClient, ResponsePacket};

/// `peko undo <peko> [turn]`
#[derive(Args)]
pub struct UndoArgs {
    /// The peko
    pub peko: String,
    /// The turn to undo (from the list); without it, list recent turns
    pub turn: Option<String>,
    /// How many turns to list
    #[arg(long, default_value_t = 10)]
    pub limit: u32,
}

fn unexpected(packet: ResponsePacket) -> anyhow::Error {
    match packet {
        ResponsePacket::Error { message, .. } => anyhow::anyhow!("{message}"),
        other => anyhow::anyhow!("unexpected answer: {}", other.variant_name()),
    }
}

/// Handle `peko undo`
pub async fn handle_undo(args: UndoArgs, _paths: &GlobalPaths, json: bool) -> anyhow::Result<()> {
    let client = DaemonClient::connect()
        .await
        .map_err(|_| anyhow::anyhow!("Peko is not running. Start it with `peko engine start`."))?;
    let Some(turn) = args.turn else {
        return match client
            .turn_changes(&args.peko, None, Some(args.limit))
            .await?
        {
            ResponsePacket::TurnChanges { turns, .. } => {
                if json {
                    println!("{}", serde_json::json!({ "turns": turns }));
                    return Ok(());
                }
                if turns.is_empty() {
                    println!("{} has no recent turns that changed files.", args.peko);
                    return Ok(());
                }
                for t in &turns {
                    let state = match &t.undone_at {
                        Some(at) => format!(" (undone {at})"),
                        None => String::new(),
                    };
                    println!("{}  {}{state}", t.turn, t.updated_at);
                    for f in &t.files {
                        println!("    {:<8} {}", f.change, f.path);
                    }
                    if t.shell {
                        println!(
                            "    (ran shell commands: changes outside the workspace aren't listed)"
                        );
                    }
                }
                println!("Undo one with `peko undo {} <turn>`.", args.peko);
                Ok(())
            }
            other => Err(unexpected(other)),
        };
    };
    match client.turn_undo(&args.peko, &turn).await? {
        ResponsePacket::TurnUndone {
            restored, skipped, ..
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "restored": restored, "skipped": skipped })
                );
                return Ok(());
            }
            if restored.is_empty() {
                println!("Nothing was put back.");
            }
            for path in &restored {
                println!("put back  {path}");
            }
            for s in &skipped {
                println!("left      {} ({})", s.path, s.why);
            }
            Ok(())
        }
        other => Err(unexpected(other)),
    }
}
