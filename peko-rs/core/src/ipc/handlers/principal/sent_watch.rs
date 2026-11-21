//! `PrincipalSentWatch`: everything a peko posts in its one-to-one
//! conversations, for the script that relays them somewhere else.
//!
//! `peko send --as visitor:<id>` hands a relay the reply to the message
//! it sent. Anything the peko says later in that conversation (after its
//! owner approves a request, or when a check-in fires) has no request
//! waiting for it; this stream is where the relay finds it. Owner only:
//! it reads every conversation the peko has.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use peko_auth::caller::CallerContext;
use peko_auth::ActorRef;
use peko_channel::{ChannelId, ChannelPort};
use peko_protocol::channel::{ChannelEvent, ConversationKind};
use peko_protocol::ipc::HEARTBEAT_INTERVAL_SECS;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use super::{map_posted_to_log_message, LogSelf, PostCard, PrincipalHost};
use crate::ipc::packet::ResponsePacket;
use crate::ipc::response_sink::ResponseSink;
use crate::ipc::send_response::send_response;
use crate::principal::Principal;

/// What a channel's forwarder hands the stream.
enum Forwarded {
    Event(ChannelId, ChannelEvent),
    Lagged(u64),
}

/// The forwarders of the channels being watched; they stop with the stream.
#[derive(Default)]
struct Forwarders(Vec<JoinHandle<()>>);

