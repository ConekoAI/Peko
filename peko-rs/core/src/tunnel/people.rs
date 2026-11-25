//! People's names (ADR-124).
//!
//! A person has a display name they choose and, once signed in to
//! PekoHub, a handle that is theirs alone. PekoHub keeps both: this
//! runtime sets its person's name there (`PUT /v1/people/me/name`, signed
//! with the runtime's key, which PekoHub takes as the account it is
//! registered to) and asks it for the current names and handles of the
//! people it shares conversations with (`POST /v1/people/names`). Before
//! signing in, the person's name is the one they chose here, else their
//! login name on this computer; neither leaves the computer.

use std::sync::Arc;

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use peko_channel::{ChannelPort, ChannelStore};
use peko_identity::runtime_metadata::HostInfo;
use serde::Deserialize;

use crate::common::paths::PathResolver;
use crate::common::vault::Vault;
use crate::tunnel::hub_directory::HubAgentDirectoryClient;
use crate::tunnel::registration::{actor_store, linked_account, runtime_key};

/// The longest name, in characters (PekoHub's limit too).
pub const NAME_MAX: usize = 64;

/// The most accounts one `POST /v1/people/names` asks about.
const NAMES_PER_ASK: usize = 200;

/// What this computer's person is called, and their handle once signed in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Person {
    pub name: Option<String>,
    pub handle: Option<String>,
}

/// A name as it is kept: whitespace runs folded to one space, trimmed;
/// `None` when nothing is left, it is longer than [`NAME_MAX`], or it
/// carries control or formatting characters (which could hide or
/// reorder what others read).
#[must_use]
pub fn clean_name(raw: &str) -> Option<String> {
    let name = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let hidden = |c: char| {
        c.is_control()
            || matches!(c,
                '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}')
    };
    (!name.is_empty() && name.chars().count() <= NAME_MAX && !name.chars().any(hidden))
        .then_some(name)
}

/// What this computer's person is called, from their PekoHub name (while
/// signed in) and the name they chose here: the first of those, else
/// their login name on this computer.
#[must_use]
pub fn person_name(hub_name: Option<String>, chosen: Option<String>) -> Option<String> {
    hub_name.or(chosen).or_else(HostInfo::login_name)
}

/// This computer's person as `store` knows them.
pub async fn person_in(store: &ChannelStore) -> Person {
    Person {
        name: person_name(
            store.person_display_name().await,
            store.person_chosen_name().await,
        ),
        handle: store.person_handle().await,
    }
}

/// This computer's person.
pub async fn person(resolver: &PathResolver) -> Person {
    person_in(&actor_store(resolver)).await
}

/// One person as PekoHub gives them.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HubPerson {
    pub id: String,
    pub handle: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

impl HubPerson {
    /// What to call them: their display name, else their handle.
    pub(crate) fn shown(&self) -> String {
        self.display_name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| self.handle.clone())
    }
}

