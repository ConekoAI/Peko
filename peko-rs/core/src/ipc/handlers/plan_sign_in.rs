//! `plan_sign_in` domain request handler (ADR-115).
//!
//! Owns `RequestPacket::{ChatgptSignInStart, ChatgptSignInStatus,
//! ChatgptSignInCancel}`: Peko Desktop signs in with ChatGPT through the
//! engine, which listens for the browser's return on its own loopback
//! port and stores the sign-in in its vault. The desktop opens the
//! address, asks how it is going, and can hand over the address a
//! browser on another computer ended on.
//!
//! The handler holds a narrow [`PlanSignInHost`] port; the daemon side
//! (`AppState`) implements it. F6: no other `ipc::handlers::*` import.

use std::sync::Arc;

use async_trait::async_trait;

use crate::common::chatgpt_plan::FlowState;
use crate::ipc::handlers::RequestHandler;
use crate::ipc::packet::{PlanModelWire, PlanSignInState, RequestPacket, ResponsePacket};
use crate::ipc::response_sink::ResponseSink;
use crate::ipc::send_response::send_response;
use crate::ipc::server::PeerAddr;
use peko_auth::caller::CallerContext;

/// Vault slot a sign-in goes to when the client names none.
const DEFAULT_SLOT: &str = "chatgpt";

/// Port the handler runs sign-ins through.
#[async_trait]
pub(crate) trait PlanSignInHost: Send + Sync {
    /// Start a sign-in storing into slot `name`; `(flow_id, url)`.
    async fn plan_sign_in_start(&self, name: &str) -> anyhow::Result<(String, String)>;
    /// Where `flow_id` stands, handing it `pasted` if given.
    fn plan_sign_in_status(&self, flow_id: &str, pasted: Option<String>) -> FlowState;
    /// Stop waiting for `flow_id`.
    fn plan_sign_in_cancel(&self, flow_id: &str) -> FlowState;
}

pub(crate) struct PlanSignInHandler {
    host: Arc<dyn PlanSignInHost>,
}

impl PlanSignInHandler {
    pub(crate) fn new(host: Arc<dyn PlanSignInHost>) -> Self {
        Self { host }
    }
}

fn wire(state: FlowState) -> PlanSignInState {
    match state {
        FlowState::Waiting => PlanSignInState::Waiting,
        FlowState::Done(signed_in) => PlanSignInState::Done {
            credential_id: signed_in.credential_id,
            account: signed_in.account,
            models: signed_in
                .models
                .into_iter()
                .map(|m| PlanModelWire {
                    slug: m.slug,
                    display_name: m.display_name,
                })
                .collect(),
        },
        FlowState::Failed(message) => PlanSignInState::Failed { message },
        FlowState::Gone => PlanSignInState::Gone,
    }
}

