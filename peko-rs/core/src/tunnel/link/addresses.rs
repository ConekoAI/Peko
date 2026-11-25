//! Where a computer can be reached directly (ADR-125 D5).
//!
//! A runtime tells the computers it links with its addresses in a sealed
//! `tunnel_direct_addresses` frame, signed by its runtime key. What it
//! tells by default is each private address of its own interfaces with
//! the port it listens on. What it dials of what it was told is bounded:
//! private addresses only (a told name only when every address it
//! resolves to is private), at most [`MAX_TOLD`] per computer, so a peer
//! cannot point this computer at hosts on the internet.

use std::net::{IpAddr, SocketAddr};

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};

use crate::tunnel::did_key::did_key_to_verifying_key;
use crate::tunnel::tunnel_channel_signature::{jws_sign, jws_verify};

/// Version tag of an address statement.
pub const DIRECT_ADDRESSES_VERSION: &str = "peko-direct-addresses/1";
/// The most addresses dialed (or told) per computer.
pub const MAX_TOLD: usize = 8;

/// What a `tunnel_direct_addresses` frame signs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectAddresses {
    pub v: String,
    pub runtime_id: String,
    /// `host:port`, at most [`MAX_TOLD`].
    pub addresses: Vec<String>,
    /// Whether it takes frames relayed through PekoHub (`false` in
    /// `direct`, D7).
    pub relay: bool,
    pub iat: i64,
}

/// Sign this runtime's addresses.
#[must_use]
pub fn sign_addresses(
    key: &SigningKey,
    runtime_id: &str,
    addresses: &[String],
    relay: bool,
) -> String {
    let payload = DirectAddresses {
        v: DIRECT_ADDRESSES_VERSION.into(),
        runtime_id: runtime_id.into(),
        addresses: addresses.iter().take(MAX_TOLD).cloned().collect(),
        relay,
        iat: chrono::Utc::now().timestamp(),
    };
    jws_sign(
        key,
        &serde_json::to_vec(&payload).expect("addresses serialize"),
    )
}

/// Check an address statement that came from `source`: signed by the
/// runtime it names, which is `source`.
///
/// # Errors
///
/// It is not a statement, or `source` did not sign it.
pub fn verify_addresses(compact: &str, source: &str) -> Result<DirectAddresses, String> {
    let key = did_key_to_verifying_key(source).map_err(|e| format!("{source}: {e}"))?;
    let (_, bytes) = jws_verify(&key, compact).map_err(|e| e.to_string())?;
    let statement: DirectAddresses =
        serde_json::from_slice(&bytes).map_err(|e| format!("addresses: {e}"))?;
    if statement.v != DIRECT_ADDRESSES_VERSION {
        return Err(format!(
            "addresses version must be {DIRECT_ADDRESSES_VERSION:?}"
        ));
    }
    if statement.runtime_id != source {
        return Err("the statement names another runtime".into());
    }
    Ok(statement)
}

/// Whether `ip` is in a private range a told address may name: `10/8`,
/// `172.16/12`, `192.168/16`, `100.64/10` (carrier-grade NAT, which
/// Tailscale uses), `fc00::/7`. Loopback, link-local and public
/// addresses are not.
#[must_use]
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 10
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168)
                || (a == 100 && (64..=127).contains(&b))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private(IpAddr::V4(v4));
            }
            (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// The addresses this computer tells, from its interfaces' addresses:
/// the private ones, with `port`, at most [`MAX_TOLD`].
#[must_use]
pub fn own_addresses(interfaces: impl IntoIterator<Item = IpAddr>, port: u16) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for ip in interfaces {
        if !is_private(ip) {
            continue;
        }
        let address = SocketAddr::new(ip, port).to_string();
        if !out.contains(&address) {
            out.push(address);
        }
        if out.len() == MAX_TOLD {
            break;
        }
    }
    out
}

/// The interfaces' addresses as the OS reports them now.
#[must_use]
pub fn interface_addresses() -> Vec<IpAddr> {
    match if_watch::tokio::IfWatcher::new() {
        Ok(watcher) => watcher.iter().map(|net| net.addr()).collect(),
        Err(e) => {
            tracing::warn!("could not list this computer's network interfaces: {e}");
            Vec::new()
        }
    }
}