impl Drop for Forwarders {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// One conversation of the peko: whom it is with, and how its rows map.
struct Conversation {
    to: ActorRef,
    me: LogSelf,
}

pub(super) async fn handle_principal_sent_watch(
    request_id: u64,
    name: &str,
    since: Option<String>,
    caller: &CallerContext,
    host: &dyn PrincipalHost,
    sink: &dyn ResponseSink,
) -> anyhow::Result<()> {
    let error = |message: String| ResponsePacket::Error {
        request_id,
        message,
    };
    let since = match since.as_deref().map(chrono::DateTime::parse_from_rfc3339) {
        None => None,
        Some(Ok(at)) => Some(at.with_timezone(&chrono::Utc)),
        Some(Err(e)) => {
            send_response(
                sink,
                error(format!("[bad_request] since is not RFC 3339: {e}")),
            )
            .await?;
            return Ok(());
        }
    };
    let manager = host.principal_manager();
    let Some(principal) = manager.get_by_name(name).await else {
        send_response(sink, error(format!("[not_found] Peko '{name}' not loaded"))).await?;
        return Ok(());
    };
    let owner = principal.config.read().await.owner.clone();
    if !caller.is_operator() && caller.subject() != owner {
        send_response(
            sink,
            error("[forbidden] only the peko's owner can follow everything it posts".into()),
        )
        .await?;
        return Ok(());
    }
    let Some(port) = manager.channel_port() else {
        send_response(
            sink,
            error(format!("[not_found] '{name}' has no conversations")),
        )
        .await?;
        return Ok(());
    };
    run(request_id, &principal, port, since, sink).await
}

async fn run(
    request_id: u64,
    principal: &Arc<Principal>,
    port: Arc<dyn ChannelPort>,
    since: Option<chrono::DateTime<chrono::Utc>>,
    sink: &dyn ResponseSink,
) -> anyhow::Result<()> {
    let own = ActorRef::from(&principal.id);
    let own_key = port.key_of(&own);
    let authors: HashSet<String> = [
        principal.id.to_string(),
        port.actor_info(&own).await.address,
    ]
    .into_iter()
    .collect();

    let (tx, mut rx) = mpsc::channel::<Forwarded>(256);
    let mut forwarders = Forwarders::default();
    let mut conversations: HashMap<ChannelId, Conversation> = HashMap::new();
    // Subscribed before the replay reads, so a post in between is not
    // lost; one that lands in both is skipped by its id.
    let new = watch_new_conversations(
        principal,
        &port,
        &own_key,
        &mut conversations,
        &tx,
        &mut forwarders,
    )
    .await;
    let mut replayed: HashSet<String> = HashSet::new();
    if let Some(since) = since {
        let mut rows = Vec::new();
        for channel in new {
            let Ok(events) = port
                .peek_with_ids(&channel, &peko_channel::Checkpoint::zero())
                .await
            else {
                continue;
            };
            for (line, event) in events {
                let ChannelEvent::Posted {
                    ref author, ref at, ..
                } = event
                else {
                    continue;
                };
                if !authors.contains(author) {
                    continue;
                }
                let Ok(when) = chrono::DateTime::parse_from_rfc3339(at) else {
                    continue;
                };
                let when = when.with_timezone(&chrono::Utc);
                if when > since {
                    rows.push((when, channel.clone(), line, event));
                }
            }
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, channel, line, event) in rows {
            if let Some(id) = post_id(&event) {
                replayed.insert(id);
            }
            let Some(packet) = appended(
                request_id,
                &conversations,
                &channel,
                event,
                format!("chan_{line}"),
            ) else {
                continue;
            };
            if send_response(sink, packet).await.is_err() {
                return Ok(());
            }
        }
    }

    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat.tick().await;
    loop {
        tokio::select! {
            forwarded = rx.recv() => match forwarded {
                Some(Forwarded::Event(channel, event)) => {
                    let ChannelEvent::Posted { ref author, .. } = event else {
                        continue;
                    };
                    if !authors.contains(author) {
                        continue;
                    }
                    if let Some(id) = post_id(&event) {
                        if replayed.remove(&id) {
                            continue;
                        }
                    }
                    let fallback = format!("chat_{}", uuid::Uuid::new_v4().simple());
                    let Some(packet) = appended(request_id, &conversations, &channel, event, fallback) else {
                        continue;
                    };
                    if send_response(sink, packet).await.is_err() {
                        return Ok(());
                    }
                }
                Some(Forwarded::Lagged(skipped)) => {
                    let _ = send_response(sink, ResponsePacket::Error {
                        request_id,
                        message: format!(
                            "sent watch lagged; follow again with --since (skipped {skipped} events)"
                        ),
                    })
                    .await;
                    return Ok(());
                }
                None => return Ok(()),
            },
            _ = heartbeat.tick() => {
                // A conversation that started since the last tick (a new
                // visitor) is watched from here on; the first reply in it
                // went back to whoever sent the first message.
                watch_new_conversations(principal, &port, &own_key, &mut conversations, &tx, &mut forwarders).await;
                if send_response(sink, ResponsePacket::Heartbeat { request_id }).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

/// Subscribe to every one-to-one conversation of the peko not watched
/// yet; returns the channels it added.
async fn watch_new_conversations(
    principal: &Arc<Principal>,
    port: &Arc<dyn ChannelPort>,
    own_key: &peko_auth::ActorKey,
    conversations: &mut HashMap<ChannelId, Conversation>,
    tx: &mpsc::Sender<Forwarded>,
    forwarders: &mut Forwarders,
) -> Vec<ChannelId> {
    let Ok(channels) = port.list_for_principal(&principal.id).await else {
        return Vec::new();
    };
    let mut added = Vec::new();
    for channel in channels {
        if conversations.contains_key(&channel) {
            continue;
        }
        if !matches!(
            port.conversation_kind(&channel).await,
            Ok(ConversationKind::Dm)
        ) {
            continue;
        }
        let Ok(members) = port.list_members(&channel).await else {
            continue;
        };
        let Some(to) = members.into_iter().find(|m| port.key_of(m) != *own_key) else {
            continue;
        };
        let mut events = port.subscribe_events(&channel).await;
        let tx = tx.clone();
        let forwarded_channel = channel.clone();
        forwarders.0.push(tokio::spawn(async move {
            loop {
                let item = match events.recv().await {
                    Ok(event) => Forwarded::Event(forwarded_channel.clone(), event),
                    Err(broadcast::error::RecvError::Lagged(n)) => Forwarded::Lagged(n),
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                if tx.send(item).await.is_err() {
                    return;
                }
            }
        }));
        let me = LogSelf::of(principal, port, &channel).await;
        conversations.insert(channel.clone(), Conversation { to, me });
        added.push(channel);
    }
    added
}

fn post_id(event: &ChannelEvent) -> Option<String> {
    match event {
        ChannelEvent::Posted { msg_id, .. } => msg_id.clone(),
        _ => None,
    }
}

/// The packet for one of the peko's posts, or `None` when the post is
/// not in a conversation being watched.
fn appended(
    request_id: u64,
    conversations: &HashMap<ChannelId, Conversation>,
    channel: &ChannelId,
    event: ChannelEvent,
    fallback_id: String,
) -> Option<ResponsePacket> {
    let conversation = conversations.get(channel)?;
    let ChannelEvent::Posted {
        author,
        text,
        at,
        attachments,
        options,
        msg_id,
        ask,
        answers,
        ..
    } = event
    else {
        return None;
    };
    let message = map_posted_to_log_message(
        msg_id.clone().unwrap_or(fallback_id),
        &author,
        &at,
        text,
        attachments,
        PostCard {
            options,
            msg_id,
            ask,
            answers,
        },
        &conversation.me,
    );
    Some(ResponsePacket::PrincipalSentAppended {
        request_id,
        to: conversation.to.clone(),
        channel: channel.to_string(),
        message,
    })
}
