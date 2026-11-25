//! ChatGPT plan sharing: sign in with ChatGPT so OpenAI's Responses API
//! is billed to the person's ChatGPT plan instead of an API key.
//!
//! The sign-in is OAuth 2.0 authorization code with PKCE against
//! `auth.openai.com`, asking for the `chatgpt.tokens.use.direct` scope
//! (the person allows sharing their plan with this app on OpenAI's
//! consent page). The first sign-in uses OpenAI's dynamic client
//! `dynamic_agent_client`: OpenAI registers an `oaiapp_…` client for
//! this app and returns its id with the code. Later sign-ins and every
//! renewal use that id. The access token is a bearer token for
//! `https://api.openai.com/v1`; refresh tokens rotate on every renewal.
//!
//! This module is the protocol only: no storage, no listener, no
//! browser. The vault-backed token source and the sign-in flow live in
//! the root crate (`common::chatgpt_plan`).

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use rand::RngCore;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// OpenAI's OpenID configuration.
pub const DISCOVERY_URL: &str = "https://auth.openai.com/.well-known/openid-configuration";
/// The only issuer a ChatGPT sign-in is accepted from.
pub const ISSUER: &str = "https://auth.openai.com";
/// The API the tokens are for; also the base URL of models that use them.
pub const API_BASE_URL: &str = "https://api.openai.com/v1";
/// The scope that lets this app use the person's ChatGPT plan.
pub const SHARING_SCOPE: &str = "chatgpt.tokens.use.direct";
/// A token with less than this left is renewed before use, in ms.
pub const RENEW_MARGIN_MS: i64 = 60_000;

