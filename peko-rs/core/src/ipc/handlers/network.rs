//! `network` domain request handler (ADR-125 D9): how this computer
//! connects to its other computers and people's.
//!
//! `network_status`, `network_settings_set` and `network_link_retry`
//! answer only a local caller. The handler holds a narrow
//! [`NetworkHost`] port (F6/F7): it never imports `AppState`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::common::types::config::{Connection, DirectWith};
use crate::ipc::handlers::RequestHandler;
use crate::ipc::packet::{RequestPacket, ResponsePacket};
use crate::ipc::response_sink::ResponseSink;
use crate::ipc::send_response::send_response;
use crate::ipc::server::PeerAddr;
use crate::tunnel::link::NetworkStatus;
use peko_auth::caller::CallerContext;

/// A change to the `[network]` link settings; `None` leaves a field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NetworkChange {
    pub connection: Option<Connection>,
    pub direct_with: Option<DirectWith>,
    pub direct_listen: Option<String>,
    pub direct_addresses: Option<Vec<String>>,
    pub direct_peers: Option<Vec<String>>,
}

/// Narrow port the `network` handler uses to reach daemon state.
#[async_trait]
pub(crate) trait NetworkHost: Send + Sync {
    /// What `network_status` answers.
    async fn network_status(&self) -> NetworkStatus;
    /// Check, write and apply `change`.
    async fn network_settings_set(&self, change: NetworkChange) -> Result<NetworkStatus, String>;
    /// Dial now.
    async fn network_link_retry(&self, runtime_id: Option<String>);
}

/// See the module docs.
pub(crate) struct NetworkHandler {
    host: Arc<dyn NetworkHost>,
}

impl NetworkHandler {
    pub(crate) fn new(host: Arc<dyn NetworkHost>) -> Self {
        Self { host }
    }
}

#[async_trait]
impl RequestHandler for NetworkHandler {
    fn domain(&self) -> &'static str {
        "network"
    }

    fn matches(&self, request: &RequestPacket) -> bool {
        matches!(
            request,
            RequestPacket::NetworkStatus { .. }
                | RequestPacket::NetworkSettingsSet { .. }
                | RequestPacket::NetworkLinkRetry { .. }
        )
    }

    async fn handle(
        &self,
        request: RequestPacket,
        caller: &CallerContext,
        sink: &dyn ResponseSink,
        _peer: &PeerAddr,
    ) -> anyhow::Result<()> {
        let request_id = request.request_id();
        if !caller.is_local() {
            return send_response(
                sink,
                ResponsePacket::Error {
                    request_id,
                    message: "[permission_denied] local callers only".to_string(),
                },
            )
            .await;
        }
        let response = match request {
            RequestPacket::NetworkStatus { .. } => ResponsePacket::NetworkStatusResult {
                request_id,
                status: self.host.network_status().await,
            },
            RequestPacket::NetworkSettingsSet {
                connection,
                direct_with,
                direct_listen,
                direct_addresses,
                direct_peers,
                ..
            } => {
                let change = NetworkChange {
                    connection,
                    direct_with,
                    direct_listen,
                    direct_addresses,
                    direct_peers,
                };
                match self.host.network_settings_set(change).await {
                    Ok(status) => ResponsePacket::NetworkStatusResult { request_id, status },
                    Err(message) => ResponsePacket::Error {
                        request_id,
                        message,
                    },
                }
            }
            RequestPacket::NetworkLinkRetry { runtime_id, .. } => {
                self.host.network_link_retry(runtime_id).await;
                ResponsePacket::Done {
                    request_id,
                    success: true,
                    error: None,
                }
            }
            _ => unreachable!("matches() admits only network requests"),
        };
        send_response(sink, response).await
    }
}

/// Check `change` and fold it into `network` (the `[network]` table).
///
/// # Errors
///
/// An address is not `host:port`.
pub(crate) fn apply_change(
    network: &mut crate::common::types::config::NetworkConfig,
    change: NetworkChange,
) -> Result<(), String> {
    use crate::tunnel::link::addresses::check_host_port;
    if let Some(listen) = &change.direct_listen {
        if !listen.is_empty() {
            listen
                .parse::<std::net::SocketAddr>()
                .map_err(|_| format!("direct_listen {listen:?} is not ip:port"))?;
        }
    }
    for entry in change
        .direct_addresses
        .iter()
        .chain(change.direct_peers.iter())
        .flatten()
    {
        check_host_port(entry)?;
    }
    if let Some(connection) = change.connection {
        network.connection = connection;
    }
    if let Some(with) = change.direct_with {
        network.direct_with = with;
    }
    if let Some(listen) = change.direct_listen {
        network.direct_listen = (!listen.is_empty()).then_some(listen);
    }
    if let Some(addresses) = change.direct_addresses {
        network.direct_addresses = addresses;
    }
    if let Some(peers) = change.direct_peers {
        network.direct_peers = peers;
    }
    Ok(())
}

