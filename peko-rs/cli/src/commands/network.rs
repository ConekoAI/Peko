//! `peko network`: how this computer connects to your other computers
//! and people's (ADR-125 D10).
//!
//! By default everything goes through PekoHub. In `automatic` this
//! computer links directly with the computers on the same network (a LAN,
//! a VPN, Tailscale) and uses PekoHub for the rest; in `direct` nothing
//! of a conversation crosses PekoHub, and messages to computers on other
//! networks wait.

use clap::{Subcommand, ValueEnum};
use peko_core::common::types::config::{Connection, DirectWith};
use peko_core::ipc::{DaemonClient, ResponsePacket};
use peko_core::tunnel::link::{NetworkStatus, Path};

use crate::commands::GlobalPaths;

/// `connection` as typed.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Mode {
    /// Everything through PekoHub (the default)
    Pekohub,
    /// Directly when the other computer is on the same network, through PekoHub otherwise
    Automatic,
    /// Never through PekoHub; messages wait until the other computer is on the same network
    Direct,
}

impl From<Mode> for Connection {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Pekohub => Connection::Pekohub,
            Mode::Automatic => Connection::Automatic,
            Mode::Direct => Connection::Direct,
        }
    }
}

/// `direct_with` as typed.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum With {
    /// The computers on your own account
    Mine,
    /// Also the computers of people you talk with
    People,
}

impl From<With> for DirectWith {
    fn from(with: With) -> Self {
        match with {
            With::Mine => DirectWith::Mine,
            With::People => DirectWith::People,
        }
    }
}

/// `peko network peer` actions.
#[derive(Subcommand)]
pub enum PeerCommands {
    /// Dial this address without being told it (any address you write)
    Add {
        /// `host:port`, like 10.0.0.12:11438
        address: String,
    },
    /// Stop dialing this address
    Remove {
        /// `host:port`
        address: String,
    },
}

/// Network subcommands
#[derive(Subcommand)]
#[command(disable_version_flag = true)]
pub enum NetworkCommands {
    /// How this computer connects, and how each computer is reached
    Status {
        /// JSON output
        #[arg(long)]
        json: bool,
    },

    /// Choose how this computer connects: pekohub, automatic or direct
    Mode {
        mode: Mode,
        /// Listen for direct links here (`ip:port`, default 0.0.0.0:11438)
        #[arg(long)]
        listen: Option<String>,
        /// Do not ask before turning PekoHub off for conversations
        #[arg(long)]
        yes: bool,
    },

    /// Choose which computers this one links with directly
    With { with: With },

    /// Addresses to dial without being told them
    #[command(subcommand)]
    Peer(PeerCommands),

    /// Dial the computers this one knows addresses for, now
    Retry {
        /// One computer's runtime id; every one without
        runtime_id: Option<String>,
    },
}

fn unexpected(packet: ResponsePacket) -> anyhow::Error {
    match packet {
        ResponsePacket::Error { message, .. } => anyhow::anyhow!("{message}"),
        other => anyhow::anyhow!("unexpected answer: {}", other.variant_name()),
    }
}

/// How a path reads in a line of `peko network status`.
#[must_use]
pub fn path_words(path: Path, address: Option<&str>) -> String {
    match path {
        Path::Direct => match address.and_then(host_of) {
            Some(host) => format!("direct ({host})"),
            None => "direct".to_string(),
        },
        Path::Relay => "through PekoHub".to_string(),
        Path::Waiting => "waiting for the same network".to_string(),
        Path::Offline => "offline".to_string(),
    }
}

fn host_of(address: &str) -> Option<String> {
    address
        .parse::<std::net::SocketAddr>()
        .ok()
        .map(|a| a.ip().to_string())
}

fn mode_words(connection: Connection) -> &'static str {
    match connection {
        Connection::Pekohub => "PekoHub only",
        Connection::Automatic => "Automatic (directly when on the same network, PekoHub otherwise)",
        Connection::Direct => "Direct only (never through PekoHub)",
    }
}

