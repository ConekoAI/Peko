//! Phones on the daemon side (ADR-108): the device verbs, the person's
//! read mark between their runtimes, live events, and push; and the owner's
//! devices for IPC (ADR-111).
//!
//! A phone never names an IPC request. Each verb in [`AppState::device_verb`]
//! is carried out here as the runtime's local user; a verb that is not in
//! that table does not exist for a phone. What decides whether a device
//! message is believed at all is [`crate::tunnel::devices::DeviceGate`].

use std::collections::{HashMap, HashSet};
use std::str::FromStr as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use peko_auth::caller::CallerContext;
use peko_channel::{ChannelEvent, ChannelId, ChannelPort as _, ConversationKind, StoreSignal};
use peko_subject::ActorRef;
use serde_json::{json, Value};

use super::AppState;
use crate::ipc::packet::{RequestPacket, ResponsePacket};
use crate::tunnel::device_sync::{DeviceSync, ShownCode};
use crate::tunnel::devices::{
    seal_notification, sign_event, sign_read_mark, sign_response, verify_read_mark, DeviceGate,
    DeviceNotifyPayload, DeviceRecord, ReadMarkPayload, DEVICE_NOTIFY_VERSION, READ_MARK_VERSION,
};
use crate::tunnel::TunnelMessage;

/// Longest text a phone may post.
const MAX_POST_CHARS: usize = 32_000;
/// Most posts one `read` returns.
const MAX_READ: usize = 100;
/// ADR-108 D8: the most of a file a phone is given at once.
const FILE_CHUNK_BYTES: usize = 256 * 1024;
/// The most of a file a phone sends at once: signed and encoded, it
/// still fits the 256 KB PekoHub lets a phone send.
const UPLOAD_CHUNK_BYTES: usize = 96 * 1024;
/// How long a file a phone sent waits for the post that names it.
const UPLOAD_KEPT: Duration = Duration::from_hours(1);
/// A notification waits this long; it is not sent if the person has read
/// the post by then (ADR-108 D6).
const NOTIFY_HOLD: Duration = if cfg!(test) {
    Duration::from_millis(60)
} else {
    Duration::from_secs(5)
};
/// How often runs and approval requests are looked at for phones.
const POLL: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(2)
};
/// Longest `approvals` waits for the person's other computers.
const REMOTE_APPROVALS_WAIT: Duration = Duration::from_secs(5);
/// Longest notification body.
const NOTIFY_BODY_CHARS: usize = 240;

/// What the daemon keeps for phones and the device list.
pub(crate) struct DeviceRuntime {
    pub(crate) gate: Arc<DeviceGate>,
    /// ADR-111: the owner's device list, kept current (made on first use).
    sync: std::sync::OnceLock<Arc<DeviceSync>>,
    /// Conversations a notification went out for and no clear has yet.
    notified: Mutex<HashSet<String>>,
    pump_started: AtomicBool,
}

impl DeviceRuntime {
    pub(crate) fn new(gate: DeviceGate) -> Self {
        Self {
            gate: Arc::new(gate),
            sync: std::sync::OnceLock::new(),
            notified: Mutex::new(HashSet::new()),
            pump_started: AtomicBool::new(false),
        }
    }
}

/// Collects what an IPC handler answers.
struct Collect(Mutex<Vec<ResponsePacket>>);

#[async_trait::async_trait]
impl crate::ipc::response_sink::ResponseSink for Collect {
    async fn send_bytes(&self, bytes: &[u8]) -> std::io::Result<()> {
        let packet = serde_json::from_slice(bytes).map_err(std::io::Error::other)?;
        self.0.lock().expect("collect sink").push(packet);
        Ok(())
    }
}

fn me() -> ActorRef {
    ActorRef::User("local".into())
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("[bad_request] `{key}` is required"))
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

impl AppState {
    fn store(&self) -> &Arc<peko_channel::ChannelStore> {
        self.tunnel_channel_port.local()
    }

    async fn send_to_hub(&self, message: TunnelMessage) -> bool {
        match self.tunnel_handle_slot.read().await.as_ref() {
            Some(handle) => handle.send(message).is_ok(),
            None => false,
        }
    }

    /// Carry out one fixed IPC request as the runtime's local user and
    /// return its single answer. The request is built here, never by a
    /// phone.
    async fn as_local_user(&self, request: RequestPacket) -> Result<ResponsePacket, String> {
        let sink = Collect(Mutex::new(Vec::new()));
        crate::ipc::handlers::RequestDispatcher::dispatch(
            self.clone(),
            request,
            &CallerContext::local(),
            &sink,
            &crate::ipc::server::PeerAddr::Ip(([127, 0, 0, 1], 0).into()),
        )
        .await
        .map_err(|e| format!("{e:#}"))?;
        match sink.0.into_inner().expect("collect sink").pop() {
            Some(ResponsePacket::Error { message, .. }) => Err(message),
            Some(packet) => Ok(packet),
            None => Err("no answer".into()),
        }
    }

    // ── The device list (ADR-111) ─────────────────────────────────────

    /// What keeps the owner's device list current and runs joins.
    pub(crate) fn device_sync(&self) -> Arc<DeviceSync> {
        Arc::clone(self.devices.sync.get_or_init(|| {
            DeviceSync::new(
                Arc::clone(&self.runtime_signing_key),
                crate::tunnel::TunnelHost::runtime_display_name(self),
                Arc::clone(&self.principal_manager),
                Arc::clone(&self.devices.gate),
                Arc::clone(&self.tunnel_handle_slot),
            )
        }))
    }

    /// Remove a device from the owner's list, and audit it. `by` says who
    /// removed it (`owner`, or the phone itself).
    ///
    /// # Errors
    ///
    /// No such device, or PekoHub did not take the change.
    pub(crate) async fn device_remove(&self, device_id: &str, by: &str) -> Result<bool, String> {
        self.device_sync().remove(device_id).await?;
        let _ = self
            .observability
            .audit_security(
                "device.removed",
                None,
                json!({ "device": device_id, "by": by }),
            )
            .await;
        Ok(true)
    }

    // ── Inbound device messages ───────────────────────────────────────

    /// A device message PekoHub relayed, or a read mark from another of
    /// the person's runtimes.
    pub(crate) async fn on_device_message(&self, message: TunnelMessage) {
        let gate = &self.devices.gate;
        match message {
            TunnelMessage::TunnelDeviceRequest {
                request_id,
                device_id,
                runtime_id,
                signature,
            } => {
                if runtime_id != gate.runtime_id() {
                    return;
                }
                let outcome =
                    match gate.verify_request(&request_id, &device_id, &runtime_id, &signature) {
                        Ok((device, request)) => {
                            self.device_verb(&device, &request.verb, &request.args)
                                .await
                        }
                        Err(refusal) => {
                            tracing::debug!(%device_id, "device request refused: {refusal}");
                            Err(refusal)
                        }
                    };
                let signature = sign_response(
                    &self.runtime_signing_key,
                    &request_id,
                    &device_id,
                    &runtime_id,
                    outcome,
                );
                self.send_to_hub(TunnelMessage::TunnelDeviceResponse {
                    request_id,
                    device_id,
                    runtime_id,
                    signature,
                })
                .await;
            }
            TunnelMessage::TunnelDeviceGone { device_id } => gate.unwatch(&device_id),
            TunnelMessage::TunnelReadMark {
                request_id,
                source_runtime_id,
                source_principal_did,
                channel_id,
                msg_id,
                signature,
            } => {
                self.on_read_mark(
                    &request_id,
                    &source_runtime_id,
                    &source_principal_did,
                    &channel_id,
                    &msg_id,
                    &signature,
                )
                .await;
            }
            _ => {}
        }
    }

    // ── Verbs ─────────────────────────────────────────────────────────

    /// ADR-108 D3: the whole of what a phone may ask.
    async fn device_verb(
        &self,
        device: &DeviceRecord,
        verb: &str,
        args: &Value,
    ) -> Result<Value, String> {
        match verb {
            "conversations" => self.verb_conversations().await,
            "read" => self.verb_read(args).await,
            "post" => self.verb_post(device, args).await,
            "file" => self.verb_file(args).await,
            "upload" => self.verb_upload(device, args).await,
            "mark_read" => {
                let channel = self.person_channel(arg_str(args, "channel")?).await?;
                let moved = self
                    .person_mark_read(&channel, arg_str(args, "msg_id")?)
                    .await?;
                Ok(json!({ "moved": moved == peko_channel::PersonMarkMove::Moved }))
            }
            "approvals" => self.verb_approvals().await,
            "secrets" => self.verb_secrets().await,
            "secret_provide" => self.verb_secret_answer(device, args, true).await,
            "secret_decline" => self.verb_secret_answer(device, args, false).await,
            "approval_decide" => self.verb_approval_decide(device, args).await,
            "browsers" => self.verb_browsers().await,
            "browser" => self.verb_browser(device, args).await,
            "stop" => self.verb_stop(args).await,
            "board" => {
                let channel = self.person_channel(arg_str(args, "channel")?).await?;
                match self
                    .as_local_user(RequestPacket::ChannelBoard {
                        request_id: 1,
                        channel: channel.as_str().to_string(),
                        sender_name: None,
                        op: None,
                    })
                    .await?
                {
                    ResponsePacket::ChannelBoardResult { items, .. } => {
                        Ok(json!({ "items": items }))
                    }
                    _ => Err("unexpected answer".into()),
                }
            }
            "watch" => {
                self.devices.gate.watch(&device.device_id);
                self.start_device_pump();
                Ok(json!({ "watching": true }))
            }
            "unwatch" => {
                self.devices.gate.unwatch(&device.device_id);
                Ok(json!({ "watching": false }))
            }
            "unpair" => {
                self.device_remove(&device.device_id, "device").await?;
                Ok(json!({ "paired": false }))
            }
            other => Err(format!("[unknown_verb] a phone cannot ask for `{other}`")),
        }
    }

    /// A conversation the person is in, by its id.
    async fn person_channel(&self, raw: &str) -> Result<ChannelId, String> {
        let missing = || "[not_found] no such conversation".to_string();
        let channel = ChannelId::parse(raw).ok_or_else(missing)?;
        if self
            .tunnel_channel_port
            .belongs_to_another_account(&channel)
            .await
        {
            return Err(missing());
        }
        match self.channel_port.list_members(&channel).await {
            Ok(members) if members.contains(&me()) => Ok(channel),
            _ => Err(missing()),
        }
    }

    /// What an actor is called here.
    async fn display_name(&self, address: &str) -> String {
        match ActorRef::from_str(address) {
            Ok(actor) => {
                let info = self.channel_port.actor_info(&actor).await;
                info.name.unwrap_or(info.address)
            }
            Err(_) => address.to_string(),
        }
    }

    /// The conversations a run is working in now.
    async fn working_channels(&self) -> HashSet<String> {
        let running: HashSet<String> = self
            .streaming_runs
            .lock()
            .expect("streaming runs")
            .keys()
            .cloned()
            .collect();
        let mut out = HashSet::new();
        if running.is_empty() {
            return out;
        }
        for principal in self.principal_manager.list_all().await {
            let mut sessions = peko_session::manager::SessionManager::new()
                .with_sessions_dir_internal(principal.memory.sessions_dir());
            let Ok(metas) = sessions.list_all_sessions(false).await else {
                continue;
            };
            for meta in metas {
                if running.contains(&meta.session_id.to_string()) {
                    out.extend(meta.channel_id);
                }
            }
        }
        out
    }

