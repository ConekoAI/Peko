//! The owner's other computer steps into a peko's browser on the peko's
//! host (ADR-119 D6). A child module of `tunnel_channel_port` so it
//! shares the port's store and tunnel context.
//!
//! - The person's side asks ([`TunnelChannelPort::peko_browser`]) in its
//!   DM with the peko and accepts only the answer it is waiting for.
//! - The host checks the request
//!   ([`TunnelChannelPort::verified_peko_browser_request`]); the
//!   dispatcher asks the daemon, which answers only the peko's owner,
//!   and the port signs and sends the answer
//!   ([`TunnelChannelPort::answer_peko_browser`]).
//!
//! Both messages are sealed (ADR-110), so PekoHub reads neither the
//! pictures of a page nor what is typed into it.

use super::peko_models::OwnDmPeko;
use super::TunnelChannelPort;
use crate::tunnel::{
    browser_text_digest, did_key_to_verifying_key, sign_history, verify_peko_browser,
    verify_peko_browser_request, PekoBrowserPayload, PekoBrowserRequestPayload, TunnelHandle,
    TunnelMessage, PEKO_BROWSER_REQUEST_VERSION, PEKO_BROWSER_VERSION,
};
use peko_channel::ChannelId;
use std::time::Duration;
use tracing::{debug, warn};

/// How long the person's side waits for the host's answer: a `frame`
/// waits up to eight seconds for the page to change before it answers.
const BROWSER_WAIT: Duration = Duration::from_secs(25);

/// A request waiting for its answer.
pub(super) struct PendingBrowser {
    host: String,
    principal_did: String,
    reply: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
}

impl TunnelChannelPort {
    /// Ask the host of the peko in `channel` — this account's DM with a
    /// peko on another runtime — one thing of its browser.
    ///
    /// # Errors
    ///
    /// The channel is not this account's DM with a peko elsewhere, this
    /// runtime has no PekoHub connection, the host does not answer, or
    /// it refuses.
    pub async fn peko_browser(
        &self,
        channel: &ChannelId,
        verb: &str,
        args: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let OwnDmPeko {
            ctx,
            account,
            runtime_id,
            principal_did,
            handle,
        } = self.own_dm_peko(channel).await?;
        let request_id = uuid::Uuid::new_v4().to_string();
        let args = args.to_string();
        let iat = chrono::Utc::now().timestamp();
        let payload = PekoBrowserRequestPayload {
            v: PEKO_BROWSER_REQUEST_VERSION.into(),
            request_id: request_id.clone(),
            source_runtime_id: ctx.caller_runtime_id.clone(),
            recipient_runtime_id: runtime_id.clone(),
            source_principal_did: format!("user:hub:{account}"),
            principal_did: principal_did.clone(),
            verb: verb.to_string(),
            args_digest: browser_text_digest(&args),
            iat,
            exp: iat + 300,
        };
        let signature = sign_history(&ctx.signing_key, &payload);
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.browser_pending
            .lock()
            .expect("browser pending")
            .insert(
                request_id.clone(),
                PendingBrowser {
                    host: runtime_id,
                    principal_did,
                    reply,
                },
            );
        let sent = handle.send(TunnelMessage::TunnelPekoBrowserRequest {
            request_id: request_id.clone(),
            source_runtime_id: payload.source_runtime_id,
            recipient_runtime_id: payload.recipient_runtime_id,
            source_principal_did: payload.source_principal_did,
            principal_did: payload.principal_did,
            verb: payload.verb,
            args,
            signature,
        });
        if let Err(e) = sent {
            self.browser_pending
                .lock()
                .expect("browser pending")
                .remove(&request_id);
            anyhow::bail!("Not connected to PekoHub: {e}");
        }
        match tokio::time::timeout(BROWSER_WAIT, answer).await {
            Ok(Ok(Ok(body))) => Ok(body),
            Ok(Ok(Err(refused))) => anyhow::bail!("{refused}"),
            _ => {
                self.browser_pending
                    .lock()
                    .expect("browser pending")
                    .remove(&request_id);
                anyhow::bail!("The computer hosting this peko did not answer; it may be offline")
            }
        }
    }

