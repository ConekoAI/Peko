//! `tunnel` domain request handler (F6 step 4).
//!
//! Owns the daemon-side tunnel control IPC variants: `TunnelStop`,
//! `TunnelStatus`. The handler holds a narrow [`TunnelHost`] port;
//! the daemon-side implementation (`AppState`) is reached only through
//! the trait, so this module never imports
//! `crate::daemon::state::AppState` directly.
//!
//! Note: the inbound-message `TunnelHost` defined in `crate::tunnel::host`
//! (F5) is a separate trait with different methods — it powers the
//! tunnel dispatcher (ADR-035), not the IPC control surface. The two
//! traits share a name but live in different modules and serve
//! different consumers, per the F5 + F7 dependency-inversion pattern.
//!
//! Boundary rules:
//! - Dependency inversion: the consumer (`ipc::handlers::tunnel`) defines
//!   the [`TunnelHost`] trait; the producer (`daemon::state`) implements
//!   it (same pattern as `SystemHost`, `AuthHost`, `ToolHost`).
//! - F6: this module must not import any other `ipc::handlers::*` module.

use std::sync::Arc;

use async_trait::async_trait;

use crate::ipc::handlers::RequestHandler;
use crate::ipc::packet::{RequestPacket, ResponsePacket};
use crate::ipc::response_sink::ResponseSink;
use crate::ipc::send_response::send_response;
use crate::ipc::server::PeerAddr;
use peko_auth::caller::CallerContext;

/// Narrow port the `tunnel` IPC handler uses to reach daemon state.
///
/// `AppState` is the sole implementor. All methods are async: they
/// drive the tunnel lifecycle (ADR-035) and stay behind `async_trait`
/// to match the rest of the F6/F7 handler family.
#[async_trait]
pub(crate) trait TunnelHost: Send + Sync {
    /// Stop the live outbound tunnel. Idempotent — safe to call when
    /// no tunnel is running.
    async fn stop_tunnel(&self);

    /// Whether the outbound tunnel is currently connected.
    async fn tunnel_connected(&self) -> bool;

    /// Register with the Hub at `hub` as the account `token` signs in as,
    /// then (re)connect the tunnel.
    async fn register_runtime(
        &self,
        hub: &str,
        token: &str,
    ) -> anyhow::Result<crate::tunnel::registration::RuntimeRegistration>;

    /// Remove the Hub registration and close the tunnel.
    async fn unregister_runtime(&self, token: &str, discard_unsent: bool) -> anyhow::Result<()>;

    /// The Hub account the local user is linked to, `None` when
    /// unregistered (ADR-081 D3).
    async fn linked_account(&self) -> Option<crate::tunnel::registration::LinkedAccount>;

    /// What this computer's person is called, and their handle (ADR-124).
    async fn person(&self) -> crate::tunnel::people::Person;

    /// Set what this computer's person is called (ADR-124).
    async fn set_person_name(&self, name: &str) -> anyhow::Result<crate::tunnel::people::Person>;
    /// Publish a seed on PekoHub as this computer's person (ADR-127 D10).
    async fn publish_seed(
        &self,
        seed: &str,
        version: &str,
        readme: Option<&str>,
    ) -> anyhow::Result<crate::tunnel::seed_publish::Published>;

    /// How this computer connects (ADR-125 D2).
    async fn connection(&self) -> crate::common::types::config::Connection;
}

/// `tunnel` domain request handler. Constructed with an `Arc<dyn TunnelHost>`
/// (typically `Arc::new(app_state.clone())` from the dispatcher).
pub(crate) struct TunnelHandler {
    host: Arc<dyn TunnelHost>,
}

impl TunnelHandler {
    pub(crate) fn new(host: Arc<dyn TunnelHost>) -> Self {
        Self { host }
    }
}

