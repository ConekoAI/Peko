//! Auth data types

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// API key entry stored on disk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyEntry {
    /// Key ID (prefix: pkr_ + first 8 chars of the key)
    pub id: String,
    /// Argon2id PHC string of the full key (`$argon2id$v=19$m=...,t=...,p=...$<salt>$<hash>`).
    /// Verify via [`crate::api_key::verify_password_hash`].
    pub hash: String,
    /// Human-readable name
    pub name: String,
    /// When the key was created
    pub created_at: DateTime<Utc>,
    /// When the key was last used
    pub last_used_at: Option<DateTime<Utc>>,
    /// Granted scopes
    pub scopes: Vec<ApiKeyScope>,
    /// Whether the key is enabled
    pub enabled: bool,
}

/// API key permission scopes
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiKeyScope {
    Read,
    Write,
    Admin,
    /// ADR-121 D3: the caller counts as the peko's local owner — every
    /// check that passes for the socket passes for it — except that it
    /// may not make, list or revoke keys. Implies every other scope.
    Owner,
}

impl std::fmt::Display for ApiKeyScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read => write!(f, "read"),
            Self::Write => write!(f, "write"),
            Self::Admin => write!(f, "admin"),
            Self::Owner => write!(f, "owner"),
        }
    }
}

impl std::str::FromStr for ApiKeyScope {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "admin" => Ok(Self::Admin),
            "owner" => Ok(Self::Owner),
            _ => Err(format!("Unknown scope: {s}")),
        }
    }
}

/// On-disk format for api_keys.toml
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeysFile {
    pub version: String,
    #[serde(rename = "key")]
    pub keys: Vec<ApiKeyEntry>,
}

impl Default for ApiKeysFile {
    fn default() -> Self {
        Self {
            version: "1".to_string(),
            keys: Vec::new(),
        }
    }
}

/// Pekohub configuration stored on disk
///
/// Defined for forward compatibility. Used by the tunnel dispatcher
/// when `enable_pekohub_jwt` is true (issue #17): the dispatcher reads
/// `Authorization: Bearer <jwt>` from the bridge payload, validates
/// against the runtime DID as audience and the configured trusted
/// issuers, and uses the validated `sub` claim as the caller's
/// identity.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PekohubConfig {
    pub version: String,
    pub runtime_did: String,
    #[serde(flatten)]
    pub credential: PekohubCredential,
    pub pekohub: PekohubEndpoints,
}

/// Pekohub credential types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "credential_type")]
pub enum PekohubCredential {
    #[serde(rename = "shared_secret")]
    SharedSecret { secret: String },
    #[serde(rename = "mtls")]
    Mtls {
        cert_path: PathBuf,
        key_path: PathBuf,
    },
}

/// Pekohub endpoint configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PekohubEndpoints {
    pub issuer: String,
    pub jwks_url: String,
    pub token_endpoint: String,
}

/// On-disk format for auth_config.toml. Every field has a default, so a
/// file that sets only `enable_api_key = true` parses (ADR-121).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfigFile {
    pub version: String,
    /// Enable local trust (Unix socket / Windows named pipe)
    pub enable_local_trust: bool,
    /// Enable pekohub JWT authentication
    pub enable_pekohub_jwt: bool,
    /// Enable API key authentication
    pub enable_api_key: bool,
    /// Trusted JWT issuers
    pub trusted_issuers: Vec<String>,
    /// Rate limit configuration
    pub rate_limit: RateLimitConfigFile,
}

impl Default for AuthConfigFile {
    fn default() -> Self {
        Self {
            version: "1".to_string(),
            enable_local_trust: true,
            enable_pekohub_jwt: true,
            enable_api_key: false,
            trusted_issuers: vec!["pekohub".to_string()],
            rate_limit: RateLimitConfigFile::default(),
        }
    }
}

/// Rate limit configuration on disk
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfigFile {
    pub jwt_requests_per_minute: u32,
    pub api_key_requests_per_minute: u32,
    pub burst_jwt: u32,
    pub burst_api_key: u32,
}

impl Default for RateLimitConfigFile {
    fn default() -> Self {
        Self {
            jwt_requests_per_minute: 30,
            api_key_requests_per_minute: 100,
            burst_jwt: 10,
            burst_api_key: 20,
        }
    }
}

#[cfg(test)]
mod auth_config_file_tests {
    use super::*;

    /// ADR-121: one line turns API keys on; the rest keeps its defaults.
    #[test]
    fn a_file_with_one_setting_parses() {
        let file: AuthConfigFile = toml::from_str("enable_api_key = true\n").unwrap();
        assert!(file.enable_api_key);
        assert!(file.enable_local_trust);
        assert_eq!(file.rate_limit.api_key_requests_per_minute, 100);
    }
}
