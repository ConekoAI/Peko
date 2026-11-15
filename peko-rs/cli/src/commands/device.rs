//! `peko device`: the phones paired with this computer (ADR-108).
//!
//! A paired phone reads and posts as you through this computer. Pairing
//! shows a code for the phone to scan; the code is shown here and nowhere
//! else, so only someone at this computer can add a phone.

use crate::commands::GlobalPaths;
use clap::Subcommand;
use peko_core::ipc::{DaemonClient, ResponsePacket};

/// Phone pairing subcommands
#[derive(Subcommand)]
#[command(disable_version_flag = true)]
pub enum DeviceCommands {
    /// Show a code for a phone to scan
    ///
    /// The code works once and for ten minutes. Open Peko on the phone,
    /// sign in, and scan it.
    Pair,

    /// List the phones paired with this computer
    List,

    /// Remove a paired phone
    ///
    /// The phone can no longer read or post through this computer.
    Remove {
        /// The phone's id, from `peko device list`
        device_id: String,
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
            match client.device_list().await? {
                ResponsePacket::DeviceListResult { devices, .. } => {
                    if json {
                        println!("{}", serde_json::to_string(&devices)?);
                    } else if devices.is_empty() {
                        println!("No phone is paired with this computer. Add one with `peko device pair`.");
                    } else {
                        for device in devices {
                            println!(
                                "{}  {} ({})  paired {}  last seen {}",
                                device.device_id,
                                device.name,
                                device.platform,
                                device.paired_at,
                                device.last_seen.as_deref().unwrap_or("never"),
                            );
                        }
                    }
                    Ok(())
                }
                ResponsePacket::Error { message, .. } => anyhow::bail!("{message}"),
                other => anyhow::bail!("unexpected answer: {}", other.variant_name()),
            }
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
}