#[async_trait]
impl RequestHandler for TunnelHandler {
    fn domain(&self) -> &'static str {
        "tunnel"
    }

    fn matches(&self, request: &RequestPacket) -> bool {
        matches!(
            request,
            RequestPacket::TunnelStop { .. }
                | RequestPacket::TunnelStatus { .. }
                | RequestPacket::RuntimeRegister { .. }
                | RequestPacket::RuntimeUnregister { .. }
                | RequestPacket::PersonNameSet { .. }
                | RequestPacket::SeedPublish { .. }
        )
    }

    async fn handle(
        &self,
        request: RequestPacket,
        caller: &CallerContext,
        sink: &dyn ResponseSink,
        _peer: &PeerAddr,
    ) -> anyhow::Result<()> {
        match request {
            RequestPacket::TunnelStop { request_id } => {
                self.host.stop_tunnel().await;
                let response = ResponsePacket::Done {
                    request_id,
                    success: true,
                    error: None,
                };
                send_response(sink, response).await?;
            }

            RequestPacket::TunnelStatus { request_id } => {
                let configured = crate::tunnel::credential::has_pekohub_credential();
                let connected = self.host.tunnel_connected().await;
                let account = self.host.linked_account().await;
                let person = self.host.person().await;
                let response = ResponsePacket::TunnelStatus {
                    request_id,
                    configured,
                    daemon_running: true,
                    connected,
                    hub_url: crate::tunnel::credential::registered_hub_origin(),
                    account_name: account.as_ref().and_then(|a| a.name.clone()),
                    account: account.map(|a| a.id),
                    person_name: person.name,
                    person_handle: person.handle,
                    connection: Some(self.host.connection().await),
                };
                send_response(sink, response).await?;
            }

            // Registration binds the runtime and its local user to a Hub
            // account: the local user's decision only (ADR-081 D3).
            RequestPacket::RuntimeRegister { request_id, .. }
            | RequestPacket::RuntimeUnregister { request_id, .. }
                if !caller.is_local() =>
            {
                let response = ResponsePacket::Error {
                    request_id,
                    message: "[permission_denied] only the local user registers this runtime"
                        .to_string(),
                };
                send_response(sink, response).await?;
            }

            // ADR-124: only this computer's person names themself.
            RequestPacket::PersonNameSet { request_id, .. } if !caller.is_local() => {
                let response = ResponsePacket::Error {
                    request_id,
                    message: "[permission_denied] only the local user sets their name".to_string(),
                };
                send_response(sink, response).await?;
            }

            // ADR-127 D10: a seed is published as this computer's person,
            // so only they (and what they run here) publish one.
            RequestPacket::SeedPublish { request_id, .. } if !caller.is_local() => {
                let response = ResponsePacket::Error {
                    request_id,
                    message: "[permission_denied] only the local user publishes a seed".to_string(),
                };
                send_response(sink, response).await?;
            }

            RequestPacket::SeedPublish {
                request_id,
                path,
                version,
                readme_path,
            } => {
                let response = match publish_seed_files(
                    self.host.as_ref(),
                    &path,
                    &version,
                    readme_path.as_deref(),
                )
                .await
                {
                    Ok(published) => ResponsePacket::SeedPublished {
                        request_id,
                        owner: published.owner,
                        name: published.name,
                        version: published.version,
                        digest: published.digest,
                        web_url: published.web_url,
                    },
                    Err(e) => ResponsePacket::Error {
                        request_id,
                        message: format!("{e:#}"),
                    },
                };
                send_response(sink, response).await?;
            }

            RequestPacket::PersonNameSet { request_id, name } => {
                let response = match self.host.set_person_name(&name).await {
                    Ok(_) => ResponsePacket::Done {
                        request_id,
                        success: true,
                        error: None,
                    },
                    Err(e) => ResponsePacket::Error {
                        request_id,
                        message: format!("{e:#}"),
                    },
                };
                send_response(sink, response).await?;
            }

            RequestPacket::RuntimeRegister {
                request_id,
                hub,
                token,
            } => {
                let response = match self.host.register_runtime(&hub, &token).await {
                    Ok(registered) => ResponsePacket::RuntimeRegistered {
                        request_id,
                        hub: registered.origin,
                        runtime_did: registered.runtime_did,
                        account: registered.account,
                    },
                    Err(e) => ResponsePacket::Error {
                        request_id,
                        message: format!("{e:#}"),
                    },
                };
                send_response(sink, response).await?;
            }

            RequestPacket::RuntimeUnregister {
                request_id,
                token,
                discard_unsent,
            } => {
                let response = match self.host.unregister_runtime(&token, discard_unsent).await {
                    Ok(()) => ResponsePacket::Done {
                        request_id,
                        success: true,
                        error: None,
                    },
                    Err(e) => ResponsePacket::Error {
                        request_id,
                        message: format!("{e:#}"),
                    },
                };
                send_response(sink, response).await?;
            }

            // `matches()` returned true, so the exhaustive list above
            // covers every owned variant. This arm is unreachable.
            _ => unreachable!("TunnelHandler::matches allowed an unhandled variant"),
        }
        Ok(())
    }
}

