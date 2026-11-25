//! A ChatGPT plan as a model credential (OpenAI plan sharing).
//!
//! `peko credential login chatgpt` signs in with ChatGPT in the browser
//! (`peko_providers::chatgpt` has the protocol) and stores the result as
//! one vault credential: namespace `llm`, kind `oauth_token`, material
//! the current access token, and metadata `{ "oauth": "chatgpt_plan",
//! "host_id": …, "session": PlanSession }` — the session holds the
//! refresh token, so it lives only in the encrypted vault. A model whose
//! `credential_id` names it (`api_format = "openai_responses"`,
//! `base_url = "https://api.openai.com/v1"`) is billed to the plan.
//!
//! The vault's credential provider hands such a credential out as a
//! [`PlanTokenSource`]: every request asks it for a token, and it renews
//! the token shortly before it expires, writing the rotated refresh
//! token back. Renewals of one credential are serialized in this
//! process, and the vault is re-read first, so a token another process
//! just renewed is used rather than renewed again (a reused refresh
//! token ends the sign-in).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use peko_provider_api::{CredentialError, TokenFuture, TokenSource};
use peko_providers::chatgpt::{self, PlanSession, PlanTokens, RenewError, Renewal, Returning};
use secrecy::{ExposeSecret, SecretString};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::common::vault::{Credential, CredentialKind, Vault};

/// Namespace plan credentials are stored under, beside API keys.
pub const NAMESPACE: &str = "llm";
/// `metadata.oauth` of a plan credential.
const MARK: &str = "chatgpt_plan";
/// How long a sign-in waits for the browser.
const SIGN_IN_TIMEOUT: Duration = Duration::from_mins(30);

/// The plan session stored with `credential`, if it is a plan credential.
#[must_use]
pub fn plan_session(credential: &Credential) -> Option<PlanSession> {
    if credential.kind != CredentialKind::OAuthToken
        || credential.metadata.get("oauth").and_then(|v| v.as_str()) != Some(MARK)
    {
        return None;
    }
    serde_json::from_value(credential.metadata.get("session")?.clone()).ok()
}

/// Whether `credential` is a signed-in ChatGPT plan.
#[must_use]
pub fn is_plan_credential(credential: &Credential) -> bool {
    plan_session(credential).is_some()
}

/// `metadata` with every refresh or ID token masked, for display.
#[must_use]
pub fn shown_metadata(metadata: &serde_json::Value) -> serde_json::Value {
    match metadata {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| {
                let v = if matches!(k.as_str(), "refresh_token" | "id_token") && !v.is_null() {
                    serde_json::Value::String("(hidden)".into())
                } else {
                    shown_metadata(v)
                };
                (k.clone(), v)
            })
            .collect::<serde_json::Map<_, _>>()
            .into(),
        serde_json::Value::Array(items) => items.iter().map(shown_metadata).collect(),
        other => other.clone(),
    }
}

/// The plan credential stored in slot `name`, if any.
#[must_use]
pub fn find_plan_credential(vault: &Vault, name: &str) -> Option<Credential> {
    vault
        .list_credentials(&crate::common::vault::CredentialFilter {
            namespace: Some(NAMESPACE.to_string()),
            ..Default::default()
        })
        .into_iter()
        .filter(|s| s.name == name)
        .filter_map(|s| vault.get_credential(&s.id))
        .find(is_plan_credential)
}

/// Store a signed-in plan in slot `name`, replacing the plan credential
/// already there (its id, and so every model that uses it, is kept).
/// Returns the credential id.
pub fn store_plan(vault: &Vault, name: &str, tokens: &PlanTokens, host_id: &str) -> Result<String> {
    let mut credential = Credential::now(
        NAMESPACE.to_string(),
        name.to_string(),
        CredentialKind::OAuthToken,
        tokens.access_token.clone(),
    );
    if let Some(existing) = find_plan_credential(vault, name) {
        credential.id = existing.id;
    }
    credential.metadata = serde_json::json!({
        "oauth": MARK,
        "host_id": host_id,
        "account": tokens.session.account(),
        "session": tokens.session,
    });
    vault.set_credential(&credential)?;
    Ok(credential.id)
}

/// What a new sign-in for slot `name` reuses: the computer's host id,
/// and the app registration of the account signed in there before.
#[must_use]
pub fn previous_sign_in(vault: &Vault, name: &str) -> (String, Option<Returning>) {
    let existing = find_plan_credential(vault, name);
    let host_id = existing
        .as_ref()
        .and_then(|c| c.metadata.get("host_id")?.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("urn:uuid:{}", uuid::Uuid::new_v4()));
    let returning = existing.as_ref().and_then(plan_session).map(|s| Returning {
        client_id: s.client_id,
        email: s.email,
    });
    (host_id, returning)
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default()
}

