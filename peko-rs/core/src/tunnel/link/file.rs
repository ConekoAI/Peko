//! `runtime/links.json` (ADR-125 D5): addresses told and addresses that
//! worked, so after a restart, or with PekoHub down, a computer dials
//! the ones it knew.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What is remembered of one computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRecord {
    /// The addresses it last told (`host:port`).
    #[serde(default)]
    pub told: Vec<String>,
    /// The `iat` of that statement; an older one is ignored.
    #[serde(default)]
    pub told_iat: i64,
    /// Whether it takes relayed frames (D7).
    #[serde(default = "yes")]
    pub relay: bool,
    /// Addresses a link to it came up on, newest first.
    #[serde(default)]
    pub worked: Vec<String>,
    /// RFC 3339: when a link to it last came up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_up: Option<String>,
}

fn yes() -> bool {
    true
}

impl Default for PeerRecord {
    fn default() -> Self {
        Self {
            told: Vec::new(),
            told_iat: 0,
            relay: true,
            worked: Vec::new(),
            last_up: None,
        }
    }
}

/// The file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinksFile {
    /// By runtime id.
    #[serde(default)]
    pub peers: BTreeMap<String, PeerRecord>,
}

/// How many addresses that worked are kept per computer.
const WORKED_KEPT: usize = 4;

impl LinksFile {
    /// Read `path`; an absent or unreadable file is empty.
    #[must_use]
    pub fn load(path: Option<&Path>) -> Self {
        path.and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Write it to `path` (owner-only).
    pub fn save(&self, path: Option<&PathBuf>) {
        let Some(path) = path else { return };
        let bytes = serde_json::to_vec_pretty(self).expect("links file serializes");
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = crate::security::computers::write_private(path, &bytes) {
            tracing::warn!(path = %path.display(), "could not save links.json: {e}");
        }
    }

    /// `runtime` told `addresses` at `iat`. Returns whether it changed
    /// anything (an older statement does not).
    pub fn told(&mut self, runtime: &str, addresses: Vec<String>, relay: bool, iat: i64) -> bool {
        let record = self.peers.entry(runtime.to_string()).or_default();
        if iat < record.told_iat {
            return false;
        }
        let changed = record.told != addresses || record.relay != relay;
        record.told = addresses;
        record.relay = relay;
        record.told_iat = iat;
        changed
    }

    /// A link to `runtime` came up on `address`.
    pub fn worked(&mut self, runtime: &str, address: &str) {
        let record = self.peers.entry(runtime.to_string()).or_default();
        record.worked.retain(|a| a != address);
        record.worked.insert(0, address.to_string());
        record.worked.truncate(WORKED_KEPT);
        record.last_up = Some(chrono::Utc::now().to_rfc3339());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_older_statement_changes_nothing_and_the_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime").join("links.json");
        let mut file = LinksFile::default();
        assert!(file.told("B", vec!["10.0.0.2:11438".into()], true, 100));
        assert!(!file.told("B", vec!["10.0.0.9:11438".into()], true, 50));
        assert_eq!(file.peers["B"].told, vec!["10.0.0.2:11438".to_string()]);
        assert!(file.told("B", vec!["10.0.0.2:11438".into()], false, 101));
        for n in 0..6 {
            file.worked("B", &format!("10.0.0.{n}:11438"));
        }
        file.worked("B", "10.0.0.3:11438");
        assert_eq!(file.peers["B"].worked.len(), WORKED_KEPT);
        assert_eq!(file.peers["B"].worked[0], "10.0.0.3:11438");
        file.save(Some(&path));
        assert_eq!(LinksFile::load(Some(&path)), file);
        assert_eq!(LinksFile::load(None), LinksFile::default());
    }
}
