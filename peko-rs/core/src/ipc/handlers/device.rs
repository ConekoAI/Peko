//! `device` domain request handler (ADR-108): the owner pairs, lists and
//! removes phones on this computer, and the person's read mark.
//!
//! Every request here is the local user's: a paired phone is a way to
//! act as the owner, so only someone at the computer may add or remove
//! one. The phone's own requests never come through IPC; they arrive
//! over the tunnel (`tunnel/devices.rs`, `daemon/state/devices.rs`).
//!
//! Boundary rule F6: this module imports no other `ipc::handlers::*`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::ipc::handlers::RequestHandler;
use crate::ipc::packet::{ChannelReadInfo, DeviceInfo, RequestPacket, ResponsePacket};
use crate::ipc::response_sink::ResponseSink;
use crate::ipc::send_response::send_response;
use crate::ipc::server::PeerAddr;
use crate::tunnel::devices::PairingOffer;
use peko_auth::caller::CallerContext;

/// Narrow port the `device` handler uses to reach daemon state.
#[async_trait]
pub(crate) trait DeviceHost: Send + Sync {
    /// Open a pairing to show on this computer.
    fn pair_start(&self) -> Result<PairingOffer, String>;

    /// The paired phones, oldest pairing first.
    fn paired_devices(&self) -> Vec<DeviceInfo>;

    /// Remove a paired phone. `Ok(false)` when none has that id.
    async fn remove_device(&self, device_id: &str) -> Result<bool, String>;

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
            RequestPacket::DevicePairStart { .. }
                | RequestPacket::DeviceList { .. }
                | RequestPacket::DeviceRemove { .. }
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
            RequestPacket::DevicePairStart { .. } => match self.host.pair_start() {
                Ok(offer) => ResponsePacket::DevicePairStarted {
                    request_id,
                    qr: offer.qr_text(),
                    code: offer.pairing_id,
                    expires_at: offer.expires_at,
                },
                Err(message) => error(message),
            },
            RequestPacket::DeviceList { .. } => ResponsePacket::DeviceListResult {
                request_id,
                devices: self.host.paired_devices(),
            },
            RequestPacket::DeviceRemove { device_id, .. } => {
                match self.host.remove_device(&device_id).await {
                    Ok(removed) => ResponsePacket::Done {
                        request_id,
                        success: removed,
                        error: (!removed).then(|| format!("no phone `{device_id}` is paired")),
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