/// Ask PekoHub at `origin` as this runtime (a `PekoRuntime` proof, which
/// it answers as the account the runtime is registered to).
fn signed(
    request: reqwest::RequestBuilder,
    runtime_did: &str,
    key: &SigningKey,
    path: &str,
) -> Result<reqwest::RequestBuilder> {
    let proof = HubAgentDirectoryClient::directory_proof(runtime_did, key, path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(request.header("authorization", format!("PekoRuntime {proof}")))
}

/// Set the display name of the account this runtime is registered to.
pub(crate) async fn hub_set_name(
    origin: &str,
    runtime_did: &str,
    key: &SigningKey,
    name: &str,
) -> Result<HubPerson> {
    let path = "/v1/people/me/name";
    let request = reqwest::Client::new()
        .put(format!("{origin}{path}"))
        .json(&serde_json::json!({ "name": name }));
    let response = signed(request, runtime_did, key, path)?
        .send()
        .await
        .context("could not reach PekoHub")?;
    let status = response.status();
    if !status.is_success() {
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        let why = body["error"].as_str().unwrap_or("no reason given");
        anyhow::bail!("PekoHub did not take the name (HTTP {status}): {why}");
    }
    Ok(response.json().await?)
}

/// The current names and handles of the accounts `ids`.
async fn hub_names(
    origin: &str,
    runtime_did: &str,
    key: &SigningKey,
    ids: &[String],
) -> Result<Vec<HubPerson>> {
    #[derive(Deserialize)]
    struct Answer {
        people: Vec<HubPerson>,
    }
    let path = "/v1/people/names";
    let request = reqwest::Client::new()
        .post(format!("{origin}{path}"))
        .json(&serde_json::json!({ "ids": ids }));
    let response = signed(request, runtime_did, key, path)?
        .send()
        .await
        .context("could not reach PekoHub")?;
    anyhow::ensure!(
        response.status().is_success(),
        "PekoHub did not give names: HTTP {}",
        response.status()
    );
    Ok(response.json::<Answer>().await?.people)
}

/// The HTTP origin of the Hub this runtime is registered with.
fn registered_origin(resolver: &PathResolver) -> Result<String> {
    let credential = crate::tunnel::PekoHubCredential::from_file(&resolver.pekohub_config())
        .context("this computer is not signed in to PekoHub")?;
    crate::tunnel::hub_origin(&credential.url).context("the registration names no Hub origin")
}

/// Keep what PekoHub says of this computer's own person: their name here
/// and to others, and their handle.
async fn keep_own(store: &ChannelStore, account: &str, me: &HubPerson) -> Result<()> {
    let shown = me.shown();
    store.set_person_display_name(Some(&shown)).await?;
    store.set_person_chosen_name(Some(&shown)).await?;
    store.set_person_handle(Some(&me.handle)).await?;
    store
        .refresh_people(&[(account.to_string(), Some(shown), Some(me.handle.clone()))])
        .await?;
    Ok(())
}

/// Set what this computer's person is called. Signed in, PekoHub takes it
/// first (and may refuse it), so every computer and conversation of
/// theirs follows; signed out, it stays here until they sign in.
pub async fn set_person_name(
    resolver: &PathResolver,
    vault: Arc<Vault>,
    name: &str,
) -> Result<Person> {
    let name = clean_name(name).with_context(|| {
        format!("A name is 1 to {NAME_MAX} characters, without control characters")
    })?;
    let store = actor_store(resolver);
    match linked_account(resolver).await {
        Some(account) => {
            let origin = registered_origin(resolver)?;
            let (identity, key) = runtime_key(resolver, vault)?;
            let me = hub_set_name(&origin, &identity.runtime_did, &key, &name).await?;
            keep_own(&store, &account.id, &me).await?;
        }
        None => store.set_person_chosen_name(Some(&name)).await?,
    }
    Ok(person_in(&store).await)
}

/// Bring the names and handles of the people this runtime knows, its own
/// person among them, up to what PekoHub says now. Only a signed-in
/// runtime asks. Returns how many people changed.
pub async fn refresh_people_names(resolver: &PathResolver, vault: Arc<Vault>) -> Result<usize> {
    let Some(account) = linked_account(resolver).await else {
        return Ok(0);
    };
    let store = actor_store(resolver);
    let mut ids: Vec<String> = store
        .known_actors()
        .await?
        .into_iter()
        .filter(|a| a.kind == "user")
        .filter_map(|a| a.hub_id)
        .collect();
    ids.push(account.id.clone());
    ids.sort();
    ids.dedup();
    let origin = registered_origin(resolver)?;
    let (identity, key) = runtime_key(resolver, vault)?;
    let mut people = Vec::new();
    for chunk in ids.chunks(NAMES_PER_ASK) {
        people.extend(hub_names(&origin, &identity.runtime_did, &key, chunk).await?);
    }
    let mut changed = 0;
    if let Some(me) = people.iter().find(|p| p.id == account.id) {
        let shown = me.shown();
        if store.person_display_name().await.as_deref() != Some(shown.as_str())
            || store.person_handle().await.as_deref() != Some(me.handle.as_str())
        {
            keep_own(&store, &account.id, me).await?;
            changed += 1;
        }
    }
    let rows: Vec<_> = people
        .iter()
        .filter(|p| p.id != account.id)
        .map(|p| (p.id.clone(), Some(p.shown()), Some(p.handle.clone())))
        .collect();
    Ok(changed + store.refresh_people(&rows).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_tidied_and_nothing_hidden_gets_in() {
        assert_eq!(clean_name("  Maya   Lin\n").as_deref(), Some("Maya Lin"));
        assert_eq!(clean_name("🐱").as_deref(), Some("🐱"));
        assert_eq!(clean_name("   "), None);
        assert_eq!(clean_name(&"x".repeat(65)), None);
        assert_eq!(
            clean_name(&"é".repeat(64)).map(|n| n.chars().count()),
            Some(64)
        );
        assert_eq!(clean_name("Maya\u{202E}evil"), None);
        assert_eq!(clean_name("Maya\u{200B}"), None);
        assert_eq!(clean_name("Maya\u{7}"), None);
    }

    #[test]
    fn the_hub_name_wins_then_the_chosen_one_then_the_login_name() {
        assert_eq!(
            person_name(Some("Maya Lin".into()), Some("maya".into())).as_deref(),
            Some("Maya Lin")
        );
        assert_eq!(
            person_name(None, Some("Maya".into())).as_deref(),
            Some("Maya")
        );
        assert_eq!(person_name(None, None), HostInfo::login_name());
    }

    #[tokio::test]
    async fn signed_out_a_chosen_name_stays_here() {
        let home = tempfile::tempdir().unwrap();
        let resolver = PathResolver::with_dirs(
            home.path().to_path_buf(),
            home.path().join("data"),
            home.path().join("cache"),
        );
        let vault = Arc::new(Vault::for_test(home.path(), "test-passphrase"));
        let before = person(&resolver).await;
        assert_eq!(before.name, HostInfo::login_name());
        assert_eq!(before.handle, None);

        let after = set_person_name(&resolver, vault.clone(), "  Maya  ")
            .await
            .unwrap();
        assert_eq!(
            after,
            Person {
                name: Some("Maya".into()),
                handle: None
            }
        );
        assert_eq!(person(&resolver).await.name.as_deref(), Some("Maya"));
        assert!(set_person_name(&resolver, vault.clone(), "\u{202E}")
            .await
            .is_err());
        // Signed out, nothing to ask PekoHub.
        assert_eq!(refresh_people_names(&resolver, vault).await.unwrap(), 0);
    }

    /// A PekoHub that answers each request in turn with the next body,
    /// and hands back the requests it got.
    async fn fake_hub(
        answers: Vec<&'static str>,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (seen, requests) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            for body in answers {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 16384];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let _ = seen.send(String::from_utf8_lossy(&buf[..n]).to_string());
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (origin, requests)
    }

    /// Signed in, the name goes to PekoHub first, signed by this runtime;
    /// later, a name changed on PekoHub (from another computer) and other
    /// people's new names and handles come back here.
    #[tokio::test]
    async fn signed_in_the_name_is_the_accounts_and_people_follow_pekohub() {
        let home = tempfile::tempdir().unwrap();
        let resolver = PathResolver::with_dirs(
            home.path().to_path_buf(),
            home.path().join("data"),
            home.path().join("cache"),
        );
        let vault = Arc::new(Vault::for_test(home.path(), "test-passphrase"));
        let me = "0b6c7a4e-0000-4000-8000-000000000001";
        let noah = "0b6c7a4e-0000-4000-8000-000000000002";
        let store = actor_store(&resolver);
        store.ensure_local_user("did:key:zRuntime").await.unwrap();
        store.link_local_user("did:key:zRuntime", me).await.unwrap();
        let noah_ref: peko_subject::ActorRef = format!("user:hub:{noah}").parse().unwrap();
        store
            .register_actor(
                &noah_ref,
                Some(noah),
                None,
                None,
                Some("noah-gh"),
                peko_channel::Registration::Registered,
            )
            .await
            .unwrap();

        let set = r#"{"id":"0b6c7a4e-0000-4000-8000-000000000001","handle":"maya","displayName":"Maya Lin"}"#;
        let names = r#"{"people":[
            {"id":"0b6c7a4e-0000-4000-8000-000000000001","handle":"maya","displayName":"Maya"},
            {"id":"0b6c7a4e-0000-4000-8000-000000000002","handle":"noah","displayName":null}]}"#;
        let (origin, mut requests) = fake_hub(vec![set, names]).await;
        crate::tunnel::PekoHubCredential {
            url: crate::tunnel::registration::tunnel_url(&origin),
            runtime_id: "did:key:zRuntime".into(),
            tls: None,
        }
        .save_to_file(&resolver.pekohub_config())
        .unwrap();

        let named = set_person_name(&resolver, vault.clone(), "Maya Lin")
            .await
            .unwrap();
        assert_eq!(
            named,
            Person {
                name: Some("Maya Lin".into()),
                handle: Some("maya".into())
            }
        );
        let request = requests.recv().await.unwrap();
        assert!(request.starts_with("PUT /v1/people/me/name "), "{request}");
        assert!(request.contains("authorization: PekoRuntime "), "{request}");
        assert!(request.contains(r#""name":"Maya Lin""#), "{request}");
        let local = peko_subject::ActorRef::User("local".into());
        let info = store.actor_info(&local).await;
        assert_eq!(
            (info.name.as_deref(), info.handle.as_deref()),
            (Some("Maya Lin"), Some("maya"))
        );

        assert_eq!(refresh_people_names(&resolver, vault).await.unwrap(), 2);
        let request = requests.recv().await.unwrap();
        assert!(request.starts_with("POST /v1/people/names "), "{request}");
        assert!(request.contains(noah) && request.contains(me), "{request}");
        assert_eq!(person(&resolver).await.name.as_deref(), Some("Maya"));
        let info = store.actor_info(&noah_ref).await;
        // No display name: they go by their handle.
        assert_eq!(
            (info.name.as_deref(), info.handle.as_deref()),
            (Some("noah"), Some("noah"))
        );
    }
}