/// Write `network`'s link settings into the `[network]` table of the
/// TOML document `text`, keeping everything else in it as it was.
///
/// # Errors
///
/// `text` is not TOML.
pub(crate) fn write_network_table(
    text: &str,
    network: &crate::common::types::config::NetworkConfig,
) -> Result<String, String> {
    use toml_edit::{value, Array, DocumentMut, Item, Table};
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| e.to_string())?;
    if !doc.contains_table("network") {
        doc["network"] = Item::Table(Table::new());
    }
    let table = doc["network"]
        .as_table_mut()
        .ok_or_else(|| "[network] is not a table".to_string())?;
    let set_or_remove = |table: &mut Table, key: &str, item: Option<Item>| match item {
        Some(item) => {
            table.insert(key, item);
        }
        None => {
            table.remove(key);
        }
    };
    let array = |values: &[String]| {
        let mut array = Array::new();
        for v in values {
            array.push(v.as_str());
        }
        value(array)
    };
    set_or_remove(
        table,
        "connection",
        (!network.connection.is_pekohub()).then(|| value(network.connection.as_str())),
    );
    set_or_remove(
        table,
        "direct_with",
        (!network.direct_with.is_mine()).then(|| value(network.direct_with.as_str())),
    );
    set_or_remove(
        table,
        "direct_listen",
        network.direct_listen.as_deref().map(value),
    );
    set_or_remove(
        table,
        "direct_addresses",
        (!network.direct_addresses.is_empty()).then(|| array(&network.direct_addresses)),
    );
    set_or_remove(
        table,
        "direct_peers",
        (!network.direct_peers.is_empty()).then(|| array(&network.direct_peers)),
    );
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::types::config::{NetworkConfig, PekoConfig};
    use std::sync::Mutex;

    #[test]
    fn a_change_is_checked_and_folded_in() {
        let mut network = NetworkConfig::default();
        apply_change(
            &mut network,
            NetworkChange {
                connection: Some(Connection::Automatic),
                direct_listen: Some("10.0.0.5:11438".into()),
                direct_peers: Some(vec!["10.0.0.12:11438".into()]),
                ..NetworkChange::default()
            },
        )
        .unwrap();
        assert_eq!(network.connection, Connection::Automatic);
        assert_eq!(network.direct_listen.as_deref(), Some("10.0.0.5:11438"));
        apply_change(
            &mut network,
            NetworkChange {
                direct_listen: Some(String::new()),
                ..NetworkChange::default()
            },
        )
        .unwrap();
        assert_eq!(network.direct_listen, None);
        assert!(apply_change(
            &mut network,
            NetworkChange {
                direct_peers: Some(vec!["10.0.0.12".into()]),
                ..NetworkChange::default()
            },
        )
        .is_err());
        assert!(apply_change(
            &mut network,
            NetworkChange {
                direct_listen: Some("localhost:1".into()),
                ..NetworkChange::default()
            },
        )
        .is_err());
        assert_eq!(network.direct_peers, vec!["10.0.0.12:11438".to_string()]);
    }

    #[test]
    fn the_network_table_is_written_and_the_rest_kept() {
        let text = "# my settings\n[network]\nhttp = \"127.0.0.1:11436\"\n\n[provider]\ntype = \"anthropic\"\n";
        let network = NetworkConfig {
            http: Some("127.0.0.1:11436".into()),
            connection: Connection::Direct,
            direct_with: DirectWith::People,
            direct_listen: Some("0.0.0.0:11500".into()),
            direct_peers: vec!["10.0.0.12:11438".into()],
            ..NetworkConfig::default()
        };
        let written = write_network_table(text, &network).unwrap();
        assert!(written.starts_with("# my settings"));
        assert!(written.contains("type = \"anthropic\""));
        let parsed: PekoConfig = toml::from_str(&written).unwrap();
        assert_eq!(parsed.network, network);
        // Back to the defaults: the keys go.
        let back = write_network_table(
            &written,
            &NetworkConfig {
                http: Some("127.0.0.1:11436".into()),
                ..NetworkConfig::default()
            },
        )
        .unwrap();
        assert!(!back.contains("connection"));
        assert!(!back.contains("direct_"));
        assert!(write_network_table("", &network)
            .unwrap()
            .contains("[network]"));
    }

    struct Host(Mutex<Vec<String>>);

    #[async_trait]
    impl NetworkHost for Host {
        async fn network_status(&self) -> NetworkStatus {
            NetworkStatus::idle(&crate::common::types::config::NetworkConfig::default())
        }
        async fn network_settings_set(&self, _: NetworkChange) -> Result<NetworkStatus, String> {
            self.0.lock().unwrap().push("set".into());
            Ok(self.network_status().await)
        }
        async fn network_link_retry(&self, _: Option<String>) {
            self.0.lock().unwrap().push("retry".into());
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

    #[tokio::test]
    async fn only_a_local_caller_is_answered() {
        let host = Arc::new(Host(Mutex::new(Vec::new())));
        let handler = NetworkHandler::new(host.clone());
        let sink = Sink(Mutex::new(Vec::new()));
        let peer = PeerAddr::Ip("127.0.0.1:0".parse().unwrap());
        handler
            .handle(
                RequestPacket::NetworkLinkRetry {
                    request_id: 1,
                    runtime_id: None,
                },
                &CallerContext::from_visitor_jwt("visitor-1".into()),
                &sink,
                &peer,
            )
            .await
            .unwrap();
        handler
            .handle(
                RequestPacket::NetworkStatus { request_id: 2 },
                &CallerContext::local(),
                &sink,
                &peer,
            )
            .await
            .unwrap();
        let answers = sink.0.lock().unwrap();
        assert!(
            matches!(&answers[0], ResponsePacket::Error { message, .. } if message.contains("local callers only"))
        );
        assert!(matches!(
            &answers[1],
            ResponsePacket::NetworkStatusResult { request_id: 2, .. }
        ));
        assert!(host.0.lock().unwrap().is_empty());
    }
}