/// One renewal lock per credential, shared by every provider in this
/// process: two runs must not spend the same refresh token.
fn renewal_lock(id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.entry(id.to_string()).or_default().clone()
}

/// Hands out the plan credential's access token, renewing it when it
/// is about to expire.
pub struct PlanTokenSource {
    vault: Arc<Vault>,
    id: String,
    http: reqwest::Client,
}

impl PlanTokenSource {
    #[must_use]
    pub fn new(vault: Arc<Vault>, id: impl Into<String>) -> Self {
        Self {
            vault,
            id: id.into(),
            http: http(),
        }
    }

    fn read(&self) -> Result<(SecretString, PlanSession), String> {
        let credential = self
            .vault
            .get_credential(&self.id)
            .ok_or_else(|| "the ChatGPT sign-in was removed; sign in again with `peko credential login chatgpt`".to_string())?;
        let session = plan_session(&credential)
            .ok_or_else(|| "the credential is no longer a ChatGPT sign-in".to_string())?;
        Ok((credential.material, session))
    }

    async fn current(&self) -> Result<SecretString, String> {
        let (token, session) = self.read()?;
        if chatgpt::renewal(&session, chatgpt::now_ms()) == Renewal::Fresh {
            return Ok(token);
        }
        let lock = renewal_lock(&self.id);
        let _guard = lock.lock().await;
        // Another run, or another process, may have renewed it meanwhile.
        if let Err(e) = self.vault.reload_unlocked() {
            tracing::debug!("vault re-read before renewal failed: {e:#}");
        }
        let (token, session) = self.read()?;
        match chatgpt::renewal(&session, chatgpt::now_ms()) {
            Renewal::Fresh => Ok(token),
            Renewal::Wait => Err(format!(
                "the ChatGPT sign-in for {} cannot be renewed yet; try again shortly",
                session.account()
            )),
            Renewal::Renew => match chatgpt::renew(&self.http, &session).await {
                Ok(renewed) => {
                    let mut credential = self
                        .vault
                        .get_credential(&self.id)
                        .ok_or_else(|| "the ChatGPT sign-in was removed".to_string())?;
                    credential.material = renewed.access_token.clone();
                    credential.metadata["session"] =
                        serde_json::to_value(&renewed.session).map_err(|e| e.to_string())?;
                    // The old refresh token is spent: failing to store
                    // the new one would end the sign-in on the next renewal.
                    self.vault.set_credential(&credential).map_err(|e| {
                        format!("could not store the renewed ChatGPT sign-in: {e:#}")
                    })?;
                    tracing::info!(credential = %self.id, "renewed ChatGPT plan token");
                    Ok(renewed.access_token)
                }
                // Still usable for a little while: use it and retry the
                // renewal on a later request.
                Err(RenewError::Transient(e)) if session.expires_at_ms > chatgpt::now_ms() => {
                    tracing::warn!(credential = %self.id, "ChatGPT plan renewal failed, token still valid: {e}");
                    Ok(token)
                }
                Err(e) => Err(e.to_string()),
            },
        }
    }
}

impl TokenSource for PlanTokenSource {
    fn token(&self) -> TokenFuture<'_> {
        Box::pin(async move { self.current().await.map_err(CredentialError::Backend) })
    }
}

/// A sign-in waiting for the browser to come back.
pub struct PendingSignIn {
    listener: tokio::net::TcpListener,
    endpoints: chatgpt::Endpoints,
    sign_in: chatgpt::SignIn,
    http: reqwest::Client,
}

impl PendingSignIn {
    /// Discover OpenAI's endpoints and listen for the browser's return
    /// on a free loopback port.
    pub async fn start(host_id: &str, returning: Option<&Returning>) -> Result<Self> {
        let http = http();
        let endpoints = chatgpt::discover(&http).await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("could not listen for the sign-in reply")?;
        let port = listener.local_addr()?.port();
        let redirect = format!("http://127.0.0.1:{port}/auth/callback");
        let sign_in = chatgpt::SignIn::new(&endpoints, &redirect, host_id, returning);
        Ok(Self {
            listener,
            endpoints,
            sign_in,
            http,
        })
    }