/// What of `told` this computer will dial: each `ip:port` that is
/// private, and each `name:port` whose every resolved address is, at most
/// [`MAX_TOLD`] in all.
pub async fn dialable(told: &[String]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for entry in told.iter().take(MAX_TOLD) {
        let resolved: Vec<SocketAddr> = if let Ok(address) = entry.parse::<SocketAddr>() {
            vec![address]
        } else {
            match tokio::net::lookup_host(entry.as_str()).await {
                Ok(addrs) => addrs.collect(),
                Err(_) => continue,
            }
        };
        if resolved.is_empty() || !resolved.iter().all(|a| is_private(a.ip())) {
            continue;
        }
        for address in resolved {
            if out.len() == MAX_TOLD {
                return out;
            }
            if !out.contains(&address) {
                out.push(address);
            }
        }
    }
    out
}

/// What the operator wrote in `direct_peers`: any address, resolved.
pub async fn resolve_any(entry: &str) -> Vec<SocketAddr> {
    if let Ok(address) = entry.parse::<SocketAddr>() {
        return vec![address];
    }
    tokio::net::lookup_host(entry)
        .await
        .map(|a| a.collect())
        .unwrap_or_default()
}

/// Check that a settings address is `host:port`.
///
/// # Errors
///
/// It is not `host:port`.
pub fn check_host_port(entry: &str) -> Result<(), String> {
    let (host, port) = entry
        .rsplit_once(':')
        .ok_or_else(|| format!("{entry:?} is not host:port"))?;
    if host.is_empty() || port.parse::<u16>().is_err() {
        return Err(format!("{entry:?} is not host:port"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::verifying_key_to_did_key;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn private_ranges_only() {
        for private in [
            "10.0.0.5",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.20",
            "100.64.0.1",
            "100.127.255.255",
            "fd7a:115c:a1e0::1",
            "fc00::1",
            "::ffff:192.168.0.3",
        ] {
            assert!(is_private(ip(private)), "{private} is private");
        }
        for not in [
            "8.8.8.8",
            "172.15.0.1",
            "172.32.0.1",
            "192.169.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_private(ip(not)), "{not} is not private");
        }
    }

    #[test]
    fn own_addresses_are_the_private_ones_capped_at_eight() {
        let mut interfaces = vec![ip("127.0.0.1"), ip("8.8.4.4"), ip("fe80::1")];
        interfaces.extend((1..=12).map(|n| ip(&format!("10.0.0.{n}"))));
        let told = own_addresses(interfaces, 11438);
        assert_eq!(told.len(), MAX_TOLD);
        assert_eq!(told[0], "10.0.0.1:11438");
        assert!(told.iter().all(|a| a.starts_with("10.0.0.")));
        let v6 = own_addresses([ip("fd00::5")], 11438);
        assert_eq!(v6, vec!["[fd00::5]:11438".to_string()]);
    }

    #[tokio::test]
    async fn only_private_told_addresses_are_dialed_at_most_eight() {
        let mut told: Vec<String> = vec![
            "8.8.8.8:11438".into(),
            "127.0.0.1:11438".into(),
            "localhost:11438".into(),
            "not an address".into(),
        ];
        told.extend((1..=10).map(|n| format!("192.168.0.{n}:11438")));
        let dialed = dialable(&told).await;
        // The first eight entries are considered; four of them are not
        // private, so four of the eight are dialed.
        assert_eq!(dialed.len(), 4);
        assert!(dialed.iter().all(|a| is_private(a.ip())));

        let many: Vec<String> = (1..=20).map(|n| format!("10.1.0.{n}:1")).collect();
        assert_eq!(dialable(&many).await.len(), MAX_TOLD);
    }

    #[test]
    fn statements_verify_only_from_their_signer() {
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let did = verifying_key_to_did_key(&key.verifying_key());
        let other = verifying_key_to_did_key(&SigningKey::from_bytes(&[8u8; 32]).verifying_key());
        let many: Vec<String> = (1..=12).map(|n| format!("10.0.0.{n}:11438")).collect();
        let compact = sign_addresses(&key, &did, &many, false);
        let statement = verify_addresses(&compact, &did).unwrap();
        assert_eq!(statement.addresses.len(), MAX_TOLD);
        assert!(!statement.relay);
        assert!(verify_addresses(&compact, &other).is_err());
        // A statement naming another runtime than its signer.
        let lying = sign_addresses(&key, &other, &many, true);
        assert!(verify_addresses(&lying, &did).is_err());
    }

    #[test]
    fn settings_addresses_need_a_port() {
        assert!(check_host_port("10.0.0.5:11438").is_ok());
        assert!(check_host_port("studio.tail1234.ts.net:11438").is_ok());
        assert!(check_host_port("[fd00::1]:11438").is_ok());
        assert!(check_host_port("10.0.0.5").is_err());
        assert!(check_host_port(":11438").is_err());
        assert!(check_host_port("host:port").is_err());
    }
}
