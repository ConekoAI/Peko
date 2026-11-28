//! Publishing a seed on PekoHub as this computer's person (ADR-127 D10).
//!
//! A published seed belongs to a person: PekoHub files it under their
//! handle, and only they publish its next version. From this computer the
//! request is signed with the runtime's key (a `PekoRuntime` proof, as
//! for `PUT /v1/people/me/name`), which PekoHub takes for the account the
//! computer is registered to and is on the device list of (runtime
//! ADR-090, ADR-111). So the publisher is that person exactly as on the
//! website; no seed belongs to a computer or a peko. A computer that is
//! not signed in publishes nothing.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::common::paths::PathResolver;
use crate::common::vault::Vault;
use crate::tunnel::hub_directory::HubAgentDirectoryClient;
use crate::tunnel::registration::{linked_account, runtime_key};

/// The most bytes a README may have (PekoHub's limit).
pub const README_MAX_BYTES: usize = 64 * 1024;

/// What PekoHub answers a publish with.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Published {
    pub owner: String,
    pub name: String,
    pub version: String,
    pub digest: String,
    pub web_url: String,
}

/// Publish `seed` at `version`, with an optional `readme`, as this
/// computer's person.
///
/// # Errors
/// When this computer is not signed in, the seed does not parse, the
/// README is too large, PekoHub cannot be reached, or it refuses the seed
/// (its message is passed on).
pub async fn publish_seed(
    resolver: &PathResolver,
    vault: Arc<Vault>,
    seed: &str,
    version: &str,
    readme: Option<&str>,
) -> Result<Published> {
    anyhow::ensure!(
        linked_account(resolver).await.is_some(),
        "this computer is not signed in to PekoHub, so there is no one to publish the seed \
         as. Sign in with `peko auth login`, or publish it on the PekoHub website"
    );
    crate::principal::seed::parse_seed(seed).context("this is not a seed")?;
    if let Some(readme) = readme {
        anyhow::ensure!(
            readme.len() <= README_MAX_BYTES,
            "a README is at most {} KB",
            README_MAX_BYTES / 1024
        );
    }
    let credential = crate::tunnel::PekoHubCredential::from_file(&resolver.pekohub_config())
        .context("this computer is not signed in to PekoHub")?;
    let origin =
        crate::tunnel::hub_origin(&credential.url).context("the registration names no Hub")?;
    let (identity, key) = runtime_key(resolver, vault)?;
    post(&origin, &identity.runtime_did, &key, seed, version, readme).await
}

/// `POST /v1/seeds` at `origin`, signed as the runtime `runtime_did`.
pub(crate) async fn post(
    origin: &str,
    runtime_did: &str,
    key: &ed25519_dalek::SigningKey,
    seed: &str,
    version: &str,
    readme: Option<&str>,
) -> Result<Published> {
    let path = "/v1/seeds";
    let proof = HubAgentDirectoryClient::directory_proof(runtime_did, key, path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut body = serde_json::json!({ "seed": seed, "version": version });
    if let Some(readme) = readme {
        body["readme"] = readme.into();
    }
    let response = reqwest::Client::new()
        .post(format!("{origin}{path}"))
        .header("authorization", format!("PekoRuntime {proof}"))
        .json(&body)
        .send()
        .await
        .context("could not reach PekoHub")?;
    let status = response.status();
    if !status.is_success() {
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        let message = body["error"]
            .as_str()
            .map_or_else(|| format!("HTTP {status}"), str::to_string);
        anyhow::bail!("PekoHub did not publish the seed: {message}");
    }
    response
        .json::<Published>()
        .await
        .context("PekoHub's answer to the publish was not understood")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The publish is signed as this runtime, for `/v1/seeds`, and
    /// PekoHub's refusal reaches the person in its own words.
    #[tokio::test]
    async fn a_publish_is_signed_as_the_runtime_and_a_refusal_is_passed_on() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            let answers = [
                (
                    "201 Created",
                    r#"{"owner":"maya","name":"ada","version":"1.0.0","digest":"sha256:ab","webUrl":"http://w/maya/seeds/ada"}"#,
                ),
                ("409 Conflict", r#"{"error":"Version 1.0.0 is published"}"#),
            ];
            for (status, body) in answers {
                let answer = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let (mut socket, _) = listener.accept().await.unwrap();
                // The whole request: headers, then as many bytes as
                // `content-length` says.
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let n = socket.read(&mut buffer).await.unwrap();
                    request.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&request).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let length = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(str::to_string)
                            })
                            .and_then(|l| l.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if body.len() >= length {
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                requests.push(String::from_utf8_lossy(&request).to_string());
                socket.write_all(answer.as_bytes()).await.unwrap();
            }
            requests
        });
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let seed = "name = \"ada\"\n";
        let published = post(&origin, "did:key:zrt", &key, seed, "1.0.0", Some("# Ada"))
            .await
            .unwrap();
        assert_eq!(published.owner, "maya");
        assert_eq!(published.web_url, "http://w/maya/seeds/ada");
        let refused = post(&origin, "did:key:zrt", &key, seed, "1.0.0", None)
            .await
            .unwrap_err();
        assert_eq!(
            refused.to_string(),
            "PekoHub did not publish the seed: Version 1.0.0 is published"
        );
        let requests = server.await.unwrap();
        let first = &requests[0];
        assert!(first.starts_with("POST /v1/seeds HTTP/1.1"), "{first}");
        let proof = first
            .lines()
            .find_map(|l| l.strip_prefix("authorization: PekoRuntime "))
            .expect("signed");
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        let claims: serde_json::Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(proof.trim().split('.').nth(1).unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(claims["sub"], "did:key:zrt");
        assert_eq!(claims["path"], "/v1/seeds");
        assert!(first.contains("\"readme\":\"# Ada\""), "{first}");
    }
}
