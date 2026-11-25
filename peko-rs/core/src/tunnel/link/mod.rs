//! Direct links between computers that can reach each other (ADR-125).
//!
//! Two computers on one network (a LAN, a VPN, Tailscale) open a **link**:
//! a WebSocket over TLS 1.3 that carries the same sealed frames PekoHub
//! would have carried (D1). [`router::Router`] takes over sealing and
//! sending from the PekoHub connection's write loop and outlives any one
//! connection; for each frame it chooses a link when one is up to the
//! computer the frame is sealed to, PekoHub otherwise (D6), or holds it
//! when the relay is turned off (D7).
//!
//! - [`hello`]: the signed `link_hello` bound to the TLS session (D3).
//! - [`tls`]: each engine's self-signed certificate and the exporter.
//! - [`addresses`]: what a computer tells and what it will dial (D5).
//! - [`order`]: `seq` and the receiver's hold across a switch (D6).
//! - [`conn`]: the listener, the dialer and one link's loops.
//! - [`file`]: `runtime/links.json`, the addresses remembered.
//! - [`audit`]: `link.up` / `link.down` / `link.refused`, and the path of
//!   each frame for the tunnel audit.

pub mod addresses;
pub mod audit;
pub mod conn;
pub mod file;
pub mod hello;
pub mod order;
pub mod router;
pub mod tls;

use serde::{Deserialize, Serialize};

pub use crate::common::types::config::{Connection, DirectWith};
pub use router::Router;

/// The sealed type that carries addresses (D5). Exempt from D7 both ways.
pub const ADDRESSES_TYPE: &str = "tunnel_direct_addresses";

/// How frames to a computer travel now (D9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Path {
    /// On a link.
    Direct,
    /// Through PekoHub.
    Relay,
    /// No link, and the relay is not used to it (`direct` here, or it
    /// said it takes no relayed frames): frames wait for a link.
    Waiting,
    /// No link and no PekoHub connection.
    Offline,
}

impl Path {
    /// Its name on the wire and in the audit.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relay => "relay",
            Self::Waiting => "waiting",
            Self::Offline => "offline",
        }
    }
}

/// Where the engine listens for links (D9).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NetworkListening {
    /// The address it listens on, while it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// What it tells other computers to dial.
    #[serde(default)]
    pub addresses: Vec<String>,
    /// Why it does not listen, when it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One computer this one links with, or could (D9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NetworkPeer {
    pub runtime_id: String,
    /// `user:hub:<id>` of the account it is on, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Its name on its device list, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub path: Path,
    /// The address of its link, while there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// RFC 3339: since when its link is up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Why the last attempt to link with it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// What `network_status` answers (D9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NetworkStatus {
    pub connection: Connection,
    pub direct_with: DirectWith,
    pub listening: NetworkListening,
    pub peers: Vec<NetworkPeer>,
    /// The settings as written (for an editor of them): `direct_listen`
    /// (absent: the default), `direct_addresses`, `direct_peers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_listen: Option<String>,
    #[serde(default)]
    pub direct_addresses: Vec<String>,
    #[serde(default)]
    pub direct_peers: Vec<String>,
}

impl NetworkStatus {
    /// The status of a computer whose router is not running.
    #[must_use]
    pub fn idle(network: &crate::common::types::config::NetworkConfig) -> Self {
        Self {
            connection: network.connection,
            direct_with: network.direct_with,
            listening: NetworkListening::default(),
            peers: Vec::new(),
            direct_listen: network.direct_listen.clone(),
            direct_addresses: network.direct_addresses.clone(),
            direct_peers: network.direct_peers.clone(),
        }
    }
}