    /// The URL to open in a browser.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.sign_in.url
    }

    /// Wait for the browser to come back to the loopback port, or for
    /// `pasted` to deliver the URL it ended on (a browser on another
    /// computer), then exchange the code for tokens.
    pub async fn finish(
        self,
        mut pasted: tokio::sync::mpsc::Receiver<String>,
    ) -> Result<PlanTokens> {
        let code = tokio::time::timeout(SIGN_IN_TIMEOUT, async {
            loop {
                tokio::select! {
                    accepted = self.listener.accept() => {
                        let Ok((mut socket, _)) = accepted else { continue };
                        let Some(target) = read_request_target(&mut socket).await else { continue };
                        match self.sign_in.callback(&target) {
                            Ok(code) => {
                                reply(&mut socket, 200, "Signed in to ChatGPT. You can close this tab and go back to Peko.").await;
                                break Ok(code);
                            }
                            Err(e) => {
                                // Not this sign-in's reply (a favicon, a stale tab): keep waiting,
                                // unless OpenAI reported a failure for this one.
                                let ours = target.contains(&format!("state={}", self.sign_in.state()));
                                reply(&mut socket, if ours { 400 } else { 404 }, &format!("{e}")).await;
                                if ours {
                                    break Err(e);
                                }
                            }
                        }
                    }
                    Some(line) = pasted.recv() => {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        break self.sign_in.callback(line);
                    }
                }
            }
        })
        .await
        .map_err(|_| anyhow!("the sign-in timed out; run the command again"))??;
        let tokens = chatgpt::exchange(&self.http, &self.endpoints, &self.sign_in, &code).await?;
        if !tokens.session.sharing_enabled() {
            bail!(
                "signed in as {}, but sharing your ChatGPT plan was not allowed; sign in again and allow it",
                tokens.session.account()
            );
        }
        Ok(tokens)
    }
}

/// Read an HTTP request's target (`/auth/callback?…`) from `socket`.
async fn read_request_target(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = vec![0u8; 8 * 1024];
    let n = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf))
        .await
        .ok()?
        .ok()?;
    let head = std::str::from_utf8(&buf[..n]).ok()?;
    let mut parts = head.lines().next()?.split_whitespace();
    (parts.next()? == "GET").then_some(())?;
    parts.next().map(str::to_string)
}

async fn reply(socket: &mut tokio::net::TcpStream, status: u16, text: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Peko</title><p style=\"font:16px system-ui;margin:3em\">{}</p>",
        text.replace('&', "&amp;").replace('<', "&lt;")
    );
    let response = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\nreferrer-policy: no-referrer\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        if status == 200 { "OK" } else { "Bad Request" },
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
}

/// A sign-in the engine runs for a client (Peko Desktop), by flow id.
#[derive(Debug, Clone, PartialEq)]
pub enum FlowState {
    /// Waiting for the browser or a pasted address.
    Waiting,
    /// Signed in and stored.
    Done(SignedIn),
    /// Failed, with a message for people.
    Failed(String),
    /// Cancelled, or not known (ended long ago, or the engine restarted).
    Gone,
}

/// What a finished sign-in stored.
#[derive(Debug, Clone, PartialEq)]
pub struct SignedIn {
    pub credential_id: String,
    pub account: String,
    /// The plan's models; empty when the list could not be read.
    pub models: Vec<chatgpt::PlanModel>,
}

struct Flow {
    pasted: tokio::sync::mpsc::Sender<String>,
    state: Arc<Mutex<FlowState>>,
    task: tokio::task::AbortHandle,
    started: std::time::Instant,
}

/// How long a finished sign-in's result is kept for a client to read.
const FLOW_KEPT: Duration = Duration::from_mins(40);

/// The sign-ins the engine is running for clients. One at a time: a new
/// one cancels any still waiting.
#[derive(Default)]
pub struct SignInFlows {
    flows: Mutex<HashMap<String, Flow>>,
}

/// The engine's sign-in flows.
pub fn sign_in_flows() -> &'static SignInFlows {
    static FLOWS: OnceLock<SignInFlows> = OnceLock::new();
    FLOWS.get_or_init(SignInFlows::default)
}

