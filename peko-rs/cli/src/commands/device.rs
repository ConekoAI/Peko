//! `peko device`: the phones paired with this computer (ADR-108) and your
//! other computers it trusts (ADR-109).
//!
//! A paired phone reads and posts as you through this computer. Pairing
//! shows a code for the phone to scan; the code is shown here and nowhere
//! else, so only someone at this computer can add a phone.
//!
//! Another computer signed in to your PekoHub account speaks as you here
//! only once you approve it: the first time it tries, it waits, and is
//! listed here as pending.

use crate::commands::GlobalPaths;
use clap::Subcommand;
use peko_core::ipc::{DaemonClient, ResponsePacket};

/// Phone and computer subcommands
#[derive(Subcommand)]
#[command(disable_version_flag = true)]
pub enum DeviceCommands {
    /// Show a code for a phone to scan
    ///
    /// The code works once and for ten minutes. Open Peko on the phone,
    /// sign in, and scan it.
    Pair,

    /// List the phones paired with this computer and your other computers
    List,

    /// Let one of your other computers use your pekos here as you
    ///
    /// Approve only a computer you just started using Peko on: it can then
    /// do anything you can with your pekos here.
    Approve {
        /// The computer's id (`apr_computer…` or `did:key:…`), from `peko device list`
        id: String,
    },

    /// Refuse a computer that asked to use your pekos here as you
    Deny {
        /// The computer's id (`apr_computer…` or `did:key:…`), from `peko device list`
        id: String,
    },

    /// Remove a paired phone, or stop trusting one of your computers
    ///
    /// Neither can read or act as you through this computer afterwards.
    Remove {
        /// The phone's id, or the computer's (`apr_computer…` or `did:key:…`),
        /// from `peko device list`
        device_id: String,
    },
}

/// Whether `id` names a computer rather than a phone.
fn is_computer_id(id: &str) -> bool {
    id.starts_with("apr_computer") || id.starts_with("did:key:")
}

/// The code as text a terminal can show: two modules per character row.
fn render_qr(text: &str) -> anyhow::Result<String> {
    let code = qrcode::QrCode::new(text.as_bytes())?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build())
}

/// Handle `peko device`
pub async fn handle_device(
    cmd: DeviceCommands,
    _paths: &GlobalPaths,
    json: bool,
) -> anyhow::Result<()> {
    let client = DaemonClient::connect()
        .await
        .map_err(|_| anyhow::anyhow!("Peko is not running. Start it with `peko daemon start`."))?;
    match cmd {
        DeviceCommands::Pair => {
            match client.device_pair_start().await? {
                ResponsePacket::DevicePairStarted {
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
                        println!(
                            "Scan this with Peko on your phone. It works once, until {expires_at}."
                        );
                        println!("Anyone who scans it can read and post as you: show it to your phone only.");
                    }
                    Ok(())
                }
                ResponsePacket::Error { message, .. } => anyhow::bail!("{message}"),
                other => anyhow::bail!("unexpected answer: {}", other.variant_name()),
            }
        }
        DeviceCommands::List => {
            let devices = match client.device_list().await? {
                ResponsePacket::DeviceListResult { devices, .. } => devices,
                ResponsePacket::Error { message, .. } => anyhow::bail!("{message}"),
                other => anyhow::bail!("unexpected answer: {}", other.variant_name()),
            };
            let (runtime, computers) = match client.computer_list().await? {
                ResponsePacket::ComputerListResult {
                    runtime, computers, ..
                } => (runtime, computers),
                ResponsePacket::Error { message, .. } => anyhow::bail!("{message}"),
                other => anyhow::bail!("unexpected answer: {}", other.variant_name()),
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "phones": devices,
                        "computers": computers,
                        "this_computer": runtime,
                    })
                );
                return Ok(());
            }
            println!("Phones");
            if devices.is_empty() {
                println!("  None paired. Add one with `peko device pair`.");
            }
            for device in devices {
                println!(
                    "  {}  {} ({})  paired {}  last seen {}",
                    device.device_id,
                    device.name,
                    device.platform,
                    device.paired_at,
                    device.last_seen.as_deref().unwrap_or("never"),
                );
            }
            println!();
            println!("Computers (this one is {runtime})");
            if computers.is_empty() {
                println!("  None of your other computers has reached this one.");
            }
            for computer in computers {
                let status = match computer.status.as_str() {
                    "pending" => "waiting for you: `peko device approve` or `peko device deny`",
                    other => other,
                };
                println!(
                    "  {}  {}  first seen {}  {status}",
                    computer.id, computer.runtime, computer.first_seen,
                );
            }
            Ok(())
        }
        DeviceCommands::Approve { id } => decide_computer(&client, &id, true, json).await,
        DeviceCommands::Deny { id } => decide_computer(&client, &id, false, json).await,
        DeviceCommands::Remove { device_id } if is_computer_id(&device_id) => {
            decide_computer(&client, &device_id, false, json).await
        }
        DeviceCommands::Remove { device_id } => match client.device_remove(&device_id).await? {
            ResponsePacket::Done { success: true, .. } => {
                if json {
                    println!("{}", serde_json::json!({ "removed": device_id }));
                } else {
                    println!("Removed {device_id}. It can no longer reach this computer.");
                }
                Ok(())
            }
            ResponsePacket::Done { error, .. } => {
                anyhow::bail!("{}", error.unwrap_or_else(|| "not removed".into()))
            }
            ResponsePacket::Error { message, .. } => anyhow::bail!("{message}"),
            other => anyhow::bail!("unexpected answer: {}", other.variant_name()),
        },
    }
}

/// Approve or deny one of your other computers (ADR-109 D3).
async fn decide_computer(
    client: &DaemonClient,
    id: &str,
    approve: bool,
    json: bool,
) -> anyhow::Result<()> {
    match client.computer_decide(id, approve).await? {
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
        ResponsePacket::Error { message, .. } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected answer: {}", other.variant_name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pairing_code_renders_as_a_qr_code() {
        let text = "peko://pair?v=1&hub=https%3A%2F%2Fapi.pekohub.ai&runtime=did%3Akey%3Az6Mk&id=pair_0123456789abcdef&secret=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let rendered = render_qr(text).unwrap();
        assert!(rendered.lines().count() > 10);
        assert!(rendered.contains('\u{2588}') || rendered.contains('\u{2580}'));
    }

    #[test]
    fn computers_are_told_apart_from_phones() {
        assert!(is_computer_id("apr_computer0123456789abcdef0123"));
        assert!(is_computer_id("did:key:z6MkTest"));
        assert!(!is_computer_id("iphone-4F7K"));
    }
}