const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
const REGISTERED_CLIENT_PREFIX: &str = "oaiapp_";
const AGENT_NAME: &str = "Peko";
/// Refresh errors after which only a new sign-in helps.
const SIGNED_OUT_ERRORS: &[&str] = &[
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

/// The endpoints OpenAI's discovery document names.
#[derive(Debug, Clone, Deserialize)]
pub struct Endpoints {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
}

/// Fetch OpenAI's endpoints, refusing any not served from the issuer.
pub async fn discover(http: &reqwest::Client) -> Result<Endpoints> {
    let endpoints: Endpoints = http
        .get(DISCOVERY_URL)
        .send()
        .await
        .context("could not reach ChatGPT sign-in")?
        .error_for_status()
        .context("ChatGPT sign-in is unavailable")?
        .json()
        .await
        .context("ChatGPT sign-in returned an unreadable configuration")?;
    verify_endpoints(&endpoints)?;
    Ok(endpoints)
}

fn verify_endpoints(endpoints: &Endpoints) -> Result<()> {
    if endpoints.issuer != ISSUER {
        bail!("ChatGPT sign-in names an unexpected issuer");
    }
    let issuer = reqwest::Url::parse(ISSUER)?.origin();
    let urls = [
        Some(&endpoints.authorization_endpoint),
        Some(&endpoints.token_endpoint),
        endpoints.revocation_endpoint.as_ref(),
    ];
    for url in urls.into_iter().flatten() {
        if reqwest::Url::parse(url)?.origin() != issuer {
            bail!("ChatGPT sign-in configuration could not be verified");
        }
    }
    Ok(())
}

/// What a sign-in already knows about an account signed in before.
#[derive(Debug, Clone, Default)]
pub struct Returning {
    /// The `oaiapp_…` client OpenAI registered for this app.
    pub client_id: String,
    /// The account's email, sent as a login hint.
    pub email: Option<String>,
}

/// One pending sign-in: the URL to open and what checks the reply.
#[derive(Debug)]
pub struct SignIn {
    /// Open this in a browser.
    pub url: String,
    /// Where OpenAI sends the browser back.
    pub redirect_uri: String,
    state: String,
    nonce: String,
    verifier: String,
    client_id: Option<String>,
}

/// The authorization code a sign-in came back with.
#[derive(Debug)]
pub struct Code {
    code: String,
    client_id: String,
}

impl SignIn {
    /// Start a sign-in that returns to `redirect_uri`
    /// (`http://127.0.0.1:<port>/auth/callback`). `host_id` names this
    /// computer to OpenAI (`urn:uuid:…`, stable across sign-ins).
    #[must_use]
    pub fn new(
        endpoints: &Endpoints,
        redirect_uri: &str,
        host_id: &str,
        returning: Option<&Returning>,
    ) -> Self {
        let state = random_token(32);
        let nonce = random_token(32);
        let verifier = random_token(64);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        let client_id = returning.map(|r| r.client_id.clone());
        let mut url = reqwest::Url::parse(&endpoints.authorization_endpoint)
            .expect("discovery endpoints are verified URLs");
        {
            let mut query = url.query_pairs_mut();
            query.append_pair(
                "client_id",
                client_id.as_deref().unwrap_or(DYNAMIC_CLIENT_ID),
            );
            match returning {
                Some(r) => {
                    if let Some(email) = &r.email {
                        query.append_pair("login_hint", email);
                    }
                }
                None => {
                    query.append_pair("agent_name_hint", AGENT_NAME);
                }
            }
            query
                .append_pair("ext_agent_host_id", host_id)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("scope", SCOPES)
                .append_pair("resource", API_BASE_URL)
                .append_pair("state", &state)
                .append_pair("nonce", &nonce)
                .append_pair("code_challenge_method", "S256")
                .append_pair("code_challenge", &challenge);
        }
        Self {
            url: url.into(),
            redirect_uri: redirect_uri.to_string(),
            state,
            nonce,
            verifier,
            client_id,
        }
    }

    /// The sign-in's `state`, which its reply carries.
    #[must_use]
    pub fn state(&self) -> &str {
        &self.state
    }

    /// Read the URL the browser came back to. A reply to another sign-in
    /// (wrong path or state) is an error that leaves this one waiting.
    pub fn callback(&self, returned: &str) -> Result<Code> {
        let url = reqwest::Url::parse(returned)
            .or_else(|_| reqwest::Url::parse(&self.redirect_uri)?.join(returned))
            .context("not a sign-in URL")?;
        if url.path() != "/auth/callback" {
            bail!("not the sign-in callback");
        }
        let param = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        if param("state").as_deref() != Some(self.state.as_str()) {
            bail!("this reply belongs to another sign-in");
        }
        if let Some(error) = param("error") {
            if error == "access_denied" {
                bail!("ChatGPT sign-in was declined");
            }
            bail!("ChatGPT sign-in failed: {error}");
        }
        let code = param("code").ok_or_else(|| anyhow!("the reply has no code"))?;
        let client_id = match (&self.client_id, param("client_id")) {
            (Some(known), Some(returned)) if *known != returned => {
                bail!("ChatGPT returned a different app registration; sign in again")
            }
            (Some(known), _) => known.clone(),
            (None, Some(returned)) => returned,
            (None, None) => bail!("ChatGPT did not register this app; sign in again"),
        };
        if !client_id.starts_with(REGISTERED_CLIENT_PREFIX) {
            bail!("ChatGPT returned an unexpected app registration");
        }
        Ok(Code { code, client_id })
    }
}

/// Everything about a signed-in plan except the access token: what is
/// kept beside it to renew it. Holds the refresh token, so it is stored
/// only in the encrypted vault and never logged (`Debug` hides it).
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanSession {
    pub client_id: String,
    pub refresh_token: Option<String>,
    /// When the access token expires, Unix ms.
    pub expires_at_ms: i64,
    /// Renewing before this is refused, Unix ms.
    #[serde(default)]
    pub earliest_refresh_at_ms: Option<i64>,
    pub scopes: Vec<String>,
    /// OpenAI's stable id for the account.
    pub subject: String,
    #[serde(default)]
    pub email: Option<String>,
    pub token_endpoint: String,
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
}

impl std::fmt::Debug for PlanSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanSession")
            .field("client_id", &self.client_id)
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "…"))
            .field("expires_at_ms", &self.expires_at_ms)
            .field("earliest_refresh_at_ms", &self.earliest_refresh_at_ms)
            .field("scopes", &self.scopes)
            .field("subject", &self.subject)
            .field("email", &self.email)
            .finish_non_exhaustive()
    }
}