impl SignInFlows {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Flow>> {
        self.flows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Start a sign-in that stores into `vault` slot `name`. Returns
    /// `(flow_id, url)`; the browser should open `url`.
    pub async fn start(&self, vault: Arc<Vault>, name: &str) -> Result<(String, String)> {
        let (host_id, returning) = previous_sign_in(&vault, name);
        let pending = PendingSignIn::start(&host_id, returning.as_ref()).await?;
        let url = pending.url().to_string();
        let flow_id = uuid::Uuid::new_v4().to_string();
        let (pasted, rx) = tokio::sync::mpsc::channel(4);
        let state = Arc::new(Mutex::new(FlowState::Waiting));
        let result = state.clone();
        let name = name.to_string();
        let task = tokio::spawn(async move {
            let outcome = async {
                let tokens = pending.finish(rx).await?;
                let credential_id = store_plan(&vault, &name, &tokens, &host_id)
                    .context("could not store the ChatGPT sign-in")?;
                let mut models = chatgpt::list_models(&http(), tokens.access_token.expose_secret())
                    .await
                    .unwrap_or_default();
                // Tests never ask models.dev.
                let source =
                    peko_providers::discovery::suggestions_source().filter(|_| !cfg!(test));
                if let Some(source) = source {
                    if let Some(catalog) = peko_providers::discovery::suggestions(&source).await {
                        for m in &mut models {
                            m.details.suggested = catalog.suggest(chatgpt::API_BASE_URL, &m.slug);
                        }
                    }
                }
                anyhow::Ok(SignedIn {
                    credential_id,
                    account: tokens.session.account().to_string(),
                    models,
                })
            }
            .await;
            let state = match outcome {
                Ok(signed_in) => FlowState::Done(signed_in),
                Err(e) => FlowState::Failed(format!("{e:#}")),
            };
            *result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = state;
        })
        .abort_handle();
        let mut flows = self.lock();
        flows.retain(|_, flow| {
            let waiting = matches!(
                *flow
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                FlowState::Waiting
            );
            if waiting {
                flow.task.abort();
            }
            !waiting && flow.started.elapsed() < FLOW_KEPT
        });
        flows.insert(
            flow_id.clone(),
            Flow {
                pasted,
                state,
                task,
                started: std::time::Instant::now(),
            },
        );
        Ok((flow_id, url))
    }

    /// Where the sign-in `flow_id` stands. `pasted` hands a waiting one
    /// the address a browser on another computer ended on.
    pub fn status(&self, flow_id: &str, pasted: Option<String>) -> FlowState {
        let flows = self.lock();
        let Some(flow) = flows.get(flow_id) else {
            return FlowState::Gone;
        };
        let state = flow
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let (FlowState::Waiting, Some(pasted)) = (&state, pasted) {
            let _ = flow.pasted.try_send(pasted);
        }
        state
    }

