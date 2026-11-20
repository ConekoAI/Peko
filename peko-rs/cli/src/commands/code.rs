//! `peko code`: an identity's code, to read to its owner (ADR-112).
//!
//! PekoHub tells this computer who someone is the first time they meet,
//! and when a peko or a person's devices change. A code is how two owners
//! check that word: each reads the code on their own screen, and they
//! match only if this computer holds the identity its owner holds.

use crate::commands::GlobalPaths;
use clap::Args;
use peko_core::ipc::{DaemonClient, ResponsePacket};

/// `peko code` arguments
#[derive(Args)]
pub struct CodeArgs {
    /// Whose code: one of your pekos (name), `peko:hub:<id>` for someone
    /// else's peko, or `user:hub:<id>` for a person. Your account's when
    /// left out.
    pub who: Option<String>,
}

/// Handle `peko code`
pub async fn handle_code(args: CodeArgs, _paths: &GlobalPaths, json: bool) -> anyhow::Result<()> {
    let client = DaemonClient::connect()
        .await
        .map_err(|_| anyhow::anyhow!("Peko is not running. Start it with `peko engine start`."))?;
    let of = args.who.unwrap_or_else(|| "account".to_string());
    match client.identity_code(&of).await? {
        ResponsePacket::IdentityCodeResult { of, name, code, .. } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "of": of, "name": name, "code": code })
                );
                return Ok(());
            }
            let whose = match (&name, of.as_str()) {
                (Some(name), _) => format!("{name}'s"),
                (None, "account") => "Your account's".to_string(),
                (None, other) if other.starts_with("user:") => format!("The person {other}'s"),
                (None, other) => format!("The peko {other}'s"),
            };
            match code {
                Some(code) => {
                    println!("{whose} code:");
                    println!("  {code}");
                    if of == "account" {
                        println!("Read it to someone who wants to check they reach you.");
                    } else {
                        println!("It matches the code its owner sees only if this computer");
                        println!(
                            "reaches the {} they know.",
                            if of.starts_with("user:") {
                                "person"
                            } else {
                                "peko"
                            }
                        );
                    }
                }
                None if of == "account" => {
                    println!("This computer does not hold your account's device list yet.");
                    println!("Run `peko device list` to see why.");
                }
                None => println!("This computer has not met {of} yet, so it has no code for it."),
            }
            Ok(())
        }
        ResponsePacket::Error { message, .. } => Err(anyhow::anyhow!(
            "{}",
            message
                .trim_start_matches(|c| c != ']')
                .trim_start_matches("] ")
        )),
        other => Err(anyhow::anyhow!("Unexpected engine response: {other:?}")),
    }
}
