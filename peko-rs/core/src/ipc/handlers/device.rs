//! `device` domain request handler (ADR-108, ADR-111): the owner adds,
//! joins, lists and removes the account's devices, decides what waits
//! about the account, and the person's read mark.
//!
//! Every request here is the local user's: a device on the list is a way
//! to act as the owner, so only someone at the computer may add or remove
//! one. The phone's own requests never come through IPC; they arrive
//! over the tunnel (`tunnel/devices.rs`, `daemon/state/devices.rs`).
//!
//! Boundary rule F6: this module imports no other `ipc::handlers::*`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::ipc::handlers::RequestHandler;
use crate::ipc::packet::{ChannelReadInfo, DevicesView, RequestPacket, ResponsePacket};
use crate::ipc::response_sink::ResponseSink;
use crate::ipc::send_response::send_response;
use crate::ipc::server::PeerAddr;
use crate::tunnel::device_sync::ShownCode;
use peko_auth::caller::CallerContext;

/// Narrow port the `device` handler uses to reach daemon state.
#[async_trait]
pub(crate) trait DeviceHost: Send + Sync {
    /// Show a code that adds a device.
    async fn show_code(&self) -> Result<ShownCode, String>;

    /// Join this computer to its account's devices with a code.
    async fn join(&self, code: &str) -> Result<(), String>;

    /// The account's devices and what waits about the account.
    async fn devices(&self) -> DevicesView;
    /// ADR-112: `of`'s name and code, as this computer holds them.
    async fn identity_code(&self, of: &str) -> (Option<String>, Option<String>);

    /// Remove a device from the account.
    async fn remove_device(&self, device_id: &str) -> Result<bool, String>;

    /// The owner's decision on something about the account.
    async fn decide_security(&self, id: &str, approve: bool) -> Result<String, String>;

    /// Move the person's read mark to the post `msg_id`, or to the newest
    /// post at or before `at`; whether it moved.
    async fn mark_read(
        &self,
        channel: &str,
        msg_id: Option<&str>,
        at: Option<&str>,
    ) -> Result<bool, String>;

    /// The person's read state in each of their conversations.
    async fn read_state(&self) -> Result<Vec<ChannelReadInfo>, String>;
}

/// `device` domain request handler.
pub(crate) struct DeviceHandler {
    host: Arc<dyn DeviceHost>,
}

impl DeviceHandler {
    pub(crate) fn new(host: Arc<dyn DeviceHost>) -> Self {
        Self { host }
    }
}

#[async_trait]
impl RequestHandler for DeviceHandler {
    fn domain(&self) -> &'static str {
        "device"
    }

    fn matches(&self, request: &RequestPacket) -> bool {
        matches!(
            request,
            RequestPacket::DeviceAdd { .. }
                | RequestPacket::DeviceJoin { .. }
                | RequestPacket::DeviceList { .. }
                | RequestPacket::IdentityCode { .. }
                | RequestPacket::DeviceRemove { .. }
                | RequestPacket::SecurityDecide { .. }
                | RequestPacket::ChannelMarkRead { .. }
                | RequestPacket::ChannelReadState { .. }
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
        let error = |message: String| ResponsePacket::Error {
            request_id,
            message,
        };
        if !caller.is_local() {
            return send_response(
                sink,
                error("[permission_denied] local callers only".to_string()),
            )
            .await;
        }
        let response = match request {
            RequestPacket::DeviceAdd { .. } => match self.host.show_code().await {
                Ok(shown) => ResponsePacket::DeviceCodeShown {
                    request_id,
                    code: shown.code,
                    qr: shown.qr,
                    expires_at: shown.expires_at,
                },
                Err(message) => error(message),
            },
            RequestPacket::DeviceJoin { code, .. } => match self.host.join(&code).await {
                Ok(()) => ResponsePacket::Done {
                    request_id,
                    success: true,
                    error: None,
                },
                Err(message) => error(message),
            },
            RequestPacket::DeviceList { .. } => ResponsePacket::DeviceListResult {
                request_id,
                view: self.host.devices().await,
            },
            RequestPacket::IdentityCode { of, .. } => {
                let (name, code) = self.host.identity_code(&of).await;
                ResponsePacket::IdentityCodeResult {
                    request_id,
                    of,
                    name,
                    code,
                }
            }
            RequestPacket::DeviceRemove { device_id, .. } => {
                match self.host.remove_device(&device_id).await {
                    Ok(removed) => ResponsePacket::Done {
                        request_id,
                        success: removed,
                        error: None,
                    },
                    Err(message) => error(message),
                }
            }
            RequestPacket::SecurityDecide { id, approve, .. } => {
                match self.host.decide_security(&id, approve).await {
                    Ok(message) => ResponsePacket::ApprovalDecided {
                        request_id,
                        id,
                        approved: approve,
                        message,
                    },
                    Err(message) => error(message),
                }
            }
            RequestPacket::ChannelMarkRead {
                channel,
                msg_id,
                at,
                ..
            } => match self
                .host
                .mark_read(&channel, msg_id.as_deref(), at.as_deref())
                .await
            {
                Ok(moved) => match peko_protocol::channel::ChannelId::parse(&channel) {
                    Some(channel) => ResponsePacket::ChannelReadMarked {
                        request_id,
                        channel,
                        moved,
                    },
                    None => error(format!("invalid ChannelId: {channel}")),
                },
                Err(message) => error(message),
            },
            RequestPacket::ChannelReadState { .. } => match self.host.read_state().await {
                Ok(channels) => ResponsePacket::ChannelReadStateResult {
                    request_id,
                    channels,
                },
                Err(message) => error(message),
            },
            _ => unreachable!("DeviceHandler::matches allowed an unhandled variant"),
        };
        send_response(sink, response).await
    }
}