impl PlanSession {
    /// Whether the person allowed sharing their plan.
    #[must_use]
    pub fn sharing_enabled(&self) -> bool {
        self.scopes.iter().any(|s| s == SHARING_SCOPE)
    }

    /// The account, as people read it.
    #[must_use]
    pub fn account(&self) -> &str {
        self.email.as_deref().unwrap_or(&self.subject)
    }
}

/// A fresh access token and the session it belongs to.
#[derive(Debug)]
pub struct PlanTokens {
    pub access_token: SecretString,
    pub session: PlanSession,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    token_type: String,
    expires_in: i64,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    earliest_refresh_at: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct OAuthError {
    error: String,
}

#[derive(Deserialize)]
struct IdClaims {
    iss: String,
    sub: String,
    aud: serde_json::Value,
    exp: i64,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

/// Exchange the code a sign-in came back with for tokens.
///
/// The ID token arrives straight from the token endpoint over TLS in
/// exchange for this sign-in's code and PKCE verifier, so its issuer is
/// established by the connection (OpenID Connect Core §3.1.3.7) and
/// its signature is not checked; its issuer, audience, nonce and
/// expiry are.
pub async fn exchange(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    sign_in: &SignIn,
    code: &Code,
) -> Result<PlanTokens> {
    let response = post_form(
        http,
        &endpoints.token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", &code.client_id),
            ("code", &code.code),
            ("code_verifier", &sign_in.verifier),
            ("redirect_uri", &sign_in.redirect_uri),
            ("resource", API_BASE_URL),
        ],
    )
    .await?;
    let tokens = match response {
        Ok(tokens) => tokens,
        Err(error) if error == "invalid_grant" => bail!("this sign-in code expired; sign in again"),
        Err(error) if error == "access_denied" => bail!("ChatGPT sign-in was declined"),
        Err(error) => bail!("ChatGPT did not accept the sign-in ({error})"),
    };
    let id_token = tokens
        .id_token
        .as_deref()
        .ok_or_else(|| anyhow!("ChatGPT did not say which account signed in"))?;
    let claims = id_claims(id_token)?;
    let now = now_ms();
    let audience_ok = match &claims.aud {
        serde_json::Value::String(aud) => *aud == code.client_id,
        serde_json::Value::Array(auds) => auds.iter().any(|a| a == code.client_id.as_str()),
        _ => false,
    };
    if claims.iss != endpoints.issuer
        || !audience_ok
        || claims.nonce.as_deref() != Some(sign_in.nonce.as_str())
        || claims.exp * 1000 <= now
    {
        bail!("ChatGPT sign-in could not be verified; sign in again");
    }
    session_from(
        tokens,
        now,
        PlanSession {
            client_id: code.client_id.clone(),
            refresh_token: None,
            expires_at_ms: 0,
            earliest_refresh_at_ms: None,
            scopes: Vec::new(),
            subject: claims.sub,
            email: claims.email,
            token_endpoint: endpoints.token_endpoint.clone(),
            revocation_endpoint: endpoints.revocation_endpoint.clone(),
        },
    )
}

/// What to do with a stored token before using it.
#[derive(Debug, PartialEq, Eq)]
pub enum Renewal {
    /// Use it.
    Fresh,
    /// Renew it first.
    Renew,
    /// It has expired and cannot be renewed yet.
    Wait,
}

/// Decide whether the session's token needs renewing at `now_ms`.
#[must_use]
pub fn renewal(session: &PlanSession, now_ms: i64) -> Renewal {
    if session.expires_at_ms - now_ms > RENEW_MARGIN_MS {
        return Renewal::Fresh;
    }
    match session.earliest_refresh_at_ms {
        Some(earliest) if earliest > now_ms => {
            if session.expires_at_ms > now_ms {
                Renewal::Fresh
            } else {
                Renewal::Wait
            }
        }
        _ => Renewal::Renew,
    }
}

/// Why a renewal failed.
#[derive(Debug, thiserror::Error)]
pub enum RenewError {
    /// The sign-in ended (revoked, expired, refresh token reused); only
    /// signing in again helps.
    #[error(
        "the ChatGPT sign-in for {0} ended; sign in again with `peko credential login chatgpt`"
    )]
    SignedOut(String),
    /// A failure worth retrying later.
    #[error("could not renew the ChatGPT sign-in: {0}")]
    Transient(String),
}