    /// Stop waiting for the sign-in `flow_id`.
    pub fn cancel(&self, flow_id: &str) -> FlowState {
        if let Some(flow) = self.lock().remove(flow_id) {
            flow.task.abort();
        }
        FlowState::Gone
    }
}

/// Sign the plan credential `id` out: revoke its refresh token with
/// OpenAI (best effort). The caller deletes the credential.
pub async fn revoke(credential: &Credential) -> bool {
    match plan_session(credential) {
        Some(session) => chatgpt::revoke(&http(), &session).await,
        None => false,
    }
}

/// The access token of a credential for an immediate call (listing
/// models right after sign-in).
#[must_use]
pub fn access_token(credential: &Credential) -> &str {
    credential.material.expose_secret()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(access: &str, refresh: &str, expires_in_ms: i64, token_endpoint: &str) -> PlanTokens {
        PlanTokens {
            access_token: SecretString::from(access.to_string()),
            session: PlanSession {
                client_id: "oaiapp_1".into(),
                refresh_token: Some(refresh.into()),
                expires_at_ms: chatgpt::now_ms() + expires_in_ms,
                earliest_refresh_at_ms: None,
                scopes: vec![chatgpt::SHARING_SCOPE.into()],
                subject: "user-1".into(),
                email: Some("a@example.com".into()),
                token_endpoint: token_endpoint.into(),
                revocation_endpoint: None,
            },
        }
    }

    fn vault() -> (tempfile::TempDir, Arc<Vault>) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::for_test(dir.path(), "test-passphrase"));
        (dir, vault)
    }

    /// Answer every request on a local port with `body`, counting them.
    async fn token_server(body: String) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = vec![0u8; 16 * 1024];
                let _ = socket.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (url, hits)
    }

    #[test]
    fn a_stored_plan_is_found_again_and_keeps_its_id() {
        let (_dir, vault) = vault();
        let first = store_plan(
            &vault,
            "chatgpt",
            &tokens("at-1", "rt-1", 3_600_000, ""),
            "urn:uuid:h",
        )
        .unwrap();
        let again = store_plan(
            &vault,
            "chatgpt",
            &tokens("at-2", "rt-2", 3_600_000, ""),
            "urn:uuid:h",
        )
        .unwrap();
        assert_eq!(first, again);
        let credential = vault.get_credential(&first).unwrap();
        assert!(is_plan_credential(&credential));
        assert_eq!(access_token(&credential), "at-2");
        let (host, returning) = previous_sign_in(&vault, "chatgpt");
        assert_eq!(host, "urn:uuid:h");
        assert_eq!(returning.unwrap().client_id, "oaiapp_1");
        // An ordinary API key is not a plan.
        let key = Credential::now(
            "llm",
            "openai",
            CredentialKind::ApiKey,
            SecretString::from("sk-x".to_string()),
        );
        assert!(!is_plan_credential(&key));
    }

    #[test]
    fn shown_metadata_hides_refresh_and_id_tokens() {
        let shown = shown_metadata(&serde_json::json!({
            "oauth": MARK,
            "session": {"refresh_token": "rt-1", "client_id": "oaiapp_1", "nested": [{"id_token": "x"}]}
        }));
        let text = shown.to_string();
        assert!(!text.contains("rt-1") && !text.contains("\"x\""), "{text}");
        assert!(text.contains("oaiapp_1"));
    }

    #[tokio::test]
    async fn a_fresh_token_is_used_without_renewing() {
        let (_dir, vault) = vault();
        let (url, hits) = token_server("{}".into()).await;
        let id = store_plan(
            &vault,
            "chatgpt",
            &tokens("at-1", "rt-1", 3_600_000, &url),
            "h",
        )
        .unwrap();
        let token = PlanTokenSource::new(vault, id).token().await.unwrap();
        assert_eq!(token.expose_secret(), "at-1");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn concurrent_requests_renew_an_expiring_token_once_and_store_it() {
        let (_dir, vault) = vault();
        let (url, hits) = token_server(
            serde_json::json!({
                "access_token": "at-2", "refresh_token": "rt-2", "token_type": "Bearer",
                "expires_in": 3600, "scope": chatgpt::SHARING_SCOPE
            })
            .to_string(),
        )
        .await;
        let id = store_plan(
            &vault,
            "chatgpt",
            &tokens("at-1", "rt-1", 10_000, &url),
            "h",
        )
        .unwrap();
        let a = PlanTokenSource::new(vault.clone(), id.clone());
        let b = PlanTokenSource::new(vault.clone(), id.clone());
        let (ta, tb) = tokio::join!(a.token(), b.token());
        assert_eq!(ta.unwrap().expose_secret(), "at-2");
        assert_eq!(tb.unwrap().expose_secret(), "at-2");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        let stored = plan_session(&vault.get_credential(&id).unwrap()).unwrap();
        assert_eq!(stored.refresh_token.as_deref(), Some("rt-2"));
    }

    #[tokio::test]
    async fn an_ended_sign_in_says_to_sign_in_again() {
        let (_dir, vault) = vault();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let body = r#"{"error":"invalid_grant"}"#;
            let _ = socket
                .write_all(
                    format!(
                        "HTTP/1.1 400 X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        });
        let id = store_plan(&vault, "chatgpt", &tokens("at-1", "rt-1", -1, &url), "h").unwrap();
        let err = PlanTokenSource::new(vault, id).token().await.unwrap_err();
        assert!(
            err.to_string().contains("peko credential login chatgpt"),
            "{err}"
        );
    }

    /// A model whose credential is a plan is built with the renewing
    /// token source; one with an API key is not.
    #[tokio::test]
    async fn the_resolver_builds_plan_models_on_the_token_source() {
        use peko_providers::{ApiFormat, LlmResolver, ModelCatalog, ModelConfig};
        let (dir, vault) = vault();
        let plan = store_plan(
            &vault,
            "chatgpt",
            &tokens("at-1", "rt-1", 3_600_000, ""),
            "h",
        )
        .unwrap();
        let key = Credential::now(
            "llm",
            "openai",
            CredentialKind::ApiKey,
            SecretString::from("sk-x".to_string()),
        );
        vault.set_credential(&key).unwrap();
        let catalog = ModelCatalog::load_or_init(&dir.path().join("models.toml"))
            .await
            .unwrap();
        let resolver = LlmResolver::new(
            catalog,
            Arc::new(peko_providers::InMemorySecretStore::default()),
        )
        .with_credential_provider(Arc::new(
            crate::common::vault_credential_provider::VaultCredentialProvider::new(vault),
        ));
        let mut config = ModelConfig::new(
            "gpt-plan",
            ApiFormat::OpenAiResponses,
            chatgpt::API_BASE_URL,
            "gpt-5.5",
        );
        config.credential_id = Some(plan);
        assert!(resolver.token_source(&config).is_some());
        assert!(resolver.build_provider(&config).await.is_ok());
        config.credential_id = Some(key.id);
        assert!(resolver.token_source(&config).is_none());
    }
}