fn print_status(status: &NetworkStatus) {
    println!("Connects: {}", mode_words(status.connection));
    if status.connection == Connection::Pekohub {
        return;
    }
    println!(
        "Links with: {}",
        match status.direct_with {
            DirectWith::Mine => "your computers",
            DirectWith::People => "your computers and people you talk with",
        }
    );
    match (&status.listening.address, &status.listening.error) {
        (Some(address), _) => println!("Listening on {address}"),
        (None, Some(error)) => println!("Not listening: {error}"),
        (None, None) => println!("Not listening"),
    }
    if !status.listening.addresses.is_empty() {
        println!("Tells others: {}", status.listening.addresses.join(", "));
    }
    if !status.direct_peers.is_empty() {
        println!("Also dials: {}", status.direct_peers.join(", "));
    }
    if status.peers.is_empty() {
        println!("No other computers yet.");
        return;
    }
    println!();
    for peer in &status.peers {
        let name = peer.name.as_deref().unwrap_or("unnamed computer");
        let whose = peer
            .account
            .as_deref()
            .map(|a| format!(" ({a})"))
            .unwrap_or_default();
        println!(
            "  {name}{whose}  {}",
            path_words(peer.path, peer.address.as_deref())
        );
        println!("      {}", peer.runtime_id);
        if let Some(error) = &peer.last_error {
            if peer.path != Path::Direct {
                println!("      last try: {error}");
            }
        }
    }
}

fn confirm(message: &str) -> anyhow::Result<bool> {
    use std::io::Write;
    print!("{message} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("y"))
}

/// Handle `peko network`
pub async fn handle_network(
    cmd: NetworkCommands,
    _paths: &GlobalPaths,
    json: bool,
) -> anyhow::Result<()> {
    let client = DaemonClient::connect()
        .await
        .map_err(|_| anyhow::anyhow!("Peko is not running. Start it with `peko engine start`."))?;
    let set = |connection: Option<Connection>,
               with: Option<DirectWith>,
               listen: Option<String>,
               peers: Option<Vec<String>>| {
        let client = &client;
        async move {
            match client
                .network_settings_set(connection, with, listen, None, peers)
                .await?
            {
                ResponsePacket::NetworkStatusResult { status, .. } => Ok(status),
                other => Err(unexpected(other)),
            }
        }
    };
    let status = match cmd {
        NetworkCommands::Status { json: local_json } => {
            let status = match client.network_status().await? {
                ResponsePacket::NetworkStatusResult { status, .. } => status,
                other => return Err(unexpected(other)),
            };
            if json || local_json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_status(&status);
            }
            return Ok(());
        }
        NetworkCommands::Mode { mode, listen, yes } => {
            let connection = Connection::from(mode);
            if connection == Connection::Direct && !yes {
                let phones = match client.device_list().await? {
                    ResponsePacket::DeviceListResult { view, .. } => {
                        view.devices.iter().filter(|d| d.kind == "phone").count()
                    }
                    _ => 0,
                };
                println!(
                    "Direct only: nothing of a conversation goes through PekoHub. That means:"
                );
                println!("  - Messages to computers on other networks wait until they are on the same network.");
                println!("  - Paired phones ({phones}) get no answers and no notifications.");
                println!("  - Web chat with a public peko of yours stops.");
                println!("  - This computer accepts direct connections from your computers on this network;");
                println!("    your system may ask to allow it.");
                if !confirm("Turn on Direct only?")? {
                    println!("Nothing changed.");
                    return Ok(());
                }
            } else if connection == Connection::Automatic && !yes && !json {
                println!(
                    "This computer will accept direct connections from your computers on this"
                );
                println!("network; your system may ask to allow it.");
            }
            set(Some(connection), None, listen, None).await?
        }
        NetworkCommands::With { with } => set(None, Some(with.into()), None, None).await?,
        NetworkCommands::Peer(action) => {
            let mut peers = match client.network_status().await? {
                ResponsePacket::NetworkStatusResult { status, .. } => status.direct_peers,
                other => return Err(unexpected(other)),
            };
            match action {
                PeerCommands::Add { address } => {
                    if !peers.contains(&address) {
                        peers.push(address);
                    }
                }
                PeerCommands::Remove { address } => peers.retain(|p| *p != address),
            }
            set(None, None, None, Some(peers)).await?
        }
        NetworkCommands::Retry { runtime_id } => {
            match client.network_link_retry(runtime_id).await? {
                ResponsePacket::Done { .. } => {}
                other => return Err(unexpected(other)),
            }
            if json {
                println!("{}", serde_json::json!({ "retried": true }));
            } else {
                println!("Dialing now.");
            }
            return Ok(());
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_status(&status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_read_as_the_adr_says() {
        assert_eq!(
            path_words(Path::Direct, Some("10.0.0.12:11438")),
            "direct (10.0.0.12)"
        );
        assert_eq!(path_words(Path::Relay, None), "through PekoHub");
        assert_eq!(
            path_words(Path::Waiting, None),
            "waiting for the same network"
        );
        assert_eq!(path_words(Path::Offline, None), "offline");
    }
}