/// Renew the session's access token. The refresh token rotates: the
/// returned session carries the new one, and the old one stops working.
pub async fn renew(
    http: &reqwest::Client,
    session: &PlanSession,
) -> std::result::Result<PlanTokens, RenewError> {
    let account = session.account().to_string();
    let refresh_token = session
        .refresh_token
        .as_deref()
        .ok_or_else(|| RenewError::SignedOut(account.clone()))?;
    let response = post_form(
        http,
        &session.token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &session.client_id),
            ("refresh_token", refresh_token),
            ("resource", API_BASE_URL),
        ],
    )
    .await
    .map_err(|e| RenewError::Transient(format!("{e:#}")))?;
    let tokens = match response {
        Ok(tokens) => tokens,
        Err(error) if SIGNED_OUT_ERRORS.contains(&error.as_str()) => {
            return Err(RenewError::SignedOut(account))
        }
        Err(error) => return Err(RenewError::Transient(error)),
    };
    if tokens.refresh_token.is_none() {
        return Err(RenewError::Transient(
            "ChatGPT returned a renewal without a refresh token".into(),
        ));
    }
    let renewed = session_from(tokens, now_ms(), session.clone())
        .map_err(|e| RenewError::Transient(format!("{e:#}")))?;
    if !renewed.session.sharing_enabled() {
        return Err(RenewError::SignedOut(account));
    }
    Ok(renewed)
}

/// Revoke the session's refresh token, best effort. `true` when OpenAI
/// confirmed it.
pub async fn revoke(http: &reqwest::Client, session: &PlanSession) -> bool {
    let (Some(endpoint), Some(token)) = (&session.revocation_endpoint, &session.refresh_token)
    else {
        return false;
    };
    http.post(endpoint)
        .form(&[
            ("token", token.as_str()),
            ("token_type_hint", "refresh_token"),
            ("client_id", session.client_id.as_str()),
        ])
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

/// A model the plan offers.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanModel {
    /// The wire model id.
    pub slug: String,
    pub display_name: String,
    /// What the plan's list says about it (context window, reasoning
    /// levels, …); suggestions are added by the caller.
    pub details: crate::discovery::DiscoveredModel,
}

/// The models the plan offers, as `GET /v1/models` lists them for a
/// plan token (`models[]`, keeping `visibility: "list"` ones; an
/// API-style `data[]` list is read too).
pub async fn list_models(http: &reqwest::Client, access_token: &str) -> Result<Vec<PlanModel>> {
    list_models_at(http, API_BASE_URL, access_token).await
}

async fn list_models_at(
    http: &reqwest::Client,
    base_url: &str,
    access_token: &str,
) -> Result<Vec<PlanModel>> {
    let body: serde_json::Value = http
        .get(format!("{base_url}/models"))
        .bearer_auth(access_token)
        .send()
        .await
        .context("could not list the plan's models")?
        .error_for_status()
        .context("could not list the plan's models")?
        .json()
        .await
        .context("the plan's model list was unreadable")?;
    Ok(parse_models(&body))
}

fn parse_models(body: &serde_json::Value) -> Vec<PlanModel> {
    crate::discovery::parse_list(body)
        .into_iter()
        .map(|details| PlanModel {
            slug: details.id.clone(),
            display_name: details
                .display_name
                .clone()
                .unwrap_or_else(|| details.id.clone()),
            details,
        })
        .collect()
}