    /// The person's side receives an answer: accept it only for a request
    /// of ours, from the runtime asked, about the peko asked about.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn on_peko_browser(
        &self,
        request_id: String,
        source_runtime_id: String,
        recipient_runtime_id: String,
        principal_did: String,
        ok: bool,
        body: String,
        signature: String,
    ) {
        let Some(ctx) = self.ctx.read().await.clone() else {
            return;
        };
        if recipient_runtime_id != ctx.caller_runtime_id {
            return;
        }
        let verified = did_key_to_verifying_key(&source_runtime_id)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .and_then(|key| verify_peko_browser(&key, &signature));
        let agrees = verified.as_ref().is_ok_and(|p| {
            p.request_id == request_id
                && p.source_runtime_id == source_runtime_id
                && p.recipient_runtime_id == recipient_runtime_id
                && p.principal_did == principal_did
                && p.ok == ok
                && p.body_digest == browser_text_digest(&body)
        });
        if !agrees {
            warn!(
                request_id,
                "peko browser answer failed verification; dropped"
            );
            return;
        }
        // Taken only once the answer holds, so a bad one cannot end the wait.
        let waiting = {
            let mut pending = self.browser_pending.lock().expect("browser pending");
            let matches = pending
                .get(&request_id)
                .is_some_and(|p| p.host == source_runtime_id && p.principal_did == principal_did);
            if matches {
                pending.remove(&request_id)
            } else {
                None
            }
        };
        let Some(waiting) = waiting else {
            debug!(request_id, "unrequested peko browser answer dropped");
            return;
        };
        let answer = if ok {
            serde_json::from_str(&body).map_err(|e| format!("the host's answer is not JSON: {e}"))
        } else {
            Err(body)
        };
        let _ = waiting.reply.send(answer);
    }

    /// The host checks a request: addressed here, signed by the runtime
    /// it names, and asked by a person (`user:hub:<account>`). Whether
    /// that person owns the peko is the daemon's to decide.
    pub(crate) async fn verified_peko_browser_request(
        &self,
        envelope: PekoBrowserRequestPayload,
        signature: &str,
    ) -> Option<PekoBrowserRequestPayload> {
        let ctx = self.ctx.read().await.clone()?;
        if envelope.recipient_runtime_id != ctx.caller_runtime_id
            || !envelope.source_principal_did.starts_with("user:hub:")
        {
            return None;
        }
        let verified = match did_key_to_verifying_key(&envelope.source_runtime_id)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .and_then(|key| verify_peko_browser_request(&key, signature))
        {
            Ok(payload) => payload,
            Err(e) => {
                warn!(request_id = %envelope.request_id, "peko browser request failed verification: {e}");
                return None;
            }
        };
        let same = PekoBrowserRequestPayload {
            iat: verified.iat,
            exp: verified.exp,
            ..envelope
        };
        if same != verified {
            warn!(request_id = %verified.request_id, "peko browser request disagrees with its signature");
            return None;
        }
        Some(verified)
    }

    /// The host answers `request` with what the browser said, or why not.
    pub(crate) async fn answer_peko_browser(
        &self,
        handle: &TunnelHandle,
        request: &PekoBrowserRequestPayload,
        outcome: Result<serde_json::Value, String>,
    ) {
        let Some(ctx) = self.ctx.read().await.clone() else {
            return;
        };
        let (ok, body) = match outcome {
            Ok(body) => (true, body.to_string()),
            Err(refused) => (false, refused),
        };
        let iat = chrono::Utc::now().timestamp();
        let payload = PekoBrowserPayload {
            v: PEKO_BROWSER_VERSION.into(),
            request_id: request.request_id.clone(),
            source_runtime_id: ctx.caller_runtime_id.clone(),
            recipient_runtime_id: request.source_runtime_id.clone(),
            principal_did: request.principal_did.clone(),
            ok,
            body_digest: browser_text_digest(&body),
            iat,
            exp: iat + 300,
        };
        let signature = sign_history(&ctx.signing_key, &payload);
        let _ = handle.send(TunnelMessage::TunnelPekoBrowser {
            request_id: payload.request_id,
            source_runtime_id: payload.source_runtime_id,
            recipient_runtime_id: payload.recipient_runtime_id,
            principal_did: payload.principal_did,
            ok,
            body,
            signature,
        });
    }
}
