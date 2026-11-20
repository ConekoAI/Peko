//! `peko device`: your account's devices (ADR-111).
//!
//! Your computers and phones are one list, signed by the devices on it.
//! A device joins with a code shown on one already on the list: `peko
//! device add` here shows it, and on the new device you type it (`peko
//! device join` on a computer) or scan it (the phone app). Nothing else
//! adds a device: a PekoHub sign-in alone does not.
//!
//! `approve` and `deny` decide what waits about your account: a peko's
//! owner changed on PekoHub, a peko PekoHub names with a new identity, or
//! a contact whose devices started over.

use crate::commands::GlobalPaths;
use clap::Subcommand;
use peko_core::ipc::{DaemonClient, ResponsePacket};

/// Device subcommands
#[derive(Subcommand)]
#[command(disable_version_flag = true)]
pub enum DeviceCommands {
    /// Show a code that adds a device to your account
    ///
    /// The code works once and for ten minutes. On a phone, open Peko and
    /// scan it; on another computer, run `peko device join <code>`.
    Add,

    /// Join this computer to your account with a code from another device
    ///
    /// On a device already on your account, choose Add a device (or run
    /// `peko device add`) and type the code it shows here.
    Join {
        /// The code, like ABCD-EFGH-JKMN
        code: String,
    },

    /// List your account's devices, and what waits for you about it
    List,

    /// Remove a device from your account
    ///
    /// No computer of yours acts on it afterwards. It can join again with a
    /// new code.
    Remove {
        /// The device's id (`did:key:…`, or a phone's `dev_…`), from
        /// `peko device list`
        device_id: String,
    },

    /// Approve something that waits about your account (from `peko device list`)
    Approve {
        /// Its id (`apr_owner…`, `apr_keychange…`, `apr_devreset…`)
        id: String,
    },

    /// Refuse something that waits about your account
    Deny {
        /// Its id (`apr_owner…`, `apr_keychange…`, `apr_devreset…`)
        id: String,
    },
}

/// The code as text a terminal can show: two modules per character row.
fn render_qr(text: &str) -> anyhow::Result<String> {
    let code = qrcode::QrCode::new(text.as_bytes())?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build())
}

fn unexpected(packet: ResponsePacket) -> anyhow::Error {
    match packet {
        ResponsePacket::Error { message, .. } => anyhow::anyhow!("{message}"),
        other => anyhow::anyhow!("unexpected answer: {}", other.variant_name()),
    }
}

/// Handle `peko device`
pub async fn handle_device(
    cmd: DeviceCommands,
    _paths: &GlobalPaths,
    json: bool,
) -> anyhow::Result<()> {
    let client = DaemonClient::connect()
        .await
        .map_err(|_| anyhow::anyhow!("Peko is not running. Start it with `peko engine start`."))?;
    match cmd {
        DeviceCommands::Add => match client.device_add().await? {
            ResponsePacket::DeviceCodeShown {
                code,
                qr,
                expires_at,
                ..
            } => {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({ "code": code, "qr": qr, "expires_at": expires_at })
                    );
                } else {
                    println!("{}", render_qr(&qr)?);
                    println!("  {code}");
                    println!();
                    println!("Scan it with Peko on your phone, or run `peko device join {code}` on another computer.");
                    println!("It works once, until {expires_at}. Whoever uses it acts as you: show it to your own device only.");
                }
                Ok(())
            }
            other => Err(unexpected(other)),
        },
        DeviceCommands::Join { code } => {
            if !json {
                println!("Joining… keep the device that shows the code open.");
            }
            match client.device_join(&code).await? {
                ResponsePacket::Done { success: true, .. } => {
                    if json {
                        println!("{}", serde_json::json!({ "joined": true }));
                    } else {
                        println!("This computer is on your account now.");
                    }
                    Ok(())
                }
                other => Err(unexpected(other)),
            }
        }
        DeviceCommands::List => {
            let view = match client.device_list().await? {
                ResponsePacket::DeviceListResult { view, .. } => view,
                other => return Err(unexpected(other)),
            };
            if json {
                println!("{}", serde_json::to_string(&view)?);
                return Ok(());
            }
            let Some(account) = &view.account else {
                println!("This computer is not registered with PekoHub. Run `peko runtime register` first.");
                return Ok(());
            };
            println!("Devices of {account}");
            if let Some(code) = &view.code {
                println!("  Your account's code: {code}");
            }
            if !view.on_list {
                println!("  This computer is not on them yet: on a device you already use, choose");
                println!("  Add a device (or run `peko device add`), then `peko device join <code>` here.");
            }
            for device in &view.devices {
                let name = device.name.as_deref().unwrap_or("unnamed");
                let this = if device.this { "  (this computer)" } else { "" };
                let seen = device
                    .last_seen
                    .as_deref()
                    .map(|s| format!("  last seen {s}"))
                    .unwrap_or_default();
                println!(
                    "  {} {name}{this}\n      {}  added {}{seen}",
                    device.kind,
                    device.hub_device_id.as_deref().unwrap_or(&device.id),
                    device.added_at,
                );
            }
            if !view.security.is_empty() {
                println!();
                println!("Waiting for you");
                for item in &view.security {
                    println!("  {}  {}", item.id, item.summary);
                    if !item.reason.is_empty() {
                        println!("      {}", item.reason);
                    }
                    // ADR-112: what to compare before approving.
                    if let Some(code) = &item.code {
                        println!("      Its code: {code}");
                        if let Some(old) = &item.pinned_code {
                            println!("      Until now: {old}");
                        }
                        println!("      Approve only if its owner reads you the same code.");
                    }
                }
                println!("  Decide with `peko device approve <id>` or `peko device deny <id>`.");
            }
            Ok(())
        }
        DeviceCommands::Remove { device_id } => match client.device_remove(&device_id).await? {
            ResponsePacket::Done { success: true, .. } => {
                if json {
                    println!("{}", serde_json::json!({ "removed": device_id }));
                } else {
                    println!("Removed {device_id} from your account.");
                }
                Ok(())
            }
            other => Err(unexpected(other)),
        },
        DeviceCommands::Approve { id } => decide(&client, &id, true, json).await,
        DeviceCommands::Deny { id } => decide(&client, &id, false, json).await,
    }
}

async fn decide(client: &DaemonClient, id: &str, approve: bool, json: bool) -> anyhow::Result<()> {
    match client.security_decide(id, approve).await? {
        ResponsePacket::ApprovalDecided { message, .. } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "id": id, "approved": approve, "message": message })
                );
            } else {
                println!("{message}");
            }
            Ok(())
        }
        other => Err(unexpected(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_join_code_renders_as_a_qr_code() {
        let text = "peko://join?v=1&hub=https%3A%2F%2Fapi.pekohub.ai&code=K7DXKQ7RMTPZ";
        let rendered = render_qr(text).unwrap();
        assert!(rendered.lines().count() > 10);
        assert!(rendered.contains('\u{2588}') || rendered.contains('\u{2580}'));
    }
}