/// POST a token-endpoint form. `Ok(Err(code))` is an OAuth error reply.
async fn post_form(
    http: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<std::result::Result<TokenResponse, String>> {
    let response = http
        .post(url)
        .header("accept", "application/json")
        .form(form)
        .send()
        .await
        .context("ChatGPT sign-in is unavailable")?;
    let ok = response.status().is_success();
    let status = response.status();
    let body = response
        .bytes()
        .await
        .context("ChatGPT sign-in is unavailable")?;
    if ok {
        return Ok(Ok(
            serde_json::from_slice(&body).context("ChatGPT returned an unreadable token reply")?
        ));
    }
    Ok(Err(serde_json::from_slice::<OAuthError>(&body)
        .map(|e| e.error)
        .unwrap_or_else(|_| format!("HTTP {status}"))))
}

fn session_from(tokens: TokenResponse, now: i64, base: PlanSession) -> Result<PlanTokens> {
    if !tokens.token_type.eq_ignore_ascii_case("bearer") {
        bail!("ChatGPT returned an unsupported token type");
    }
    let earliest = tokens.earliest_refresh_at.as_ref().and_then(|v| match v {
        serde_json::Value::Number(n) => n.as_f64().map(|s| (s * 1000.0) as i64),
        serde_json::Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.timestamp_millis()),
        _ => None,
    });
    Ok(PlanTokens {
        access_token: SecretString::from(tokens.access_token),
        session: PlanSession {
            refresh_token: tokens.refresh_token.or(base.refresh_token),
            expires_at_ms: now + tokens.expires_in.max(0) * 1000,
            earliest_refresh_at_ms: earliest,
            // A reply without `scope` keeps the scopes granted before.
            scopes: if tokens.scope.trim().is_empty() {
                base.scopes.clone()
            } else {
                tokens
                    .scope
                    .split_whitespace()
                    .map(str::to_string)
                    .collect()
            },
            ..base
        },
    })
}

fn id_claims(id_token: &str) -> Result<IdClaims> {
    let payload = id_token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("ChatGPT returned a malformed identity"))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .context("ChatGPT returned a malformed identity")?;
    serde_json::from_slice(&bytes).context("ChatGPT returned a malformed identity")
}

fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// The current time, Unix ms.
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serve one HTTP reply on a local port; the handle yields the request.
    async fn serve_once(status: u16, body: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            request
        });
        (url, handle)
    }

    fn endpoints(token_endpoint: &str) -> Endpoints {
        Endpoints {
            issuer: ISSUER.into(),
            authorization_endpoint: "https://auth.openai.com/oauth/authorize".into(),
            token_endpoint: token_endpoint.into(),
            revocation_endpoint: None,
        }
    }

    fn id_token(claims: &serde_json::Value) -> String {
        let enc = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!(
            "{}.{}.sig",
            enc(br#"{"alg":"RS256"}"#),
            enc(claims.to_string().as_bytes())
        )
    }

    fn query(url: &str) -> std::collections::HashMap<String, String> {
        reqwest::Url::parse(url)
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    fn session(expires_at_ms: i64, earliest: Option<i64>) -> PlanSession {
        PlanSession {
            client_id: "oaiapp_1".into(),
            refresh_token: Some("rt-1".into()),
            expires_at_ms,
            earliest_refresh_at_ms: earliest,
            scopes: vec![SHARING_SCOPE.into()],
            subject: "user-1".into(),
            email: Some("a@example.com".into()),
            token_endpoint: String::new(),
            revocation_endpoint: None,
        }
    }

    #[test]
    fn discovery_endpoints_must_come_from_the_issuer() {
        assert!(verify_endpoints(&endpoints("https://auth.openai.com/oauth/token")).is_ok());
        assert!(verify_endpoints(&endpoints("https://evil.example/token")).is_err());
        let mut other = endpoints("https://auth.openai.com/oauth/token");
        other.issuer = "https://evil.example".into();
        assert!(verify_endpoints(&other).is_err());
    }

    #[test]
    fn first_sign_in_asks_the_dynamic_client_to_share_the_plan() {
        let sign_in = SignIn::new(
            &endpoints("https://auth.openai.com/oauth/token"),
            "http://127.0.0.1:4242/auth/callback",
            "urn:uuid:host",
            None,
        );
        let q = query(&sign_in.url);
        assert_eq!(q["client_id"], DYNAMIC_CLIENT_ID);
        assert_eq!(q["agent_name_hint"], "Peko");
        assert_eq!(q["ext_agent_host_id"], "urn:uuid:host");
        assert!(q["scope"].split(' ').any(|s| s == SHARING_SCOPE));
        assert_eq!(q["resource"], API_BASE_URL);
        assert_eq!(q["code_challenge_method"], "S256");
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(sign_in.verifier.as_bytes()));
        assert_eq!(q["code_challenge"], challenge);
        assert!(!q.contains_key("login_hint"));
    }

    #[test]
    fn a_returning_account_reuses_its_registration() {
        let returning = Returning {
            client_id: "oaiapp_9".into(),
            email: Some("a@example.com".into()),
        };
        let sign_in = SignIn::new(
            &endpoints("https://auth.openai.com/oauth/token"),
            "http://127.0.0.1:1/auth/callback",
            "urn:uuid:host",
            Some(&returning),
        );
        let q = query(&sign_in.url);
        assert_eq!(q["client_id"], "oaiapp_9");
        assert_eq!(q["login_hint"], "a@example.com");
        assert!(!q.contains_key("agent_name_hint"));
        let base = "http://127.0.0.1:1/auth/callback";
        let ok = format!("{base}?code=c&state={}", sign_in.state);
        assert_eq!(sign_in.callback(&ok).unwrap().client_id, "oaiapp_9");
        let other = format!("{base}?code=c&state={}&client_id=oaiapp_8", sign_in.state);
        assert!(sign_in.callback(&other).is_err());
    }

    #[test]
    fn callback_checks_state_path_errors_and_registration() {
        let sign_in = SignIn::new(
            &endpoints("https://auth.openai.com/oauth/token"),
            "http://127.0.0.1:1/auth/callback",
            "urn:uuid:host",
            None,
        );
        let base = "http://127.0.0.1:1/auth/callback";
        let state = &sign_in.state;
        let code = sign_in
            .callback(&format!("{base}?code=c1&state={state}&client_id=oaiapp_7"))
            .unwrap();
        assert_eq!(
            (code.code.as_str(), code.client_id.as_str()),
            ("c1", "oaiapp_7")
        );
        // A pasted path-only URL is read against the redirect URI.
        assert!(sign_in
            .callback(&format!(
                "/auth/callback?code=c1&state={state}&client_id=oaiapp_7"
            ))
            .is_ok());
        for bad in [
            format!("{base}?code=c1&state=other&client_id=oaiapp_7"),
            format!("http://127.0.0.1:1/favicon.ico?state={state}"),
            format!("{base}?error=access_denied&state={state}"),
            format!("{base}?code=c1&state={state}"),
            format!("{base}?code=c1&state={state}&client_id=someone_else"),
        ] {
            assert!(sign_in.callback(&bad).is_err(), "accepted {bad}");
        }
    }

    #[tokio::test]
    async fn exchange_checks_the_identity_and_keeps_the_session() {
        let pending = SignIn::new(
            &endpoints("https://auth.openai.com/oauth/token"),
            "http://127.0.0.1:1/auth/callback",
            "urn:uuid:host",
            None,
        );
        let exp = now_ms() / 1000 + 3600;
        let token = |nonce: &str| {
            serde_json::json!({
                "access_token": "at-1",
                "refresh_token": "rt-1",
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": format!("openid {SHARING_SCOPE}"),
                "earliest_refresh_at": 1_900_000_000,
                "id_token": id_token(&serde_json::json!({
                    "iss": ISSUER, "sub": "user-1", "aud": ["oaiapp_7"],
                    "exp": exp, "nonce": nonce, "email": "a@example.com"
                })),
            })
            .to_string()
        };
        let code = Code {
            code: "c1".into(),
            client_id: "oaiapp_7".into(),
        };
        let http = reqwest::Client::new();

        let (url, request) = serve_once(200, token(&pending.nonce)).await;
        let tokens = exchange(&http, &endpoints(&format!("{url}/token")), &pending, &code)
            .await
            .unwrap();
        let request = request.await.unwrap();
        assert!(
            request.contains("grant_type=authorization_code"),
            "{request}"
        );
        assert!(request.contains(&format!("code_verifier={}", pending.verifier)));
        use secrecy::ExposeSecret;
        assert_eq!(tokens.access_token.expose_secret(), "at-1");
        assert_eq!(tokens.session.subject, "user-1");
        assert_eq!(tokens.session.account(), "a@example.com");
        assert_eq!(tokens.session.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(
            tokens.session.earliest_refresh_at_ms,
            Some(1_900_000_000_000)
        );
        assert!(tokens.session.sharing_enabled());
        assert!(!format!("{:?}", tokens.session).contains("rt-1"));

        let (url, _) = serve_once(200, token("another-nonce")).await;
        assert!(
            exchange(&http, &endpoints(&format!("{url}/token")), &pending, &code)
                .await
                .is_err()
        );
    }

    #[test]
    fn renewal_respects_the_margin_and_earliest_refresh() {
        let now = 1_000_000;
        assert_eq!(renewal(&session(now + 120_000, None), now), Renewal::Fresh);
        assert_eq!(renewal(&session(now + 30_000, None), now), Renewal::Renew);
        assert_eq!(renewal(&session(now - 1, None), now), Renewal::Renew);
        assert_eq!(
            renewal(&session(now + 30_000, Some(now + 10_000)), now),
            Renewal::Fresh
        );
        assert_eq!(
            renewal(&session(now - 1, Some(now + 10_000)), now),
            Renewal::Wait
        );
    }

    #[tokio::test]
    async fn renew_rotates_the_refresh_token_and_spots_a_signed_out_session() {
        let http = reqwest::Client::new();
        let (url, request) = serve_once(
            200,
            serde_json::json!({
                "access_token": "at-2", "refresh_token": "rt-2", "token_type": "bearer",
                "expires_in": 600, "scope": SHARING_SCOPE
            })
            .to_string(),
        )
        .await;
        let mut old = session(0, None);
        old.token_endpoint = format!("{url}/token");
        let renewed = renew(&http, &old).await.unwrap();
        let request = request.await.unwrap();
        assert!(
            request.contains("grant_type=refresh_token") && request.contains("refresh_token=rt-1")
        );
        assert_eq!(renewed.session.refresh_token.as_deref(), Some("rt-2"));
        assert_eq!(renewed.session.subject, "user-1");
        assert!(renewed.session.expires_at_ms > now_ms());

        let (url, _) = serve_once(400, r#"{"error":"refresh_token_reused"}"#.into()).await;
        old.token_endpoint = format!("{url}/token");
        assert!(matches!(
            renew(&http, &old).await,
            Err(RenewError::SignedOut(_))
        ));

        let (url, _) = serve_once(503, "{}".into()).await;
        old.token_endpoint = format!("{url}/token");
        assert!(matches!(
            renew(&http, &old).await,
            Err(RenewError::Transient(_))
        ));

        // Sharing turned off on OpenAI's side ends the sign-in too.
        let (url, _) = serve_once(
            200,
            serde_json::json!({
                "access_token": "at-3", "refresh_token": "rt-3", "token_type": "bearer",
                "expires_in": 600, "scope": "openid"
            })
            .to_string(),
        )
        .await;
        old.token_endpoint = format!("{url}/token");
        assert!(matches!(
            renew(&http, &old).await,
            Err(RenewError::SignedOut(_))
        ));
    }

    #[tokio::test]
    async fn models_are_read_from_the_plan_catalog() {
        let (url, request) = serve_once(
            200,
            serde_json::json!({"models": [
                {"slug": "gpt-5.5", "display_name": "GPT-5.5", "visibility": "list",
                 "context_window": 272000, "supported_reasoning_levels": [{"effort": "low"}, {"effort": "high"}]},
                {"slug": "internal", "display_name": "Hidden", "visibility": "hide"}
            ]})
            .to_string(),
        )
        .await;
        let models = list_models_at(&reqwest::Client::new(), &url, "at-1")
            .await
            .unwrap();
        assert!(request
            .await
            .unwrap()
            .contains("authorization: Bearer at-1"));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "gpt-5.5");
        assert_eq!(models[0].display_name, "GPT-5.5");
        assert_eq!(models[0].details.reported.context_window, Some(272_000));
        assert_eq!(
            models[0]
                .details
                .reported
                .thinking
                .as_ref()
                .unwrap()
                .efforts,
            ["low", "high"]
        );
        let api_style = serde_json::json!({"data": [{"id": "gpt-4o"}]});
        assert_eq!(parse_models(&api_style)[0].slug, "gpt-4o");
    }
}