    async fn verb_conversations(&self) -> Result<Value, String> {
        let store = self.store();
        let working = self.working_channels().await;
        let own = me();
        let mut rows = Vec::new();
        for (channel, kind) in store.person_channels().await.map_err(|e| e.to_string())? {
            if self
                .tunnel_channel_port
                .belongs_to_another_account(&channel)
                .await
            {
                continue;
            }
            let read = store.person_read(&channel).await.unwrap_or_default();
            let last = match store.newest_post(&channel).await.ok().flatten() {
                Some(ChannelEvent::Posted {
                    author,
                    text,
                    at,
                    msg_id,
                    ..
                }) => json!({
                    "msg_id": msg_id,
                    "author": author,
                    "author_name": self.display_name(&author).await,
                    "text": truncate(&text, NOTIFY_BODY_CHARS),
                    "at": at,
                }),
                _ => Value::Null,
            };
            let mut row = json!({
                "channel": channel.as_str(),
                "kind": kind,
                "last": last,
                "unread": read.unread,
                "read": read.mark,
                "working": working.contains(channel.as_str()),
            });
            match kind {
                ConversationKind::Dm => {
                    let peer = self
                        .channel_port
                        .list_members(&channel)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .find(|m| *m != own);
                    if let Some(peer) = peer {
                        let info = self.channel_port.actor_info(&peer).await;
                        row["name"] =
                            json!(info.name.clone().unwrap_or_else(|| info.address.clone()));
                        row["peer"] = json!({
                            "address": info.address,
                            "kind": info.kind,
                            "name": info.name,
                        });
                    }
                }
                ConversationKind::Group => {
                    row["name"] = json!(store.channel_name(&channel).await.unwrap_or_default());
                }
            }
            rows.push(row);
        }
        rows.sort_by(|a, b| {
            let at = |row: &Value| row["last"]["at"].as_str().unwrap_or_default().to_string();
            at(b).cmp(&at(a))
        });
        let account = store.actor_info(&own).await.hub_id;
        let invitations = match account.as_deref() {
            Some(account) => store.invitations(account).await.unwrap_or_default(),
            None => Vec::new(),
        };
        Ok(json!({ "conversations": rows, "invitations": invitations }))
    }