/// A vault slot name: 1–64 letters, digits, `.`, `_` or `-`.
fn valid_slot(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[async_trait]
impl RequestHandler for PlanSignInHandler {
    fn domain(&self) -> &'static str {
        "plan_sign_in"
    }

    fn matches(&self, request: &RequestPacket) -> bool {
        matches!(
            request,
            RequestPacket::ChatgptSignInStart { .. }
                | RequestPacket::ChatgptSignInStatus { .. }
                | RequestPacket::ChatgptSignInCancel { .. }
        )
    }

    async fn handle(
        &self,
        request: RequestPacket,
        _caller: &CallerContext,
        sink: &dyn ResponseSink,
        _peer: &PeerAddr,
    ) -> anyhow::Result<()> {
        let response = match request {
            RequestPacket::ChatgptSignInStart { request_id, name } => {
                let name = name.unwrap_or_else(|| DEFAULT_SLOT.to_string());
                if !valid_slot(&name) {
                    ResponsePacket::Error {
                        request_id,
                        message: format!(
                            "'{name}' is not a usable name: use letters, digits, '.', '_' or '-'"
                        ),
                    }
                } else {
                    match self.host.plan_sign_in_start(&name).await {
                        Ok((flow_id, url)) => ResponsePacket::ChatgptSignInStarted {
                            request_id,
                            flow_id,
                            url,
                        },
                        Err(e) => ResponsePacket::Error {
                            request_id,
                            message: format!("{e:#}"),
                        },
                    }
                }
            }
            RequestPacket::ChatgptSignInStatus {
                request_id,
                flow_id,
                pasted_url,
            } => {
                let state = wire(self.host.plan_sign_in_status(&flow_id, pasted_url));
                ResponsePacket::ChatgptSignInState {
                    request_id,
                    flow_id,
                    state,
                }
            }
            RequestPacket::ChatgptSignInCancel {
                request_id,
                flow_id,
            } => {
                let state = wire(self.host.plan_sign_in_cancel(&flow_id));
                ResponsePacket::ChatgptSignInState {
                    request_id,
                    flow_id,
                    state,
                }
            }
            other => {
                return Err(anyhow::anyhow!(
                    "plan_sign_in handler got an unclaimed request (request_id={})",
                    other.request_id()
                ))
            }
        };
        send_response(sink, response).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::chatgpt_plan::SignedIn;
    use std::sync::Mutex;

    #[derive(Default)]
    struct StubHost {
        started: Mutex<Vec<String>>,
        pasted: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl PlanSignInHost for StubHost {
        async fn plan_sign_in_start(&self, name: &str) -> anyhow::Result<(String, String)> {
            self.started.lock().unwrap().push(name.to_string());
            Ok((
                "flow-1".into(),
                "https://auth.openai.com/authorize?x".into(),
            ))
        }
        fn plan_sign_in_status(&self, flow_id: &str, pasted: Option<String>) -> FlowState {
            if let Some(p) = pasted {
                self.pasted.lock().unwrap().push(p);
                return FlowState::Waiting;
            }
            if flow_id == "flow-1" {
                FlowState::Done(SignedIn {
                    credential_id: "cred-1".into(),
                    account: "a@example.com".into(),
                    models: vec![peko_providers::chatgpt::PlanModel {
                        slug: "gpt-6.2-sol".into(),
                        display_name: "GPT-6.2-Sol".into(),
                    }],
                })
            } else {
                FlowState::Gone
            }
        }
        fn plan_sign_in_cancel(&self, _flow_id: &str) -> FlowState {
            FlowState::Gone
        }
    }

    struct CaptureSink(Arc<Mutex<Vec<u8>>>);

    #[async_trait]
    impl ResponseSink for CaptureSink {
        async fn send_bytes(&self, bytes: &[u8]) -> std::io::Result<()> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(())
        }
    }

    async fn run(host: Arc<StubHost>, request: serde_json::Value) -> serde_json::Value {
        let request: RequestPacket = serde_json::from_value(request).unwrap();
        let buf = Arc::new(Mutex::new(Vec::new()));
        PlanSignInHandler::new(host)
            .handle(
                request,
                &CallerContext::local(),
                &CaptureSink(buf.clone()),
                &PeerAddr::Ip("127.0.0.1:0".parse().unwrap()),
            )
            .await
            .unwrap();
        let bytes = buf.lock().unwrap().clone();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn start_defaults_the_slot_and_returns_the_address() {
        let host = Arc::new(StubHost::default());
        let reply = run(
            host.clone(),
            serde_json::json!({"type": "chatgpt_sign_in_start", "request_id": 7}),
        )
        .await;
        assert_eq!(reply["type"], "chatgpt_sign_in_started");
        assert_eq!(reply["flow_id"], "flow-1");
        assert!(reply["url"]
            .as_str()
            .unwrap()
            .starts_with("https://auth.openai.com/"));
        assert_eq!(*host.started.lock().unwrap(), vec!["chatgpt".to_string()]);
        let bad = run(
            host,
            serde_json::json!({"type": "chatgpt_sign_in_start", "request_id": 8, "name": "../x"}),
        )
        .await;
        assert_eq!(bad["type"], "error");
    }

    #[tokio::test]
    async fn status_reports_done_with_models_and_passes_a_pasted_address() {
        let host = Arc::new(StubHost::default());
        let done = run(host.clone(), serde_json::json!({"type": "chatgpt_sign_in_status", "request_id": 1, "flow_id": "flow-1"})).await;
        assert_eq!(done["type"], "chatgpt_sign_in_state");
        assert_eq!(done["state"]["state"], "done");
        assert_eq!(done["state"]["credential_id"], "cred-1");
        assert_eq!(done["state"]["models"][0]["slug"], "gpt-6.2-sol");
        let waiting = run(
            host.clone(),
            serde_json::json!({"type": "chatgpt_sign_in_status", "request_id": 2, "flow_id": "flow-1", "pasted_url": "http://127.0.0.1:1/auth/callback?code=c"}),
        )
        .await;
        assert_eq!(waiting["state"]["state"], "waiting");
        assert_eq!(host.pasted.lock().unwrap().len(), 1);
        let gone = run(host, serde_json::json!({"type": "chatgpt_sign_in_cancel", "request_id": 3, "flow_id": "flow-1"})).await;
        assert_eq!(gone["state"]["state"], "gone");
    }
}