/// Read the seed (and README) a local caller named, then publish it.
async fn publish_seed_files(
    host: &dyn TunnelHost,
    path: &str,
    version: &str,
    readme_path: Option<&str>,
) -> anyhow::Result<crate::tunnel::seed_publish::Published> {
    use anyhow::Context as _;
    let read = |path: &str, limit: usize| {
        let path = std::path::PathBuf::from(path);
        async move {
            anyhow::ensure!(
                path.is_absolute(),
                "{} is not an absolute path",
                path.display()
            );
            let size = tokio::fs::metadata(&path)
                .await
                .with_context(|| format!("cannot read {}", path.display()))?
                .len();
            anyhow::ensure!(
                size <= limit as u64,
                "{} is larger than {} KB",
                path.display(),
                limit / 1024
            );
            tokio::fs::read_to_string(&path)
                .await
                .with_context(|| format!("cannot read {}", path.display()))
        }
    };
    let seed = read(path, crate::principal::seed::SEED_TEXT_MAX_BYTES).await?;
    let readme = match readme_path {
        Some(path) => Some(read(path, crate::tunnel::seed_publish::README_MAX_BYTES).await?),
        None => None,
    };
    host.publish_seed(&seed, version, readme.as_deref()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records registrations; never touches the network.
    #[derive(Default)]
    struct Host {
        registered: Mutex<Vec<(String, String)>>,
        linked: Mutex<Option<crate::tunnel::registration::LinkedAccount>>,
        person: Mutex<crate::tunnel::people::Person>,
    }

    #[async_trait]
    impl TunnelHost for Host {
        async fn stop_tunnel(&self) {}
        async fn tunnel_connected(&self) -> bool {
            false
        }
        async fn register_runtime(
            &self,
            hub: &str,
            token: &str,
        ) -> anyhow::Result<crate::tunnel::registration::RuntimeRegistration> {
            self.registered
                .lock()
                .unwrap()
                .push((hub.to_string(), token.to_string()));
            Ok(crate::tunnel::registration::RuntimeRegistration {
                origin: hub.to_string(),
                runtime_did: "did:key:zRuntime".to_string(),
                account: "0b6c7a4e-0000-4000-8000-000000000000".to_string(),
            })
        }
        async fn unregister_runtime(
            &self,
            _token: &str,
            _discard_unsent: bool,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn linked_account(&self) -> Option<crate::tunnel::registration::LinkedAccount> {
            self.linked.lock().unwrap().clone()
        }
        async fn person(&self) -> crate::tunnel::people::Person {
            self.person.lock().unwrap().clone()
        }
        async fn set_person_name(
            &self,
            name: &str,
        ) -> anyhow::Result<crate::tunnel::people::Person> {
            let name = crate::tunnel::people::clean_name(name)
                .ok_or_else(|| anyhow::anyhow!("A name is 1 to 64 characters"))?;
            let mut person = self.person.lock().unwrap();
            person.name = Some(name);
            Ok(person.clone())
        }
        async fn connection(&self) -> crate::common::types::config::Connection {
            crate::common::types::config::Connection::Automatic
        }
        async fn publish_seed(
            &self,
            seed: &str,
            version: &str,
            _readme: Option<&str>,
        ) -> anyhow::Result<crate::tunnel::seed_publish::Published> {
            anyhow::ensure!(
                self.linked.lock().unwrap().is_some(),
                "this computer is not signed in to PekoHub"
            );
            Ok(crate::tunnel::seed_publish::Published {
                owner: "maya".into(),
                name: "ada".into(),
                version: version.into(),
                digest: crate::principal::seed::seed_digest(seed),
                web_url: "https://pekohub.ai/seeds/maya/ada".into(),
            })
        }
    }

    struct Sink(Mutex<Vec<ResponsePacket>>);

    #[async_trait]
    impl ResponseSink for Sink {
        async fn send_bytes(&self, bytes: &[u8]) -> std::io::Result<()> {
            let packet = serde_json::from_slice(bytes).map_err(std::io::Error::other)?;
            self.0.lock().unwrap().push(packet);
            Ok(())
        }
    }

    async fn send(
        host: Arc<Host>,
        caller: CallerContext,
        request: RequestPacket,
    ) -> ResponsePacket {
        let sink = Sink(Mutex::new(Vec::new()));
        TunnelHandler::new(host)
            .handle(
                request,
                &caller,
                &sink,
                &PeerAddr::Ip("127.0.0.1:0".parse().unwrap()),
            )
            .await
            .unwrap();
        sink.0.into_inner().unwrap().pop().expect("a response")
    }

    /// pekohub ADR-007: the desktop registers its runtime with the token
    /// it signed in with, and gets the account back.
    #[tokio::test]
    async fn the_local_user_registers_the_runtime() {
        let host = Arc::new(Host::default());
        let response = send(
            host.clone(),
            CallerContext::local(),
            RequestPacket::RuntimeRegister {
                request_id: 7,
                hub: "https://api.pekohub.ai".into(),
                token: "access-token".into(),
            },
        )
        .await;
        match response {
            ResponsePacket::RuntimeRegistered {
                request_id,
                hub,
                account,
                ..
            } => {
                assert_eq!(request_id, 7);
                assert_eq!(hub, "https://api.pekohub.ai");
                assert!(!account.is_empty());
            }
            other => panic!("expected RuntimeRegistered, got {other:?}"),
        }
        assert_eq!(
            host.registered.lock().unwrap().as_slice(),
            &[(
                "https://api.pekohub.ai".to_string(),
                "access-token".to_string()
            )]
        );
    }

    /// ADR-081 D3: registering binds the local user to an account — no
    /// remote caller may do it.
    #[tokio::test]
    async fn a_remote_caller_cannot_register_the_runtime() {
        let host = Arc::new(Host::default());
        let response = send(
            host.clone(),
            CallerContext::from_jwt("someone".into()),
            RequestPacket::RuntimeRegister {
                request_id: 8,
                hub: "https://api.pekohub.ai".into(),
                token: "t".into(),
            },
        )
        .await;
        assert!(
            matches!(&response, ResponsePacket::Error { message, .. } if message.contains("permission_denied")),
            "{response:?}"
        );
        assert!(host.registered.lock().unwrap().is_empty());
    }

    /// The desktop learns which Hub account is the local user, so it can
    /// tell `user:hub:<account>` is "you" and name the account.
    #[tokio::test]
    async fn tunnel_status_names_the_linked_account() {
        let host = Arc::new(Host::default());
        let status = |host: Arc<Host>| async move {
            match send(
                host,
                CallerContext::local(),
                RequestPacket::TunnelStatus { request_id: 9 },
            )
            .await
            {
                ResponsePacket::TunnelStatus {
                    account,
                    account_name,
                    person_name,
                    person_handle,
                    ..
                } => (account, account_name, person_name, person_handle),
                other => panic!("expected TunnelStatus, got {other:?}"),
            }
        };
        *host.person.lock().unwrap() = crate::tunnel::people::Person {
            name: Some("maya".into()),
            handle: None,
        };
        assert_eq!(
            status(host.clone()).await,
            (None, None, Some("maya".into()), None)
        );

        *host.linked.lock().unwrap() = Some(crate::tunnel::registration::LinkedAccount {
            id: "0b6c7a4e-0000-4000-8000-000000000000".into(),
            name: Some("Alice".into()),
        });
        *host.person.lock().unwrap() = crate::tunnel::people::Person {
            name: Some("Alice".into()),
            handle: Some("alice".into()),
        };
        assert_eq!(
            status(host).await,
            (
                Some("0b6c7a4e-0000-4000-8000-000000000000".into()),
                Some("Alice".into()),
                Some("Alice".into()),
                Some("alice".into())
            )
        );
    }

    /// ADR-124: this computer's person names themself; nobody else does.
    #[tokio::test]
    async fn only_the_local_user_sets_their_name() {
        let host = Arc::new(Host::default());
        let set = |name: &str| RequestPacket::PersonNameSet {
            request_id: 4,
            name: name.into(),
        };
        match send(host.clone(), CallerContext::local(), set("Maya")).await {
            ResponsePacket::Done { success: true, .. } => {}
            other => panic!("expected Done, got {other:?}"),
        }
        assert_eq!(host.person.lock().unwrap().name.as_deref(), Some("Maya"));
        match send(host.clone(), CallerContext::local(), set("  ")).await {
            ResponsePacket::Error { message, .. } => {
                assert!(message.contains("1 to 64"), "{message}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
        let visitor = CallerContext::from_jwt("someone".to_string());
        match send(host.clone(), visitor, set("Mallory")).await {
            ResponsePacket::Error { message, .. } => {
                assert!(message.contains("permission_denied"), "{message}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert_eq!(host.person.lock().unwrap().name.as_deref(), Some("Maya"));
    }

    /// ADR-127 D10: a seed is published only by the local user, only as
    /// the person this computer is signed in as.
    #[tokio::test]
    async fn only_a_signed_in_local_user_publishes_a_seed() {
        let host = Arc::new(Host::default());
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("ada.seed.toml");
        std::fs::write(&seed, "name = \"ada\"\n").unwrap();
        let publish = || RequestPacket::SeedPublish {
            request_id: 5,
            path: seed.display().to_string(),
            version: "1.0.0".into(),
            readme_path: None,
        };
        match send(host.clone(), CallerContext::local(), publish()).await {
            ResponsePacket::Error { message, .. } => {
                assert!(message.contains("not signed in"), "{message}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
        *host.linked.lock().unwrap() = Some(crate::tunnel::registration::LinkedAccount {
            id: "acct".into(),
            name: Some("Maya".into()),
        });
        let visitor = CallerContext::from_jwt("someone".to_string());
        match send(host.clone(), visitor, publish()).await {
            ResponsePacket::Error { message, .. } => {
                assert!(message.contains("permission_denied"), "{message}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
        match send(host.clone(), CallerContext::local(), publish()).await {
            ResponsePacket::SeedPublished { owner, digest, .. } => {
                assert_eq!(owner, "maya");
                assert_eq!(
                    digest,
                    crate::principal::seed::seed_digest("name = \"ada\"\n")
                );
            }
            other => panic!("expected SeedPublished, got {other:?}"),
        }
        let relative = RequestPacket::SeedPublish {
            request_id: 6,
            path: "ada.seed.toml".into(),
            version: "1.0.0".into(),
            readme_path: None,
        };
        match send(host, CallerContext::local(), relative).await {
            ResponsePacket::Error { message, .. } => {
                assert!(message.contains("not an absolute path"), "{message}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