    async fn verb_read(&self, args: &Value) -> Result<Value, String> {
        let channel = self.person_channel(arg_str(args, "channel")?).await?;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(50, |n| n as usize)
            .clamp(1, MAX_READ);
        let cursor = |key: &str| args.get(key).and_then(Value::as_str);
        let (events, has_more) = self
            .store()
            .history_page(&channel, cursor("before"), cursor("after"), limit)
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({ "channel": channel.as_str(), "events": events, "has_more": has_more }))
    }

    /// ADR-108 D8: one piece of a file a post in the person's
    /// conversation carries. The file is named by its post and its place
    /// on it, never by a path.
    async fn verb_file(&self, args: &Value) -> Result<Value, String> {
        use base64::Engine as _;
        use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
        let channel = self.person_channel(arg_str(args, "channel")?).await?;
        let msg_id = arg_str(args, "msg_id")?;
        let number = |key: &str| args.get(key).and_then(Value::as_u64).unwrap_or(0);
        let (index, offset) = (number("index") as usize, number("offset"));
        let store = self.store();
        let Some(ChannelEvent::Posted { attachments, .. }) = store
            .post_by_id(&channel, msg_id)
            .await
            .map_err(|e| e.to_string())?
        else {
            return Err("[not_found] that post is not in the conversation".into());
        };
        let attachment = attachments
            .get(index)
            .ok_or("[not_found] that post has no such file")?;
        let files_dir = store.files_dir(&channel).await.ok();
        let path = match self.peko_copy(&channel, attachment).await {
            Some(path) => Some(path),
            None => crate::principal::attachments::locate(None, files_dir.as_deref(), attachment),
        }
        .ok_or("[not_here] that file has not reached this computer yet")?;
        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(|_| "[not_here] that file is no longer on this computer")?;
        let size = file.metadata().await.map_err(|e| e.to_string())?.len();
        if offset > size {
            return Err("[bad_request] `offset` is past the end of the file".into());
        }
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| e.to_string())?;
        let mut data = Vec::with_capacity(FILE_CHUNK_BYTES.min((size - offset) as usize));
        file.take(FILE_CHUNK_BYTES as u64)
            .read_to_end(&mut data)
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "name": attachment.name,
            "mime": attachment.mime,
            "size": size,
            "offset": offset,
            "done": offset + data.len() as u64 >= size,
            "data": base64::engine::general_purpose::STANDARD.encode(&data),
        }))
    }

    /// A peko hosted here keeps a copy of the files of its conversations
    /// in its workspace, both the ones it sent and the ones its owner sent
    /// it by path (ADR-088 D2): the copy `attachment` names, in the
    /// workspace of a peko of this conversation, when the path stays
    /// inside that workspace.
    async fn peko_copy(
        &self,
        channel: &ChannelId,
        attachment: &peko_protocol::channel::Attachment,
    ) -> Option<std::path::PathBuf> {
        if attachment.path.is_empty() {
            return None;
        }
        let members = self
            .channel_port
            .list_members(channel)
            .await
            .unwrap_or_default();
        for member in members {
            let ActorRef::Principal(route) = &member else {
                continue;
            };
            let id = peko_subject::PrincipalId::from_did(route);
            let Some(principal) = self.principal_manager.get(id).await else {
                continue;
            };
            let Ok(workspace) = tokio::fs::canonicalize(&principal.workspace_path).await else {
                continue;
            };
            let Ok(path) = tokio::fs::canonicalize(crate::principal::attachments::absolute(
                &principal.workspace_path,
                attachment,
            ))
            .await
            else {
                continue;
            };
            if path.starts_with(&workspace) && path.is_file() {
                return Some(path);
            }
        }
        None
    }

    /// Where `device` keeps the file it is sending as `id`.
    fn upload_path(&self, device: &DeviceRecord, id: &str) -> Result<std::path::PathBuf, String> {
        let plain = |s: &str| {
            (8..=64).contains(&s.len())
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        };
        if !plain(id) || !plain(&device.device_id) {
            return Err("[bad_request] `id` is 8 to 64 letters, digits, `-` or `_`".into());
        }
        Ok(self
            .devices
            .gate
            .uploads_dir()
            .join(&device.device_id)
            .join(id))
    }

    /// ADR-108 D8: one piece of a file the phone is sending. Pieces
    /// arrive in order; the answer says how much of the file is held, so
    /// a phone that lost an answer continues from there.
    async fn verb_upload(&self, device: &DeviceRecord, args: &Value) -> Result<Value, String> {
        use base64::Engine as _;
        use tokio::io::AsyncWriteExt as _;
        let path = self.upload_path(device, arg_str(args, "id")?)?;
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let data = base64::engine::general_purpose::STANDARD
            .decode(arg_str(args, "data")?)
            .map_err(|_| "[bad_request] `data` is not base64")?;
        if data.len() > UPLOAD_CHUNK_BYTES {
            return Err("[bad_request] a piece is at most 96 KiB".into());
        }
        let dir = path.parent().ok_or("no upload folder")?.to_path_buf();
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| format!("cannot keep the file: {e}"))?;
        let held = tokio::fs::metadata(&path).await.map_or(0, |m| m.len());
        if offset == 0 && held == 0 {
            // A new file: what this phone left unfinished long ago goes
            // first, and it holds only so much at once.
            let (mut files, mut bytes) = (0usize, 0u64);
            let mut entries = tokio::fs::read_dir(&dir).await.map_err(|e| e.to_string())?;
            while let Ok(Some(entry)) = entries.next_entry().await {
                let Ok(meta) = entry.metadata().await else {
                    continue;
                };
                let stale = meta
                    .modified()
                    .ok()
                    .and_then(|at| at.elapsed().ok())
                    .is_some_and(|age| age > UPLOAD_KEPT);
                if stale {
                    let _ = tokio::fs::remove_file(entry.path()).await;
                } else {
                    files += 1;
                    bytes += meta.len();
                }
            }
            let limits = crate::principal::attachments::Limits::REMOTE;
            if files >= crate::principal::attachments::MAX_FILES
                || bytes >= limits.max_total.unwrap_or(u64::MAX)
            {
                return Err("[too_many] post or drop the files already sent first".into());
            }
        }
        if offset < held {
            // A piece sent again after its answer was lost.
            return Ok(json!({ "received": held }));
        }
        if offset > held {
            return Err(format!(
                "[out_of_order] {held} bytes are held; send from there"
            ));
        }
        if held + data.len() as u64 > crate::principal::attachments::REMOTE_MAX_FILE_BYTES {
            let _ = tokio::fs::remove_file(&path).await;
            return Err("[too_large] a file is at most 10 MB".into());
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| format!("cannot keep the file: {e}"))?;
        file.write_all(&data).await.map_err(|e| e.to_string())?;
        file.flush().await.map_err(|e| e.to_string())?;
        Ok(json!({ "received": held + data.len() as u64 }))
    }

    /// The files a post names, moved from what the phone sent into the
    /// conversation's file store. Each is `{ upload, name }`.
    async fn uploaded_files(
        &self,
        device: &DeviceRecord,
        channel: &ChannelId,
        named: &[Value],
    ) -> Result<Vec<peko_protocol::channel::Attachment>, String> {
        use crate::principal::attachments;
        if named.len() > attachments::MAX_FILES {
            return Err(format!(
                "[bad_request] a post carries at most {} files",
                attachments::MAX_FILES
            ));
        }
        // Each file gets a folder of its own, so it can carry its name.
        let mut paths = Vec::with_capacity(named.len());
        for file in named {
            let staged = self.upload_path(device, arg_str(file, "upload")?)?;
            // The name is a name, whatever folders the phone wrote before it.
            let name = arg_str(file, "name")?;
            let name = attachments::safe_file_name(name.rsplit(['/', '\\']).next().unwrap_or(name));
            let folder = staged.with_extension("d");
            let named_path = folder.join(&name);
            if !tokio::fs::try_exists(&named_path).await.unwrap_or(false) {
                if !tokio::fs::try_exists(&staged).await.unwrap_or(false) {
                    return Err(
                        "[not_found] that file was not sent, or was sent too long ago".into(),
                    );
                }
                tokio::fs::create_dir_all(&folder)
                    .await
                    .map_err(|e| e.to_string())?;
                tokio::fs::rename(&staged, &named_path)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            paths.push(named_path.display().to_string());
        }
        let files_dir = self
            .store()
            .files_dir(channel)
            .await
            .map_err(|e| e.to_string())?;
        // The bytes crossed PekoHub to get here: its limits apply.
        attachments::ingest_into_channel(&files_dir, &paths, attachments::Limits::REMOTE).await
    }

    /// Forget what the phone sent for a post that is now made.
    async fn drop_uploads(&self, device: &DeviceRecord, named: &[Value]) {
        for file in named {
            let Some(Ok(staged)) = file
                .get("upload")
                .and_then(Value::as_str)
                .map(|id| self.upload_path(device, id))
            else {
                continue;
            };
            let _ = tokio::fs::remove_file(&staged).await;
            let _ = tokio::fs::remove_dir_all(staged.with_extension("d")).await;
        }
    }

    async fn verb_post(&self, device: &DeviceRecord, args: &Value) -> Result<Value, String> {
        use sha2::Digest as _;
        let channel = self.person_channel(arg_str(args, "channel")?).await?;
        let named = args
            .get("attachments")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        // A post with files may have no words.
        let text = args.get("text").and_then(Value::as_str).unwrap_or("");
        if (text.trim().is_empty() && named.is_empty()) || text.chars().count() > MAX_POST_CHARS {
            return Err(
                "[bad_request] a post needs text or a file, and at most 32,000 characters".into(),
            );
        }
        // A repeated `client_msg_id` names the same post: the id is
        // derived from it, and a post already in the log is not made
        // again, however long ago the first try was.
        let client_msg_id = arg_str(args, "client_msg_id")?;
        let digest = sha2::Sha256::digest(
            format!(
                "{}\n{}\n{client_msg_id}",
                device.device_id,
                channel.as_str()
            )
            .as_bytes(),
        );
        let msg_id = format!("msg_{}", &hex::encode(digest)[..32]);
        let store = self.store();
        if store
            .post_line(&channel, &msg_id)
            .await
            .map_err(|e| e.to_string())?
            .is_some()
        {
            self.drop_uploads(device, named).await;
            return Ok(json!({ "channel": channel.as_str(), "msg_id": msg_id, "repeated": true }));
        }
        // ADR-114 D8: answers to a post's questions, checked by the
        // store against the card they name. An answer is a root post.
        let answers = match args.get("answers") {
            None | Some(Value::Null) => None,
            Some(value) => {
                if args.get("reply_to").is_some() {
                    return Err("[bad_request] answers are a root post, not a reply".into());
                }
                Some(
                    serde_json::from_value::<peko_protocol::channel::Answers>(value.clone())
                        .map_err(|e| format!("[bad_request] `answers`: {e}"))?,
                )
            }
        };
        let files = self.uploaded_files(device, &channel, named).await?;
        let mut msg = match args.get("reply_to").and_then(Value::as_str) {
            Some(parent) => {
                let line = store
                    .post_line(&channel, parent)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or("[not_found] the post this replies to is not in the conversation")?;
                peko_channel::PostMsg::reply(line, text)
            }
            None => peko_channel::PostMsg::root(text),
        };
        msg.msg_id = Some(msg_id.clone());
        msg.attachments = files.clone();
        msg.answers = answers;
        let me = me();
        self.channel_port
            .post(&channel, &me, msg)
            .await
            .map_err(|e| format!("could not post: {e}"))?;
        self.drop_uploads(device, named).await;
        // What the person writes is read by them.
        let _ = store.mark_person_read(&channel, &msg_id).await;
        // A DM with a peko hosted here: its turn starts as for any post
        // of the person's that arrives over the tunnel (ADR-090 D5).
        if args.get("reply_to").is_none() {
            use crate::tunnel::TunnelHost as _;
            self.human_dm_post(channel.clone(), me, text.to_string(), files, None)
                .await;
        }
        Ok(json!({ "channel": channel.as_str(), "msg_id": msg_id, "repeated": false }))
    }

    /// ADR-108 D5: move the person's read mark and, when it moved, tell
    /// their other runtimes.
    pub(crate) async fn person_mark_read(
        &self,
        channel: &ChannelId,
        msg_id: &str,
    ) -> Result<peko_channel::PersonMarkMove, String> {
        if !peko_channel::is_post_id(msg_id) {
            return Err("[bad_request] `msg_id` is not a post's id".into());
        }
        let moved = self
            .store()
            .mark_person_read(channel, msg_id)
            .await
            .map_err(|e| e.to_string())?;
        if moved == peko_channel::PersonMarkMove::Moved {
            if let Some(account) = self.store().actor_info(&me()).await.hub_id {
                let iat = chrono::Utc::now().timestamp();
                let payload = ReadMarkPayload {
                    v: READ_MARK_VERSION.into(),
                    request_id: uuid::Uuid::new_v4().to_string(),
                    source_runtime_id: self.devices.gate.runtime_id().to_string(),
                    source_principal_did: format!("user:hub:{account}"),
                    channel_id: channel.as_str().to_string(),
                    msg_id: msg_id.to_string(),
                    iat,
                    exp: iat + 300,
                };
                let signature = sign_read_mark(&self.runtime_signing_key, &payload);
                self.send_to_hub(TunnelMessage::TunnelReadMark {
                    request_id: payload.request_id,
                    source_runtime_id: payload.source_runtime_id,
                    source_principal_did: payload.source_principal_did,
                    channel_id: payload.channel_id,
                    msg_id: payload.msg_id,
                    signature,
                })
                .await;
            }
        }
        Ok(moved)
    }

    /// The person read a conversation on another of their runtimes.
    /// PekoHub relays a read mark only between runtimes of one account;
    /// the mark must also name that account and be signed by the runtime
    /// it says it is from. It moves the mark here and goes no further.
    async fn on_read_mark(
        &self,
        request_id: &str,
        source_runtime_id: &str,
        source_principal_did: &str,
        channel_id: &str,
        msg_id: &str,
        signature: &str,
    ) {
        if source_runtime_id == self.devices.gate.runtime_id() {
            return;
        }
        let Some(account) = self.store().actor_info(&me()).await.hub_id else {
            return;
        };
        let verified = crate::tunnel::did_key_to_verifying_key(source_runtime_id)
            .map_err(|e| e.to_string())
            .and_then(|key| verify_read_mark(&key, signature));
        let agrees = verified.as_ref().is_ok_and(|p| {
            p.request_id == request_id
                && p.source_runtime_id == source_runtime_id
                && p.source_principal_did == source_principal_did
                && p.channel_id == channel_id
                && p.msg_id == msg_id
        });
        if !agrees || source_principal_did != format!("user:hub:{account}") {
            tracing::warn!(request_id, "read mark failed verification; dropped");
            return;
        }
        if let Ok(channel) = self.person_channel(channel_id).await {
            let _ = self.store().mark_person_read(&channel, msg_id).await;
        }
    }

    /// The pekos hosted here that the person owns, with their names.
    async fn owned_pekos(&self) -> Vec<(String, Arc<crate::principal::Principal>)> {
        use peko_subject::ActorKeys as _;
        let port: &dyn peko_channel::ChannelPort = self.tunnel_channel_port.as_ref();
        let me = me();
        let mut out = Vec::new();
        for principal in self.principal_manager.list_all().await {
            let (name, owner) = {
                let config = principal.config.read().await;
                (config.name.clone(), config.owner.clone())
            };
            if peko_channel::PortKeys(port).same_actor(&owner, &me) {
                out.push((name, principal));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The approval requests waiting for the person on this computer.
    async fn local_approvals(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for (name, principal) in self.owned_pekos().await {
            for record in crate::principal::policy_gate::pending_requests_for_owner(
                &self.principal_manager,
                &principal,
                None,
            )
            .await
            {
                // Who asked, by the name this computer knows them by.
                let actor_name = match &record.call.stamp.name {
                    Some(name) => Some(name.clone()),
                    None => Some(self.display_name(&record.call.stamp.actor).await)
                        .filter(|shown| *shown != record.call.stamp.actor),
                };
                out.push(json!({
                    "peko": name,
                    "id": record.id,
                    "actor": record.call.stamp.actor,
                    "actor_name": actor_name,
                    "trust": record.call.stamp.trust.as_str(),
                    "summary": crate::principal::policy_gate::request_summary(&record),
                    "reason": record.reason,
                    "created_at": record.created_at.to_rfc3339(),
                    "expires_at": record.expires_at.to_rfc3339(),
                    "kind": crate::principal::policy_gate::request_kind(&record),
                }));
            }
        }
        out
    }

    async fn verb_approvals(&self) -> Result<Value, String> {
        let mut approvals = self.local_approvals().await;
        // ADR-099: the person's pekos on their other computers, asked
        // from this one. A computer that does not answer in time is left
        // out and named.
        let mut unreachable = Vec::new();
        if let Some(account) = self.store().actor_info(&me()).await.hub_id {
            let dms = self.store().remote_dms(&account).await.unwrap_or_default();
            let asks = dms.into_iter().map(|channel| async move {
                let asked = tokio::time::timeout(
                    REMOTE_APPROVALS_WAIT,
                    self.tunnel_channel_port.peko_approvals(&channel),
                )
                .await;
                (channel, asked)
            });
            for (channel, asked) in futures::future::join_all(asks).await {
                match asked {
                    Ok(Ok(list)) => {
                        for approval in list {
                            let mut row = serde_json::to_value(&approval).unwrap_or_default();
                            row["channel"] = json!(channel.as_str());
                            approvals.push(row);
                        }
                    }
                    // Not this person's own peko there: nothing to ask.
                    Ok(Err(_)) => {}
                    Err(_) => unreachable.push(channel.as_str().to_string()),
                }
            }
        }
        Ok(json!({ "approvals": approvals, "unreachable": unreachable }))
    }

    async fn verb_approval_decide(
        &self,
        device: &DeviceRecord,
        args: &Value,
    ) -> Result<Value, String> {
        use crate::principal::policy_gate::{decide_request_via, Decision};
        let id = arg_str(args, "id")?;
        let approve = args
            .get("approve")
            .and_then(Value::as_bool)
            .ok_or("[bad_request] `approve` is required")?;
        let always = approve && args.get("always").and_then(Value::as_bool).unwrap_or(false);
        let via = format!("device:{}", device.device_id);
        // A peko on another of the person's computers is named by its DM.
        if let Some(raw) = args.get("channel").and_then(Value::as_str) {
            let channel = self.person_channel(raw).await?;
            let message = self
                .tunnel_channel_port
                .decide_peko_approval(&channel, id, approve, always)
                .await
                .map_err(|e| format!("{e}"))?;
            let _ = self
                .observability
                .audit_security(
                    "policy.owner_decision",
                    None,
                    json!({
                        "channel": channel.as_str(),
                        "request_id": id,
                        "via_device": device.device_id,
                        "decision": if !approve { "deny" } else if always { "approve_always" } else { "approve" },
                    }),
                )
                .await;
            return Ok(json!({ "message": message }));
        }
        let name = arg_str(args, "peko")?;
        let (_, principal) = self
            .owned_pekos()
            .await
            .into_iter()
            .find(|(owned, _)| owned == name)
            .ok_or("[forbidden] Only the peko's owner decides its approval requests.")?;
        let decision = if approve {
            Decision::Approve { always }
        } else {
            Decision::Deny
        };
        let message = decide_request_via(
            &self.principal_manager,
            &principal,
            id,
            decision,
            Some(&via),
        )
        .await
        .map_err(|e| format!("{e}"))?;
        Ok(json!({ "message": message }))
    }

    /// This engine's secret desk (ADR-116): its own vault and principals.
    fn secret_desk(&self) -> crate::principal::secret_requests::SecretDesk {
        crate::principal::secret_requests::SecretDesk::new(
            Arc::clone(&self.vault),
            self.principal_manager.shared_inbox_registry(),
            Arc::downgrade(&self.principal_manager),
        )
    }

    /// ADR-116 D7 (amended): the secret requests the person's pekos on
    /// this computer have waiting, names only.
    async fn verb_secrets(&self) -> Result<Value, String> {
        use crate::principal::secret_requests::{waiting, Target};
        let mut rows = Vec::new();
        for (name, principal) in self.owned_pekos().await {
            // The address a DM names this peko by (`conversations`).
            let address = self
                .channel_port
                .actor_info(&ActorRef::from(&principal.id))
                .await
                .address;
            for r in waiting(&principal) {
                rows.push(json!({
                    "peko": name,
                    "address": address,
                    "id": r.id,
                    "secret": r.target.label(),
                    "kind": match r.target { Target::Peko { .. } => "peko", Target::Mcp { .. } => "mcp" },
                    "why": r.why,
                    "created_at": r.created_at.to_rfc3339(),
                    "expires_at": r.expires_at.to_rfc3339(),
                }));
            }
        }
        Ok(json!({ "secrets": rows }))
    }

    /// ADR-116 D7 (amended): the person gives (`provide`) or declines a
    /// secret from their phone. The request arrived sealed to this
    /// computer (ADR-110), and the value goes to the vault and nowhere
    /// else: not the audit, not the answer.
    async fn verb_secret_answer(
        &self,
        device: &DeviceRecord,
        args: &Value,
        provide: bool,
    ) -> Result<Value, String> {
        use crate::principal::secret_requests::{decline, provide as give};
        let name = arg_str(args, "peko")?;
        let id = arg_str(args, "id")?;
        let (_, principal) = self
            .owned_pekos()
            .await
            .into_iter()
            .find(|(owned, _)| owned == name)
            .ok_or("[forbidden] Only the peko's owner gives it secrets.")?;
        let desk = self.secret_desk();
        let message = if provide {
            let value = arg_str(args, "value")?;
            give(
                &desk,
                &principal,
                id,
                secrecy::SecretString::from(value.to_string()),
            )
            .await
        } else {
            decline(&desk, &principal, id).await
        }
        .map_err(|e| format!("[bad_request] {e}"))?;
        let _ = self
            .observability
            .audit_security(
                "secret.owner_answer",
                None,
                json!({
                    "peko": name,
                    "request_id": id,
                    "via_device": device.device_id,
                    "decision": if provide { "provide" } else { "decline" },
                }),
            )
            .await;
        Ok(json!({ "message": message }))
    }

    /// ADR-119 D6 (ADR-108 D12): the browsers of the person's pekos on
    /// this computer that are running or wait for the person's hand.
    async fn verb_browsers(&self) -> Result<Value, String> {
        let Some(desk) = crate::browser::browser_desk() else {
            return Ok(json!({ "browsers": [] }));
        };
        let mut rows = Vec::new();
        for (name, principal) in self.owned_pekos().await {
            let peko = desk.peko(&principal);
            let running = peko.running().await.is_some();
            let handoff = peko.handoff();
            if !running && handoff.is_none() {
                continue;
            }
            // The address a DM names this peko by (`conversations`).
            let address = self
                .channel_port
                .actor_info(&ActorRef::from(&principal.id))
                .await
                .address;
            rows.push(json!({
                "peko": name,
                "address": address,
                "running": running,
                "controller": peko.controller().map(|(viewer, _)| viewer),
                "handoff": handoff.map(|h| json!({
                    "why": h.why, "tab": h.tab, "url": h.url, "at": h.at.to_rfc3339(),
                })),
            }));
        }
        Ok(json!({ "browsers": rows }))
    }

    /// ADR-119 D6 (ADR-108 D12): the person watches a tab of their
    /// peko's browser, takes control of it, and acts in it from their
    /// phone (`do`: `state`, `frame`, `control`, `input`, `open`). The
    /// request arrived sealed to this computer and the pictures leave
    /// sealed to the phone (ADR-110), so PekoHub reads neither what is
    /// on the page nor what is typed into it.
    async fn verb_browser(&self, device: &DeviceRecord, args: &Value) -> Result<Value, String> {
        let name = arg_str(args, "peko")?;
        let action = arg_str(args, "do")?;
        if !matches!(action, "state" | "frame" | "control" | "input" | "open") {
            return Err(format!("[bad_request] a phone cannot `{action}` a browser"));
        }
        let (_, principal) = self
            .owned_pekos()
            .await
            .into_iter()
            .find(|(owned, _)| owned == name)
            .ok_or("[forbidden] Only the peko's owner uses its browser.")?;
        let desk = crate::browser::browser_desk()
            .ok_or("[not_here] The browser is not available on this computer.")?;
        let viewer = format!("device:{}", device.device_id);
        crate::browser::verbs::handle(&desk, &principal, &viewer, action, args)
            .await
            .map_err(|e| format!("[bad_request] {e}"))
    }

    async fn verb_stop(&self, args: &Value) -> Result<Value, String> {
        let channel = self.person_channel(arg_str(args, "channel")?).await?;
        let kind = self
            .channel_port
            .conversation_kind(&channel)
            .await
            .map_err(|e| e.to_string())?;
        let mut stopped = false;
        if kind == ConversationKind::Dm {
            // The person's own thread with each peko bound here: the same
            // stop `peko stop` sends.
            for principal in self.human_dm_recipients(&channel).await {
                let Some(principal) = self.principal_manager.get(principal).await else {
                    continue;
                };
                let name = principal.config.read().await.name.clone();
                if let Ok(ResponsePacket::Done { success: true, .. }) = self
                    .as_local_user(RequestPacket::PrincipalStop {
                        request_id: 1,
                        name,
                        peer: None,
                        main: false,
                    })
                    .await
                {
                    stopped = true;
                }
            }
            return Ok(json!({ "stopped": stopped }));
        }
        // A group: the person stops the pekos they own.
        for (_, principal) in self.owned_pekos().await {
            let mut sessions = peko_session::manager::SessionManager::new()
                .with_sessions_dir_internal(principal.memory.sessions_dir());
            let Ok(metas) = sessions.list_all_sessions(false).await else {
                continue;
            };
            for meta in metas {
                if meta.channel_id.as_deref() != Some(channel.as_str()) {
                    continue;
                }
                let cancel = self
                    .streaming_runs
                    .lock()
                    .expect("streaming runs")
                    .get(&meta.session_id.to_string())
                    .map(|run| run.cancel.clone());
                if let Some(cancel) = cancel {
                    cancel.cancel();
                    stopped = true;
                }
            }
        }
        Ok(json!({ "stopped": stopped }))
    }

    // ── Live events and push ──────────────────────────────────────────

    /// Send `event` to every watching device (ADR-108 D4).
    async fn emit_device_event(&self, event: Value) {
        let gate = &self.devices.gate;
        for (device, seq) in gate.next_events() {
            let signature = sign_event(
                &self.runtime_signing_key,
                &device.device_id,
                gate.runtime_id(),
                seq,
                event.clone(),
            );
            self.send_to_hub(TunnelMessage::TunnelDeviceEvent {
                device_id: device.device_id,
                runtime_id: gate.runtime_id().to_string(),
                signature,
            })
            .await;
        }
    }

    /// Seal `notification` to each paired device and hand it to PekoHub.
    async fn notify_devices(&self, collapse_key: &str, mut notification: DeviceNotifyPayload) {
        let gate = &self.devices.gate;
        for device in gate.devices() {
            notification.device_id.clone_from(&device.device_id);
            match seal_notification(
                &self.runtime_signing_key,
                &device.agreement_key,
                &notification,
            ) {
                Ok(sealed) => {
                    self.send_to_hub(TunnelMessage::TunnelDeviceNotify {
                        device_id: device.device_id,
                        runtime_id: gate.runtime_id().to_string(),
                        collapse_key: collapse_key.to_string(),
                        sealed: Some(sealed),
                        clear: false,
                    })
                    .await;
                }
                Err(e) => {
                    tracing::warn!(device = %device.device_id, "notification not sealed: {e}")
                }
            }
        }
        self.devices
            .notified
            .lock()
            .expect("notified")
            .insert(collapse_key.to_string());
    }

    fn notification(&self, title: String, body: String, channel: &str) -> DeviceNotifyPayload {
        DeviceNotifyPayload {
            v: DEVICE_NOTIFY_VERSION.into(),
            device_id: String::new(),
            runtime_id: self.devices.gate.runtime_id().to_string(),
            title,
            body: truncate(&body, NOTIFY_BODY_CHARS),
            channel: channel.to_string(),
            msg_id: None,
            approval: None,
            peko: None,
            iat: chrono::Utc::now().timestamp(),
        }
    }

    /// Whether a post by someone else is one the person is told about
    /// while the app is closed (ADR-108 D6): any post in a DM; in a group,
    /// one that mentions them or adds a board item for them.
    async fn post_notifies(
        &self,
        channel: &ChannelId,
        text: &str,
        board: Option<&peko_protocol::channel::BoardOp>,
    ) -> bool {
        if self.channel_port.conversation_kind(channel).await.ok() == Some(ConversationKind::Dm) {
            return true;
        }
        let me = me();
        let info = self.channel_port.actor_info(&me).await;
        if let Some(peko_protocol::channel::BoardOp::Add {
            for_member: Some(member),
            ..
        }) = board
        {
            return member
                .parse::<ActorRef>()
                .is_ok_and(|m| self.channel_port.key_of(&m) == self.channel_port.key_of(&me));
        }
        let mut names = vec![info.address.clone()];
        names.extend(info.hub_address());
        names.extend(info.name.clone());
        names.extend(self.store().person_display_name().await);
        names.iter().any(|name| peko_channel::mentions(text, name))
    }

    /// Something was appended to a conversation.
    async fn on_store_event(&self, channel: ChannelId, event: ChannelEvent) {
        if matches!(event, ChannelEvent::Received { .. }) {
            return;
        }
        if self.person_channel(channel.as_str()).await.is_err() {
            return;
        }
        if self.devices.gate.has_watchers() {
            self.emit_device_event(json!({
                "kind": "event",
                "channel": channel.as_str(),
                "event": event,
            }))
            .await;
        }
        let ChannelEvent::Posted {
            author,
            text,
            msg_id: Some(msg_id),
            board,
            ..
        } = event
        else {
            return;
        };
        let own = self.store().actor_info(&me()).await;
        if author == own.address || Some(&author) == own.hub_address().as_ref() {
            return;
        }
        if !self.post_notifies(&channel, &text, board.as_ref()).await {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(NOTIFY_HOLD).await;
            if state
                .store()
                .person_has_read(&channel, &msg_id)
                .await
                .unwrap_or(false)
            {
                return;
            }
            // A peko's notice of an approval request: the request has a
            // notification of its own, which opens the request.
            if text.starts_with(crate::principal::policy_gate::APPROVAL_NOTICE_PREFIX) {
                return;
            }
            let author_name = state.display_name(&author).await;
            let dm = state.channel_port.conversation_kind(&channel).await.ok()
                == Some(ConversationKind::Dm);
            let (title, body) = if dm {
                (author_name, text)
            } else {
                (
                    state
                        .store()
                        .channel_name(&channel)
                        .await
                        .unwrap_or_default(),
                    format!("{author_name}: {text}"),
                )
            };
            let mut notification = state.notification(title, body, channel.as_str());
            notification.msg_id = Some(msg_id);
            state.notify_devices(channel.as_str(), notification).await;
        });
    }

    /// The person's read mark moved, here or on another computer.
    async fn on_person_read(&self, channel: ChannelId, msg_id: String) {
        self.emit_device_event(json!({
            "kind": "read",
            "channel": channel.as_str(),
            "msg_id": msg_id,
        }))
        .await;
        let notified = self
            .devices
            .notified
            .lock()
            .expect("notified")
            .remove(channel.as_str());
        if !notified {
            return;
        }
        let gate = &self.devices.gate;
        for device in gate.devices() {
            self.send_to_hub(TunnelMessage::TunnelDeviceNotify {
                device_id: device.device_id,
                runtime_id: gate.runtime_id().to_string(),
                collapse_key: channel.as_str().to_string(),
                sealed: None,
                clear: true,
            })
            .await;
        }
    }

    /// Runs and approval requests have no signal of their own: look at
    /// them, and tell phones what changed since the last look.
    async fn poll_for_devices(
        &self,
        working: &mut HashSet<String>,
        pending: &mut Option<HashMap<String, Value>>,
    ) {
        if self.devices.gate.devices().is_empty() {
            return;
        }
        let now: HashMap<String, Value> = self
            .local_approvals()
            .await
            .into_iter()
            .filter_map(|a| Some((a["id"].as_str()?.to_string(), a)))
            .collect();
        // The first look after a start only learns what is waiting.
        if let Some(before) = pending.as_ref() {
            let changed =
                before.len() != now.len() || now.keys().any(|id| !before.contains_key(id));
            if changed {
                self.emit_device_event(json!({ "kind": "approvals" })).await;
            }
            for (id, approval) in now.iter().filter(|(id, _)| !before.contains_key(*id)) {
                let peko = approval["peko"].as_str().unwrap_or_default().to_string();
                let mut notification = self.notification(
                    peko.clone(),
                    format!(
                        "Needs your approval: {}",
                        approval["summary"].as_str().unwrap_or_default()
                    ),
                    "",
                );
                notification.approval = Some(id.clone());
                notification.peko = Some(peko);
                self.notify_devices(&format!("approval:{id}"), notification)
                    .await;
            }
        }
        *pending = Some(now);

        if !self.devices.gate.has_watchers() {
            working.clear();
            return;
        }
        let now = self.working_channels().await;
        for channel in now.symmetric_difference(working) {
            if self.person_channel(channel).await.is_ok() {
                self.emit_device_event(json!({
                    "kind": "run",
                    "channel": channel,
                    "working": now.contains(channel),
                }))
                .await;
            }
        }
        *working = now;
    }

    /// Start following conversations for paired phones. Idempotent.
    pub(crate) fn start_device_pump(&self) {
        if self.devices.pump_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let state = self.clone();
        let mut signals = self.store().subscribe_all();
        let mut shutdown = self.shutdown_tx.subscribe();
        tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            let mut tick = tokio::time::interval(POLL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut working = HashSet::new();
            let mut pending = None;
            loop {
                tokio::select! {
                    signal = signals.recv() => match signal {
                        Ok(_) if state.devices.gate.devices().is_empty() => {}
                        Ok(StoreSignal::Event { channel, event }) => {
                            state.on_store_event(channel, event).await;
                        }
                        Ok(StoreSignal::PersonRead { channel, msg_id }) => {
                            state.on_person_read(channel, msg_id).await;
                        }
                        // Events were missed: a watching phone reads again.
                        Err(RecvError::Lagged(_)) => {
                            state.emit_device_event(json!({ "kind": "resync" })).await;
                        }
                        Err(RecvError::Closed) => break,
                    },
                    _ = tick.tick() => state.poll_for_devices(&mut working, &mut pending).await,
                    _ = shutdown.recv() => break,
                }
            }
        });
    }
}

#[async_trait::async_trait]
impl crate::ipc::handlers::device::DeviceHost for AppState {
    async fn show_code(&self) -> Result<ShownCode, String> {
        self.start_device_pump();
        self.device_sync().show_code().await
    }

    async fn join(&self, code: &str) -> Result<(), String> {
        self.device_sync().join(code).await
    }

    async fn identity_code(&self, of: &str) -> (Option<String>, Option<String>) {
        match crate::principal::policy_gate::identity_code(&self.principal_manager, of).await {
            Some(found) => (found.name, Some(found.code)),
            None => (None, None),
        }
    }

    async fn devices(&self) -> crate::ipc::packet::DevicesView {
        let status = self.device_sync().status().await;
        let last_seen: HashMap<String, Option<String>> = self
            .devices
            .gate
            .devices()
            .into_iter()
            .map(|d| (d.device_id, d.last_seen))
            .collect();
        let router = self.link_router().await;
        let devices = status
            .devices
            .iter()
            .map(|m| crate::ipc::packet::DeviceInfo {
                // ADR-125 D9: how each other computer is reached now.
                path: match (&router, m.device.kind) {
                    (Some(router), crate::security::device_list::DeviceKind::Computer)
                        if m.device.id != status.me =>
                    {
                        Some(router.path_to(&m.device.id))
                    }
                    _ => None,
                },
                id: m.device.id.clone(),
                kind: match m.device.kind {
                    crate::security::device_list::DeviceKind::Computer => "computer",
                    crate::security::device_list::DeviceKind::Phone => "phone",
                }
                .into(),
                name: m.device.name.clone(),
                hub_device_id: m.device.hub_device_id.clone(),
                added_at: chrono::DateTime::from_timestamp(m.added_at, 0)
                    .unwrap_or_default()
                    .to_rfc3339(),
                added_by: m.added_by.clone(),
                this: m.device.id == status.me,
                last_seen: m
                    .device
                    .hub_device_id
                    .as_ref()
                    .and_then(|id| last_seen.get(id).cloned().flatten()),
            })
            .collect();
        let security = crate::principal::policy_gate::security_requests(&self.principal_manager)
            .await
            .into_iter()
            .map(|r| crate::ipc::packet::SecurityInfo {
                id: r.id,
                kind: r.kind,
                summary: r.summary,
                reason: r.reason,
                peko: r.peko,
                created_at: r.created_at.to_rfc3339(),
                code: r.code,
                pinned_code: r.pinned_code,
            })
            .collect();
        crate::ipc::packet::DevicesView {
            code: status.code,
            account: status.account,
            runtime: status.me,
            on_list: status.on_list,
            devices,
            security,
        }
    }

    async fn remove_device(&self, device_id: &str) -> Result<bool, String> {
        self.device_remove(device_id, "owner").await
    }

    async fn decide_security(&self, id: &str, approve: bool) -> Result<String, String> {
        crate::principal::policy_gate::decide_account_claim(
            &self.principal_manager,
            id,
            approve,
            "local",
        )
        .await
        .map_err(|e| format!("[bad_request] {e}"))
    }

    async fn mark_read(
        &self,
        channel: &str,
        msg_id: Option<&str>,
        at: Option<&str>,
    ) -> Result<bool, String> {
        let channel = self.person_channel(channel).await?;
        let msg_id = match (msg_id, at) {
            (Some(msg_id), None) => msg_id.to_string(),
            (None, Some(at)) => {
                let at = chrono::DateTime::parse_from_rfc3339(at)
                    .map_err(|_| "[bad_request] `at` is not an RFC 3339 time".to_string())?
                    .with_timezone(&chrono::Utc);
                match self
                    .store()
                    .newest_post_by(&channel, at)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    Some(msg_id) => msg_id,
                    // Nothing was posted by then: nothing to have seen.
                    None => return Ok(false),
                }
            }
            _ => return Err("[bad_request] give `msg_id` or `at`, one of them".into()),
        };
        Ok(self.person_mark_read(&channel, &msg_id).await? == peko_channel::PersonMarkMove::Moved)
    }

    async fn read_state(&self) -> Result<Vec<crate::ipc::packet::ChannelReadInfo>, String> {
        let store = self.store();
        let own = me();
        let mut out = Vec::new();
        for (channel, kind) in store.person_channels().await.map_err(|e| e.to_string())? {
            if self
                .tunnel_channel_port
                .belongs_to_another_account(&channel)
                .await
            {
                continue;
            }
            let read = store.person_read(&channel).await.unwrap_or_default();
            let peer = match kind {
                ConversationKind::Dm => {
                    let other = self
                        .channel_port
                        .list_members(&channel)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .find(|m| *m != own);
                    match other {
                        Some(other) => Some(self.channel_port.actor_info(&other).await),
                        None => None,
                    }
                }
                ConversationKind::Group => None,
            };
            out.push(crate::ipc::packet::ChannelReadInfo {
                channel,
                kind,
                peer,
                unread: read.unread,
                read: read.mark,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::state::DaemonConfigSnapshot;
    use crate::tunnel::devices::test_device::TestDevice;
    use crate::tunnel::devices::{DeviceEventPayload, DeviceResponsePayload};
    use crate::tunnel::{TunnelDispatcher, TunnelHandle};
    use peko_channel::{CreateOpts, PostMsg};
    use peko_subject::PrincipalId;
    use tokio::sync::mpsc;

    const ACCOUNT: &str = "0b6c7a4e-1d2f-4a3b-8c9d-0e1f2a3b4c5d";
    const PEKO: &str = "156f1a95-d977-4c4a-979e-8ed4848ae6eb";

    /// A runtime whose tunnel is captured, with one paired phone.
    struct Rig {
        _dir: tempfile::TempDir,
        app: AppState,
        outbound: mpsc::Receiver<TunnelMessage>,
        phone: TestDevice,
        runtime: String,
        /// What `ask` saw go out besides its own answer.
        seen: Vec<TunnelMessage>,
    }

    impl Rig {
        async fn new() -> Self {
            let dir = tempfile::TempDir::new().unwrap();
            let data_dir = dir.path().to_path_buf();
            let app = AppState::with_data_dir(
                data_dir.clone(),
                "127.0.0.1".to_string(),
                11435,
                DaemonConfigSnapshot {
                    data_dir: data_dir.clone(),
                    config_dir: data_dir.clone(),
                    log_level: "info".to_string(),
                    launch_mode: crate::daemon::LaunchMode::Headless,
                },
                data_dir,
            )
            .await
            .unwrap();
            // The Hub, as far as these tests need it: it takes every link
            // appended to the device list and passes everything else on.
            let (tx, mut wire) = mpsc::channel::<TunnelMessage>(256);
            let (seen_tx, outbound) = mpsc::channel(256);
            *app.tunnel_handle_slot().write().await = Some(TunnelHandle::new(tx));
            {
                let app = app.clone();
                tokio::spawn(async move {
                    while let Some(message) = wire.recv().await {
                        match message {
                            TunnelMessage::DeviceListAppend {
                                request_id, link, ..
                            } => {
                                let seq: Option<u64> = link
                                    .signed
                                    .split('.')
                                    .nth(1)
                                    .and_then(|p| {
                                        use base64::Engine as _;
                                        base64::engine::general_purpose::URL_SAFE_NO_PAD
                                            .decode(p)
                                            .ok()
                                    })
                                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                                    .and_then(|v| v["seq"].as_u64());
                                app.device_sync().on_appended(&request_id, true, None, seq);
                            }
                            other => {
                                if seen_tx.send(other).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
            let runtime = app.devices.gate.runtime_id().to_string();
            app.store()
                .link_local_user(&runtime, ACCOUNT)
                .await
                .unwrap();
            let rig = Self {
                _dir: dir,
                app,
                outbound,
                phone: TestDevice::new(),
                runtime,
                seen: Vec::new(),
            };
            // ADR-111: the phone is on the person's device list.
            let key = Arc::clone(&rig.app.runtime_signing_key);
            let phone_key = rig.phone.signing.clone();
            let links = crate::security::device_list::test_chain(
                &format!("user:hub:{ACCOUNT}"),
                &[
                    (
                        key.as_ref(),
                        crate::security::device_list::test_computer(&key),
                    ),
                    (&phone_key, rig.phone.list_device()),
                ],
            );
            rig.app
                .principal_manager
                .device_lists()
                .accept_own(
                    &format!("user:hub:{ACCOUNT}"),
                    links,
                    &rig.runtime,
                    &std::collections::HashSet::new(),
                )
                .unwrap();
            rig.app.device_sync().load_phones().await;
            assert!(rig.app.devices.gate.device(&rig.phone.device_id).is_some());
            rig
        }

        fn sent(&mut self) -> Vec<TunnelMessage> {
            let mut out = std::mem::take(&mut self.seen);
            out.extend(std::iter::from_fn(|| self.outbound.try_recv().ok()));
            out
        }

        /// Everything sent until `quiet` passes with nothing new.
        async fn sent_within(&mut self, wait: Duration) -> Vec<TunnelMessage> {
            let mut out = std::mem::take(&mut self.seen);
            while let Ok(Some(message)) = tokio::time::timeout(wait, self.outbound.recv()).await {
                out.push(message);
            }
            out
        }

        /// The phone asks `verb` and reads the runtime's signed answer.
        async fn ask(&mut self, verb: &str, args: Value) -> Result<Value, String> {
            let (request_id, signature) = self.phone.request(&self.runtime, verb, args);
            self.app
                .on_device_message(TunnelMessage::TunnelDeviceRequest {
                    request_id: request_id.clone(),
                    device_id: self.phone.device_id.clone(),
                    runtime_id: self.runtime.clone(),
                    signature,
                })
                .await;
            let mut answer = None;
            for message in self.sent_within(Duration::from_millis(50)).await {
                match message {
                    TunnelMessage::TunnelDeviceResponse {
                        request_id: answered,
                        signature,
                        ..
                    } if answered == request_id => {
                        let read: DeviceResponsePayload =
                            TestDevice::read(&self.runtime, &signature).unwrap();
                        answer = Some(match (read.ok, read.body, read.error) {
                            (true, Some(body), _) => Ok(body),
                            (_, _, Some(error)) => Err(error),
                            other => panic!("malformed answer {other:?}"),
                        });
                    }
                    other => self.seen.push(other),
                }
            }
            answer.unwrap_or_else(|| panic!("no answer to `{verb}`"))
        }

        /// A group with the person and a peko in it.
        async fn group(&self) -> (ChannelId, ActorRef) {
            let peko = PrincipalId("prin_mochi".into());
            let store = self.app.store();
            let channel = store
                .create(&peko, CreateOpts::runtime("trip"))
                .await
                .unwrap();
            store.invite(&channel, &peko, &me()).await.unwrap();
            (channel, ActorRef::from(&peko))
        }

        /// The person's DM with a peko on another computer.
        async fn dm(&self) -> (ChannelId, ActorRef) {
            let peer: ActorRef = format!("principal:hub:{PEKO}").parse().unwrap();
            let channel = crate::tunnel::human_dm_channel_id(PEKO, &format!("user:hub:{ACCOUNT}"));
            self.app
                .store()
                .ensure_human_dm(&me(), &peer, &channel, "Mochi", ACCOUNT)
                .await
                .unwrap();
            (channel, peer)
        }

        async fn peer_posts(&self, channel: &ChannelId, peer: &ActorRef, text: &str) -> String {
            let (_, event) = self
                .app
                .store()
                .post_with_event(channel, peer, PostMsg::root(text))
                .await
                .unwrap();
            match event {
                ChannelEvent::Posted { msg_id, .. } => msg_id.unwrap(),
                other => panic!("{other:?}"),
            }
        }
    }

    /// ADR-114 D8: a phone answers a peko's questions through `post`;
    /// answers that do not fit the card, or that reply to a post, are
    /// refused.
    #[tokio::test]
    async fn a_phone_answers_a_pekos_questions() {
        let mut rig = Rig::new().await;
        let (group, peko) = rig.group().await;
        let card = peko_protocol::channel::Ask {
            for_actor: None,
            questions: vec![peko_protocol::channel::Question {
                text: "Which city?".into(),
                options: vec!["Lisbon".into(), "Porto".into()],
                multi: false,
            }],
        };
        let (_, event) = rig
            .app
            .store()
            .post_with_event(&group, &peko, PostMsg::root("Where?").with_ask(Some(card)))
            .await
            .unwrap();
        let ChannelEvent::Posted {
            msg_id: Some(card_id),
            ..
        } = event
        else {
            panic!("the card has an id");
        };
        let answer = |chosen: &str, n: u32| {
            json!({
                "channel": group.as_str(),
                "text": format!("Which city? → {chosen}"),
                "client_msg_id": format!("a-{n}"),
                "answers": { "to": card_id, "picks": [{ "chosen": [chosen] }] },
            })
        };
        let refused = rig.ask("post", answer("Madrid", 1)).await.unwrap_err();
        assert!(refused.contains("not one of the options"), "{refused}");
        let mut threaded = answer("Lisbon", 2);
        threaded["reply_to"] = json!(card_id);
        let refused = rig.ask("post", threaded).await.unwrap_err();
        assert!(refused.contains("root post"), "{refused}");

        let posted = rig.ask("post", answer("Lisbon", 3)).await.unwrap();
        let page = rig
            .ask(
                "read",
                json!({ "channel": group.as_str(), "after": card_id }),
            )
            .await
            .unwrap();
        let events = page["events"].as_array().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["msg_id"], posted["msg_id"]);
        assert_eq!(events[0]["answers"]["picks"][0]["chosen"][0], "Lisbon");
    }

    #[tokio::test]
    async fn a_paired_phone_reads_posts_and_marks_read_as_the_person() {
        let mut rig = Rig::new().await;
        let (group, peko) = rig.group().await;
        let first = rig.peer_posts(&group, &peko, "Where shall we go?").await;

        let listed = rig.ask("conversations", json!({})).await.unwrap();
        let row = &listed["conversations"][0];
        assert_eq!(row["channel"], group.as_str());
        assert_eq!(
            (row["kind"].as_str(), row["name"].as_str()),
            (Some("group"), Some("trip"))
        );
        assert_eq!(
            (row["unread"].as_u64(), row["working"].as_bool()),
            (Some(1), Some(false))
        );
        assert_eq!(row["last"]["text"], "Where shall we go?");

        let page = rig
            .ask("read", json!({ "channel": group.as_str(), "limit": 500 }))
            .await
            .unwrap();
        assert_eq!(page["events"].as_array().unwrap().len(), 1);
        assert_eq!(page["events"][0]["msg_id"], first.as_str());

        // A post, and the same post sent again after a lost answer.
        let post = json!({ "channel": group.as_str(), "text": "Lisbon", "client_msg_id": "c-1" });
        let posted = rig.ask("post", post.clone()).await.unwrap();
        assert_eq!(posted["repeated"], false);
        let again = rig.ask("post", post).await.unwrap();
        assert_eq!(
            (again["repeated"].as_bool(), &again["msg_id"]),
            (Some(true), &posted["msg_id"])
        );
        let page = rig
            .ask("read", json!({ "channel": group.as_str(), "after": first }))
            .await
            .unwrap();
        let events = page["events"].as_array().unwrap();
        assert_eq!(events.len(), 1, "posted once");
        let me = rig.app.store().actor_info(&me()).await;
        assert_eq!(events[0]["author"].as_str(), Some(me.address.as_str()));

        // Writing reads; the mark went to the person's other computers.
        let read = rig.app.store().person_read(&group).await.unwrap();
        assert_eq!(
            (read.unread, read.mark.as_deref()),
            (0, posted["msg_id"].as_str())
        );
        let second = rig.peer_posts(&group, &peko, "Booked.").await;
        let moved = rig
            .ask(
                "mark_read",
                json!({ "channel": group.as_str(), "msg_id": second }),
            )
            .await
            .unwrap();
        assert_eq!(moved["moved"], true);
        // Not a post's id: refused, and nothing is kept for later.
        let refused = rig
            .ask(
                "mark_read",
                json!({ "channel": group.as_str(), "msg_id": "" }),
            )
            .await
            .unwrap_err();
        assert!(refused.starts_with("[bad_request]"), "{refused}");
        let marks: Vec<_> = rig
            .sent()
            .into_iter()
            .filter_map(|m| match m {
                TunnelMessage::TunnelReadMark {
                    source_principal_did,
                    channel_id,
                    msg_id,
                    ..
                } => Some((source_principal_did, channel_id, msg_id)),
                _ => None,
            })
            .collect();
        assert_eq!(
            marks,
            vec![(
                format!("user:hub:{ACCOUNT}"),
                group.as_str().to_string(),
                second
            )]
        );

        let board = rig
            .ask("board", json!({ "channel": group.as_str() }))
            .await
            .unwrap();
        assert!(board["items"].as_array().unwrap().is_empty());
    }

    /// ADR-108 D8: a phone sends a file in pieces, names it on a post,
    /// and reads it back in pieces; a file is reached only through a post
    /// in one of the person's conversations.
    #[tokio::test]
    async fn a_phone_sends_a_file_and_reads_it_back() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut rig = Rig::new().await;
        let (group, _peko) = rig.group().await;
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();

        // Pieces arrive in order; a repeated piece changes nothing.
        let mut sent = 0usize;
        for piece in bytes.chunks(UPLOAD_CHUNK_BYTES) {
            let answer = rig
                .ask(
                    "upload",
                    json!({ "id": "upload-0001", "offset": sent, "data": b64.encode(piece) }),
                )
                .await
                .unwrap();
            sent += piece.len();
            assert_eq!(answer["received"], sent);
        }
        let again = rig
            .ask(
                "upload",
                json!({ "id": "upload-0001", "offset": 0, "data": b64.encode(&bytes[..10]) }),
            )
            .await
            .unwrap();
        assert_eq!(again["received"], bytes.len());
        let gap = rig
            .ask(
                "upload",
                json!({ "id": "upload-0001", "offset": bytes.len() + 5, "data": b64.encode(b"x") }),
            )
            .await
            .unwrap_err();
        assert!(gap.starts_with("[out_of_order]"), "{gap}");
        let big = rig
            .ask(
                "upload",
                json!({ "id": "upload-0002", "offset": 0, "data": b64.encode(vec![0u8; UPLOAD_CHUNK_BYTES + 1]) }),
            )
            .await
            .unwrap_err();
        assert!(big.starts_with("[bad_request]"), "{big}");
        let odd = rig
            .ask(
                "upload",
                json!({ "id": "../../etc", "offset": 0, "data": b64.encode(b"x") }),
            )
            .await
            .unwrap_err();
        assert!(odd.starts_with("[bad_request]"), "{odd}");

        // A post names what was sent; a file that was not sent is refused
        // and nothing is posted.
        let missing = rig
            .ask(
                "post",
                json!({ "channel": group.as_str(), "text": "", "client_msg_id": "c0",
                        "attachments": [{ "upload": "upload-9999", "name": "a.png" }] }),
            )
            .await
            .unwrap_err();
        assert!(missing.starts_with("[not_found]"), "{missing}");
        let posted = rig
            .ask(
                "post",
                json!({ "channel": group.as_str(), "text": "", "client_msg_id": "c1",
                        "attachments": [{ "upload": "upload-0001", "name": "../my photo.png" }] }),
            )
            .await
            .unwrap();
        let msg_id = posted["msg_id"].as_str().unwrap().to_string();
        let uploads = rig
            .app
            .devices
            .gate
            .uploads_dir()
            .join(&rig.phone.device_id);
        assert!(
            std::fs::read_dir(&uploads).unwrap().next().is_none(),
            "what the phone sent is not kept once posted"
        );

        let page = rig
            .ask("read", json!({ "channel": group.as_str() }))
            .await
            .unwrap();
        let event = page["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["msg_id"] == msg_id.as_str())
            .unwrap();
        let file = &event["attachments"][0];
        assert_eq!(
            (file["name"].as_str(), file["size"].as_u64()),
            (Some("my_photo.png"), Some(bytes.len() as u64))
        );

        // Read back in pieces.
        let mut got = Vec::new();
        loop {
            let piece = rig
                .ask(
                    "file",
                    json!({ "channel": group.as_str(), "msg_id": msg_id, "index": 0, "offset": got.len() }),
                )
                .await
                .unwrap();
            assert_eq!(piece["size"], bytes.len());
            got.extend(b64.decode(piece["data"].as_str().unwrap()).unwrap());
            if piece["done"] == true {
                break;
            }
        }
        assert_eq!(got, bytes);
        assert!(
            got.len() > FILE_CHUNK_BYTES,
            "the file took more than one piece"
        );

        for (args, tag) in [
            (
                json!({ "channel": group.as_str(), "msg_id": msg_id, "index": 1 }),
                "[not_found]",
            ),
            (
                json!({ "channel": group.as_str(), "msg_id": "msg_none", "index": 0 }),
                "[not_found]",
            ),
            (
                json!({ "channel": group.as_str(), "msg_id": msg_id, "index": 0, "offset": bytes.len() + 1 }),
                "[bad_request]",
            ),
        ] {
            let refused = rig.ask("file", args).await.unwrap_err();
            assert!(refused.starts_with(tag), "{refused}");
        }

        // A conversation the person is not in has no files for the phone.
        let peko = PrincipalId("prin_mochi".into());
        let private = rig
            .app
            .store()
            .create(&peko, CreateOpts::runtime("not yours"))
            .await
            .unwrap();
        let refused = rig
            .ask(
                "file",
                json!({ "channel": private.as_str(), "msg_id": msg_id, "index": 0 }),
            )
            .await
            .unwrap_err();
        assert!(refused.starts_with("[not_found]"), "{refused}");
    }

    /// The list is the whole of what a phone may ask, a conversation the
    /// person is not in does not exist for it, and a request is carried
    /// out once.
    #[tokio::test]
    async fn a_phone_asks_only_what_the_list_names() {
        let mut rig = Rig::new().await;
        for verb in [
            "principal_remove",
            "channel_create",
            "credential_set",
            "bash",
        ] {
            let refused = rig.ask(verb, json!({ "name": "mochi" })).await.unwrap_err();
            assert!(refused.starts_with("[unknown_verb]"), "{verb}: {refused}");
        }

        let peko = PrincipalId("prin_mochi".into());
        let private = rig
            .app
            .store()
            .create(&peko, CreateOpts::runtime("not yours"))
            .await
            .unwrap();
        for (verb, args) in [
            ("read", json!({ "channel": private.as_str() })),
            (
                "post",
                json!({ "channel": private.as_str(), "text": "hi", "client_msg_id": "x" }),
            ),
            ("stop", json!({ "channel": private.as_str() })),
            ("board", json!({ "channel": private.as_str() })),
        ] {
            let refused = rig.ask(verb, args).await.unwrap_err();
            assert!(refused.starts_with("[not_found]"), "{verb}: {refused}");
        }
        assert!(
            rig.ask("conversations", json!({})).await.unwrap()["conversations"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        // A browser is reached only with the viewer's verbs, and only a
        // peko's the person owns here (ADR-119 D6).
        let refused = rig
            .ask("browser", json!({ "peko": "mochi", "do": "stop" }))
            .await
            .unwrap_err();
        assert!(refused.starts_with("[bad_request]"), "{refused}");
        let refused = rig
            .ask("browser", json!({ "peko": "mochi", "do": "state" }))
            .await
            .unwrap_err();
        assert!(refused.starts_with("[forbidden]"), "{refused}");
        assert!(rig.ask("browsers", json!({})).await.unwrap()["browsers"]
            .as_array()
            .unwrap()
            .is_empty());

        // The same signed request, relayed twice.
        let (request_id, signature) = rig.phone.request(&rig.runtime, "conversations", json!({}));
        let mut answers = Vec::new();
        for _ in 0..2 {
            rig.app
                .on_device_message(TunnelMessage::TunnelDeviceRequest {
                    request_id: request_id.clone(),
                    device_id: rig.phone.device_id.clone(),
                    runtime_id: rig.runtime.clone(),
                    signature: signature.clone(),
                })
                .await;
            for message in rig.sent_within(Duration::from_millis(50)).await {
                if let TunnelMessage::TunnelDeviceResponse { signature, .. } = message {
                    let answer: DeviceResponsePayload =
                        TestDevice::read(&rig.runtime, &signature).unwrap();
                    answers.push(answer.error);
                }
            }
        }
        assert!(answers[0].is_none());
        assert!(answers[1].as_deref().unwrap().starts_with("[replayed]"));

        // An approval decision names a peko the person owns.
        let refused = rig
            .ask(
                "approval_decide",
                json!({ "peko": "nobody", "id": "apr_1", "approve": true }),
            )
            .await
            .unwrap_err();
        assert!(refused.starts_with("[forbidden]"), "{refused}");
        assert!(rig.ask("approvals", json!({})).await.unwrap()["approvals"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    /// While the app is open the phone gets signed events; while it is
    /// closed, a sealed notification for a DM post it has not read, and a
    /// clear once the person reads it elsewhere.
    #[tokio::test]
    async fn a_phone_is_told_live_and_notified_sealed() {
        let mut rig = Rig::new().await;
        let (dm, mochi) = rig.dm().await;
        let (group, peko) = rig.group().await;
        rig.ask("watch", json!({})).await.unwrap();

        let msg_id = rig.peer_posts(&dm, &mochi, "Your report is ready.").await;
        rig.peer_posts(&group, &peko, "No one is mentioned here.")
            .await;
        let sent = rig.sent_within(Duration::from_millis(400)).await;

        let events: Vec<DeviceEventPayload> = sent
            .iter()
            .filter_map(|m| match m {
                TunnelMessage::TunnelDeviceEvent { signature, .. } => {
                    Some(TestDevice::read(&rig.runtime, signature).unwrap())
                }
                _ => None,
            })
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(events[0].event["kind"], "event");
        assert_eq!(events[0].event["channel"], dm.as_str());
        assert_eq!(events[0].event["event"]["text"], "Your report is ready.");

        let notified: Vec<_> = sent
            .iter()
            .filter_map(|m| match m {
                TunnelMessage::TunnelDeviceNotify {
                    device_id,
                    collapse_key,
                    sealed: Some(sealed),
                    clear: false,
                    ..
                } => Some((device_id.clone(), collapse_key.clone(), sealed.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(notified.len(), 1, "the DM post, not the group post");
        assert_eq!(
            (notified[0].0.as_str(), notified[0].1.as_str()),
            (rig.phone.device_id.as_str(), dm.as_str())
        );
        let opened = rig
            .phone
            .open_notification(&rig.runtime, &notified[0].2)
            .unwrap();
        assert_eq!(opened.body, "Your report is ready.");
        assert_eq!(opened.msg_id.as_deref(), Some(msg_id.as_str()));

        // Read on the computer: the phone clears what it shows.
        rig.app.person_mark_read(&dm, &msg_id).await.unwrap();
        let sent = rig.sent_within(Duration::from_millis(300)).await;
        assert!(sent.iter().any(|m| matches!(
            m,
            TunnelMessage::TunnelDeviceNotify { collapse_key, sealed: None, clear: true, .. }
                if collapse_key == dm.as_str()
        )));
        assert!(sent.iter().any(|m| match m {
            TunnelMessage::TunnelDeviceEvent { signature, .. } => {
                let event: DeviceEventPayload = TestDevice::read(&rig.runtime, signature).unwrap();
                event.event["kind"] == "read"
            }
            _ => false,
        }));

        // A post the person reads within the hold is never notified.
        let quick = rig.peer_posts(&dm, &mochi, "One more thing.").await;
        rig.app.person_mark_read(&dm, &quick).await.unwrap();
        let sent = rig.sent_within(Duration::from_millis(300)).await;
        assert!(!sent
            .iter()
            .any(|m| matches!(m, TunnelMessage::TunnelDeviceNotify { clear: false, .. })));

        // A group post that mentions the person is notified.
        rig.ask("unwatch", json!({})).await.unwrap();
        let me = rig.app.store().actor_info(&me()).await;
        rig.peer_posts(&group, &peko, &format!("@{} which day?", me.address))
            .await;
        let sent = rig.sent_within(Duration::from_millis(400)).await;
        assert!(!sent
            .iter()
            .any(|m| matches!(m, TunnelMessage::TunnelDeviceEvent { .. })));
        let sealed = sent
            .iter()
            .find_map(|m| match m {
                TunnelMessage::TunnelDeviceNotify {
                    sealed: Some(s), ..
                } => Some(s.clone()),
                _ => None,
            })
            .expect("a mention is notified");
        assert_eq!(
            rig.phone
                .open_notification(&rig.runtime, &sealed)
                .unwrap()
                .title,
            "trip"
        );
    }

    /// A read mark from another of the person's runtimes moves the mark
    /// here; one for another account, or not signed by the runtime it
    /// names, does not.
    #[tokio::test]
    async fn a_read_mark_from_the_persons_other_runtime_moves_the_mark() {
        let rig = Rig::new().await;
        let (group, peko) = rig.group().await;
        let msg_id = rig.peer_posts(&group, &peko, "hello").await;
        let other = crate::tunnel::devices::fresh_signing_key();
        let other_id = crate::tunnel::verifying_key_to_did_key(&other.verifying_key());
        let mark = |account: &str, key: &ed25519_dalek::SigningKey| {
            let iat = chrono::Utc::now().timestamp();
            let payload = ReadMarkPayload {
                v: READ_MARK_VERSION.into(),
                request_id: uuid::Uuid::new_v4().to_string(),
                source_runtime_id: other_id.clone(),
                source_principal_did: format!("user:hub:{account}"),
                channel_id: group.as_str().to_string(),
                msg_id: msg_id.clone(),
                iat,
                exp: iat + 300,
            };
            TunnelMessage::TunnelReadMark {
                signature: sign_read_mark(key, &payload),
                request_id: payload.request_id,
                source_runtime_id: payload.source_runtime_id,
                source_principal_did: payload.source_principal_did,
                channel_id: payload.channel_id,
                msg_id: payload.msg_id,
            }
        };
        let unread = || async { rig.app.store().person_read(&group).await.unwrap().unread };

        rig.app
            .on_device_message(mark("someone-else", &other))
            .await;
        assert_eq!(unread().await, 1);
        let forger = crate::tunnel::devices::fresh_signing_key();
        rig.app.on_device_message(mark(ACCOUNT, &forger)).await;
        assert_eq!(unread().await, 1);

        // Through the dispatcher, as PekoHub delivers it: not acted on
        // until that computer is on the person's device list (ADR-111 D5),
        // then applied.
        let dispatcher = TunnelDispatcher::new(Arc::new(rig.app.clone()));
        let handle = rig.app.tunnel_handle_slot().read().await.clone().unwrap();
        dispatcher
            .handle_message(mark(ACCOUNT, &other), handle.clone())
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(unread().await, 1, "held");
        let lists = rig.app.principal_manager.device_lists();
        let person = format!("user:hub:{ACCOUNT}");
        let list = lists.list(&person).unwrap();
        let payload = list.next_payload(crate::security::device_list::Change::Add {
            device: crate::security::device_list::test_computer(&other),
        });
        let me_key = Arc::clone(&rig.app.runtime_signing_key);
        lists
            .accept_after(
                &person,
                vec![crate::security::device_list::Link {
                    by: rig.runtime.clone(),
                    signed: crate::security::device_list::sign_payload(&me_key, &payload),
                    joiner: Some(crate::security::device_list::sign_payload(&other, &payload)),
                }],
            )
            .unwrap();
        dispatcher
            .handle_message(mark(ACCOUNT, &other), handle)
            .await;
        for _ in 0..50 {
            if unread().await == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(unread().await, 0);
    }

    /// The app on this computer (ADR-108 D5): read state names each
    /// conversation's other side, and what has been shown is named by
    /// time, the way an app that draws a thread knows it.
    #[tokio::test]
    async fn the_app_reads_and_moves_the_mark_by_time() {
        use crate::ipc::handlers::RequestDispatcher;
        let rig = Rig::new().await;
        let (dm, peko) = rig.dm().await;
        let peer = crate::ipc::server::PeerAddr::Ip(([127, 0, 0, 1], 0).into());
        let ask = |request: RequestPacket| {
            let app = rig.app.clone();
            let peer = peer.clone();
            async move {
                let sink = Collect(Mutex::new(Vec::new()));
                RequestDispatcher::dispatch(app, request, &CallerContext::local(), &sink, &peer)
                    .await
                    .unwrap();
                sink.0.into_inner().unwrap().pop().unwrap()
            }
        };
        let state = || async {
            match ask(RequestPacket::ChannelReadState { request_id: 1 }).await {
                ResponsePacket::ChannelReadStateResult { channels, .. } => channels
                    .into_iter()
                    .find(|c| c.channel == dm)
                    .expect("the DM is listed"),
                other => panic!("{other:?}"),
            }
        };
        let mark = |msg_id: Option<String>, at: Option<String>| {
            ask(RequestPacket::ChannelMarkRead {
                request_id: 1,
                channel: dm.as_str().to_string(),
                msg_id,
                at,
            })
        };

        let first = rig.peer_posts(&dm, &peko, "one").await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let between = chrono::Utc::now();
        tokio::time::sleep(Duration::from_millis(20)).await;
        rig.peer_posts(&dm, &peko, "two").await;

        let before = state().await;
        assert_eq!(before.kind, ConversationKind::Dm);
        assert_eq!((before.unread, before.read.clone()), (2, None));
        let other = before.peer.expect("a DM names its other member");
        assert_eq!(other.kind, "principal");
        assert_eq!(other.hub_id.as_deref(), Some(PEKO));

        // Seen up to a moment between the two posts: the first is read.
        match mark(None, Some(between.to_rfc3339())).await {
            ResponsePacket::ChannelReadMarked { moved, .. } => assert!(moved),
            other => panic!("{other:?}"),
        }
        let after = state().await;
        assert_eq!((after.unread, after.read), (1, Some(first)));
        // A time before anything was posted moves nothing.
        let early = (between - chrono::Duration::hours(1)).to_rfc3339();
        match mark(None, Some(early)).await {
            ResponsePacket::ChannelReadMarked { moved, .. } => assert!(!moved),
            other => panic!("{other:?}"),
        }
        // Seen up to now: nothing is unread.
        mark(None, Some(chrono::Utc::now().to_rfc3339())).await;
        assert_eq!(state().await.unread, 0);

        // One of the two, and a real time.
        for (msg_id, at) in [
            (None, None),
            (Some("msg_a".to_string()), Some(between.to_rfc3339())),
            (None, Some("yesterday".to_string())),
        ] {
            match mark(msg_id, at).await {
                ResponsePacket::Error { message, .. } => {
                    assert!(message.starts_with("[bad_request]"), "{message}");
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// Only someone at the computer adds or removes a phone; a removed
    /// phone is refused and PekoHub is told.
    #[tokio::test]
    async fn the_owner_lists_and_removes_phones_on_the_computer() {
        use crate::ipc::handlers::RequestDispatcher;
        let mut rig = Rig::new().await;
        let peer = crate::ipc::server::PeerAddr::Ip(([127, 0, 0, 1], 0).into());
        let ask = |request: RequestPacket, caller: CallerContext| {
            let app = rig.app.clone();
            let peer = peer.clone();
            async move {
                let sink = Collect(Mutex::new(Vec::new()));
                RequestDispatcher::dispatch(app, request, &caller, &sink, &peer)
                    .await
                    .unwrap();
                sink.0.into_inner().unwrap().pop().unwrap()
            }
        };
        for request in [
            RequestPacket::DeviceAdd { request_id: 1 },
            RequestPacket::DeviceList { request_id: 1 },
            RequestPacket::IdentityCode {
                request_id: 1,
                of: "account".into(),
            },
            RequestPacket::DeviceRemove {
                request_id: 1,
                device_id: rig.phone.device_id.clone(),
            },
            RequestPacket::ChannelReadState { request_id: 1 },
        ] {
            match ask(request, CallerContext::from_jwt(ACCOUNT.into())).await {
                ResponsePacket::Error { message, .. } => {
                    assert!(message.starts_with("[permission_denied]"), "{message}");
                }
                other => panic!("a remote caller got {other:?}"),
            }
        }
        match ask(
            RequestPacket::DeviceAdd { request_id: 1 },
            CallerContext::local(),
        )
        .await
        {
            ResponsePacket::Error { message, .. } => {
                assert!(message.contains("PekoHub"), "{message}");
            }
            other => panic!("a computer with no tunnel offered {other:?}"),
        }
        match ask(
            RequestPacket::DeviceList { request_id: 1 },
            CallerContext::local(),
        )
        .await
        {
            ResponsePacket::DeviceListResult { view, .. } => {
                assert!(view.on_list);
                assert_eq!(view.devices.len(), 2);
                assert!(view.devices.iter().any(|d| d.this && d.kind == "computer"));
                assert!(view
                    .devices
                    .iter()
                    .any(|d| d.hub_device_id.as_deref() == Some(rig.phone.device_id.as_str())));
                // ADR-112: the account's code is its list's, and the same
                // answer comes from `identity_code`.
                let account = view.account.clone().unwrap();
                let genesis = rig
                    .app
                    .principal_manager
                    .device_lists()
                    .list(&account)
                    .unwrap()
                    .genesis()
                    .to_string();
                let expected = crate::security::identity_code::code_of(
                    crate::security::identity_code::Identity::Person(&genesis),
                );
                assert_eq!(view.code.as_deref(), Some(expected.as_str()));
                match ask(
                    RequestPacket::IdentityCode {
                        request_id: 1,
                        of: "account".into(),
                    },
                    CallerContext::local(),
                )
                .await
                {
                    ResponsePacket::IdentityCodeResult { code, .. } => {
                        assert_eq!(code.as_deref(), Some(expected.as_str()));
                    }
                    other => panic!("{other:?}"),
                }
                for of in ["peko:hub:never-met", "user:hub:someone-else"] {
                    match ask(
                        RequestPacket::IdentityCode {
                            request_id: 1,
                            of: of.into(),
                        },
                        CallerContext::local(),
                    )
                    .await
                    {
                        ResponsePacket::IdentityCodeResult { code: None, .. } => {}
                        other => panic!("{of}: {other:?}"),
                    }
                }
            }
            other => panic!("{other:?}"),
        }
        let removed = ask(
            RequestPacket::DeviceRemove {
                request_id: 1,
                device_id: rig.phone.device_id.clone(),
            },
            CallerContext::local(),
        )
        .await;
        assert!(
            matches!(removed, ResponsePacket::Done { success: true, .. }),
            "{removed:?}"
        );
        assert!(rig.app.devices.gate.device(&rig.phone.device_id).is_none());
        let refused = rig.ask("conversations", json!({})).await.unwrap_err();
        assert!(refused.starts_with("[unpaired]"), "{refused}");
    }

    /// The phone lists the requests waiting for the person on this
    /// computer and decides one: the owner's own action, audited with the
    /// device it came from.
    #[tokio::test]
    async fn a_phone_gives_a_secret_its_peko_asked_for() {
        use crate::principal::config::{
            PrincipalConfig, PrincipalGovernanceConfig, PrincipalIdentityConfig,
            PrincipalIntentConfig, PrincipalRoutingConfig,
        };
        use crate::principal::secret_requests::{self, Target};

        let mut rig = Rig::new().await;
        let roles = rig
            .app
            .config
            .data_dir
            .join("pekos")
            .join("mochi")
            .join("roles");
        tokio::fs::create_dir_all(&roles).await.unwrap();
        tokio::fs::write(
            roles.join("primary.md"),
            "---\ndescription: \"Test\"\n---\n\nYou are mochi.\n",
        )
        .await
        .unwrap();
        let peko = rig
            .app
            .principal_manager()
            .create(PrincipalConfig {
                actor_key: None,
                name: "mochi".into(),
                id: None,
                did: None,
                owner: me(),
                identity: PrincipalIdentityConfig {
                    display_name: Some("mochi".into()),
                    description: None,
                    avatar: None,
                },
                intent: PrincipalIntentConfig::default(),
                governance: PrincipalGovernanceConfig::default(),
                routing: PrincipalRoutingConfig::default(),
                exposure: peko_auth::Exposure::LocalOnly,
                status: None,
                boot_state: None,
                permissions: Vec::new(),
                preferred_model_id: None,
                quota: None,
                children: Default::default(),
            })
            .await
            .unwrap();
        let record = secret_requests::request(
            &peko,
            Target::parse("GITHUB_TOKEN", None).unwrap(),
            "to open pull requests",
            Some("sess-asker".into()),
        )
        .await
        .unwrap();

        let listed = rig.ask("secrets", json!({})).await.unwrap();
        let waiting = listed["secrets"].as_array().unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0]["peko"], "mochi");
        assert_eq!(waiting[0]["id"], record.id.as_str());
        assert_eq!(waiting[0]["secret"], "GITHUB_TOKEN");
        let address = rig
            .app
            .channel_port
            .actor_info(&ActorRef::from(&peko.id))
            .await
            .address;
        assert_eq!(waiting[0]["address"], address.as_str());

        let refused = rig
            .ask(
                "secret_provide",
                json!({ "peko": "someone-elses", "id": record.id, "value": "ghp_phone_value_1" }),
            )
            .await
            .unwrap_err();
        assert!(refused.starts_with("[forbidden]"), "{refused}");
        let given = rig
            .ask(
                "secret_provide",
                json!({ "peko": "mochi", "id": record.id, "value": "ghp_phone_value_1" }),
            )
            .await
            .unwrap();
        assert!(
            given["message"]
                .as_str()
                .unwrap()
                .starts_with("Saved GITHUB_TOKEN"),
            "{given}"
        );
        assert!(!given.to_string().contains("ghp_phone_value_1"));
        let stored = rig
            .app
            .secret_desk()
            .peko_secret(&peko.id.0, "GITHUB_TOKEN")
            .unwrap()
            .unwrap();
        assert_eq!(
            secrecy::ExposeSecret::expose_secret(&stored),
            "ghp_phone_value_1"
        );
        assert!(rig.ask("secrets", json!({})).await.unwrap()["secrets"]
            .as_array()
            .unwrap()
            .is_empty());

        // Declined from the phone too.
        let second = secret_requests::request(
            &peko,
            Target::parse("FLY_API_TOKEN", None).unwrap(),
            "to deploy",
            None,
        )
        .await
        .unwrap();
        let declined = rig
            .ask(
                "secret_decline",
                json!({ "peko": "mochi", "id": second.id }),
            )
            .await
            .unwrap();
        assert!(
            declined["message"]
                .as_str()
                .unwrap()
                .starts_with("Declined"),
            "{declined}"
        );
    }

    #[tokio::test]
    async fn a_phone_decides_an_approval_request_as_the_owner() {
        use crate::principal::config::{
            PrincipalConfig, PrincipalGovernanceConfig, PrincipalIdentityConfig,
            PrincipalIntentConfig, PrincipalRoutingConfig,
        };
        use crate::principal::policy_gate::{pending_requests, PrincipalPolicyGate};
        use crate::security::judge::{GatedCall, PolicyGate as _};
        use peko_tools_core::{ActingFor, Compartment, Origin, Trust};

        let mut rig = Rig::new().await;
        let roles = rig
            .app
            .config
            .data_dir
            .join("pekos")
            .join("mochi")
            .join("roles");
        tokio::fs::create_dir_all(&roles).await.unwrap();
        tokio::fs::write(
            roles.join("primary.md"),
            "---\ndescription: \"Test\"\n---\n\nYou are mochi.\n",
        )
        .await
        .unwrap();
        let peko = rig
            .app
            .principal_manager()
            .create(PrincipalConfig {
                actor_key: None,
                name: "mochi".into(),
                id: None,
                did: None,
                owner: me(),
                identity: PrincipalIdentityConfig {
                    display_name: Some("mochi".into()),
                    description: None,
                    avatar: None,
                },
                intent: PrincipalIntentConfig::default(),
                governance: PrincipalGovernanceConfig::default(),
                routing: PrincipalRoutingConfig::default(),
                exposure: peko_auth::Exposure::LocalOnly,
                status: None,
                boot_state: None,
                permissions: Vec::new(),
                preferred_model_id: None,
                quota: None,
                children: Default::default(),
            })
            .await
            .unwrap();
        let gate = PrincipalPolicyGate::new(Arc::downgrade(rig.app.principal_manager()));
        let id = gate
            .ask_owner(
                &GatedCall {
                    call_id: Some("call_gated".into()),
                    principal_id: peko.id.0.clone(),
                    principal_name: None,
                    session_id: Some("guest-session".into()),
                    agent_id: None,
                    workspace: None,
                    stamp: ActingFor {
                        actor: "user:hub:noah".into(),
                        name: Some("Noah".into()),
                        trust: Trust::Untrusted,
                        compartment: Compartment::Conversation {
                            channel: "dm_x".into(),
                        },
                        origin: Origin::Ingress { channel: None },
                        owner_request: false,
                    },
                    tool_name: "Read".into(),
                    params: json!({ "file_path": "finances/q3.md" }),
                },
                "finances are private",
            )
            .await
            .unwrap();

        let listed = rig.ask("approvals", json!({})).await.unwrap();
        let waiting = listed["approvals"].as_array().unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(
            (waiting[0]["peko"].as_str(), waiting[0]["id"].as_str()),
            (Some("mochi"), Some(id.as_str()))
        );
        assert_eq!(waiting[0]["actor_name"], "Noah");

        let decided = rig
            .ask(
                "approval_decide",
                json!({ "peko": "mochi", "id": id, "approve": false }),
            )
            .await
            .unwrap();
        assert!(
            decided["message"].as_str().unwrap().starts_with("Denied"),
            "{decided}"
        );
        assert!(pending_requests(&peko).is_empty());
        let again = rig
            .ask(
                "approval_decide",
                json!({ "peko": "mochi", "id": id, "approve": true }),
            )
            .await
            .unwrap_err();
        assert!(again.contains("already decided"), "{again}");
    }

    #[tokio::test]
    async fn a_phone_signs_itself_out() {
        let mut rig = Rig::new().await;
        assert_eq!(rig.ask("unpair", json!({})).await.unwrap()["paired"], false);
        assert!(rig.app.devices.gate.devices().is_empty());
        assert!(rig
            .ask("conversations", json!({}))
            .await
            .unwrap_err()
            .starts_with("[unpaired]"));
    }
}
