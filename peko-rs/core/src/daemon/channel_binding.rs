//! Channel subscribers and the group-channel wake (ADR-049 Phase 3,
//! ADR-072).
//!
//! Every (principal, channel) membership gets a [`ChannelSubscriber`]
//! that reads new events. What may start a run is narrow: outside
//! groups, **only humans and cron start runs** (ADR-072); in a group, a
//! post starts a run only for a peko it addresses (ADR-128).
//!
//! - **DM channels** (`kind: dm` in `meta.json`) get
//!   [`NoopChannelResponder`]. A user's DM is driven by the ingress path
//!   that posted it (`peko send`, Hub, A2A, IPC steer —
//!   `PeerChildTurns::drive_turn_streaming`). A post by another principal
//!   starts nothing: the principal reads it with ChannelRead (its turn
//!   context lists unread posts) or waits for one with ChannelRead
//!   `watch`.
//! - **Group channels** (`kind: group` — every `group:<slug>` and bare
//!   `chan_*` group) get a [`GroupWakeResponder`] when the principal has
//!   a resolvable model. A post wakes the peko only when it addresses
//!   it (ADR-128 D1): it mentions it (`@<name>`, case-insensitive, whole
//!   word), replies to one of its posts (ADR-126 D7), answers its
//!   question card, or adds a board item for it. That holds for a
//!   person's post and another peko's alike. A person's post that
//!   addresses no one also wakes the only peko in the group, and a peko
//!   whose binding is `human` (a chair, D5); `off` wakes nothing. A
//!   reaction is not a message and wakes no one (ADR-126 D2), and the
//!   principal's own posts never wake it. Everything else is read, not
//!   pushed: an unaddressed post starts nothing and nudges nothing.
//! - **The wake limit** (ADR-128 D2). Wakes and steers that other pekos'
//!   posts cause are counted per peko per group, per rolling hour and
//!   day ([`crate::principal::group_wakes`]). Past a limit the post is
//!   held, and when the limit has room again the peko is woken once with
//!   everything held. A person's post is never counted and never held.
//!   Other pekos' posts found together in one read of the log (this
//!   runtime catching up) are delivered as one wake too (D8).
//! - **Peer-post nudge** (ADR-086), DMs only. A post by another principal
//!   on a DM starts no turn; it brings the recipient's attention-gated
//!   `keepalive` forward through the cron engine ([`PeerPostNudge`]). The
//!   nudge starts no run itself: the keepalive fires on the next poll, at
//!   least two minutes after its last turn and within an hourly budget,
//!   and the attention check decides whether it runs.
//!
//! ## The group wake
//!
//! The wake runs in the principal's per-(principal, channel) standing
//! child session (`/group-<slug>`, provisioned via
//! [`crate::principal::peer_children::ensure_group_child`]), driven by
//! [`PeerTurnDriver`] — the same `drive_turn_streaming` call the ingress
//! paths make: no time limit, the session's conversation context, and a
//! stop handle in the daemon's streaming-run registry. When a person's
//! post started the turn, the responder posts its final text back to the
//! channel as the principal, threaded onto that post. When another
//! peko's post started it, nothing is posted unless the peko posts with
//! `ChannelSend` (ADR-128 D4), and the turn is told so. The turn input is
//! `{author}: {text}` so the principal can tell speakers apart; a reply
//! reads `{author} (replying to {quoted author}: “…”): {text}`, quoting
//! the start of the message it answers from this copy's log (ADR-126
//! D6).
//!
//! - **Run concurrency.** `consider_response` spawns the turn as a
//!   detached task so the subscriber's cursor keeps advancing; turns
//!   for one channel serialize on a per-responder mutex, so a second
//!   mention mid-run queues behind the first.
//! - **A post that meets a running turn (ADR-120 D2).** When a turn
//!   already runs in the session (this responder's, a completion wake,
//!   an `Agent` resume), a post with that turn's standing — the same
//!   stamp (`same_standing`) — is steered into it, as a DM post is: the
//!   running loop takes it in at its next step. A post with other
//!   standing is not: the turn acts for someone else, and the post would
//!   borrow its tools (the owner's request in a shared group would also
//!   lose its ADR-100 standing, seen live: `as_owner` refused). The
//!   running turn gets a note of it instead, and the post waits for the
//!   run to end and gets its own turn, acting for its author, threaded
//!   onto it. What a run ended without taking in is settled before the
//!   next turn (`ResponderInner::settle_leftovers`): a steered post
//!   gets its own turn, a note is dropped. A post with no author falls
//!   back to queued steering and one successor turn per leftover.
//! - **Failures are log-only**: a broken wake must never post errors into
//!   a channel other members read.
//! - **Restart semantics.** Subscriber cursors are persisted and loaded at
//!   spawn, so a restart does not re-fire channel history. A turn in
//!   flight when the daemon dies is not retried.
//!
//! `peko send group:` (the `ChannelPost` IPC handler) only posts; the
//! subscriber's [`GroupWakeResponder`] is the only driver for group
//! posts.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use peko_channel::{
    ChannelCursors, ChannelEvent, ChannelId, ChannelMeter, ChannelPort, ChannelResponder,
    ChannelSubscriber, ConversationKind, NoopChannelResponder, PostMsg, RespondCtx,
    SubscriptionConfig, WakePolicy,
};
use peko_observability::Observability;
use peko_session::manager::SessionManager;
use peko_session::SteeringMessage;
use peko_subject::{ActorRef, PrincipalId};

use crate::daemon::state::StreamingRunHandle;
use crate::principal::child_turns::PeerChildTurns;
use crate::principal::group_wakes::{
    compiled_message, HeldPost, Room, WakeBook, PEKO_STARTED_NOTE, UNPOSTED_REMINDER,
};
use crate::principal::manager::PrincipalManager;
use crate::principal::Principal;
use crate::session::ownership::is_run_active_error;

/// Poll interval while the collision watch waits for another path's
/// run on the bound session to end.
const COMPLETION_POLL_MS: u64 = 50;

/// The daemon's in-flight streaming-run registry (`AppState::streaming_runs`),
/// keyed by session id; `PrincipalStop` cancels runs through it.
pub(crate) type StreamingRuns = Arc<std::sync::Mutex<HashMap<String, StreamingRunHandle>>>;

// ---------------------------------------------------------------------------
// Turn-driver + binding-resolver seams
// ---------------------------------------------------------------------------

/// Drives one LLM turn in an existing session of the principal's tree
/// and returns the final response text. Trait seam so the responder's
/// decision logic is unit-testable without a provider; the production
/// impl is [`PeerTurnDriver`].
///
/// The two collision-seam methods (`steering_inbox`, `has_active_run`)
/// support the run-active collision path: when `drive_turn` refuses
/// with `err_run_active`, the responder converts the post to queued
/// steering via [`Self::steering_inbox`], polls
/// [`Self::has_active_run`] until the in-flight run ends, and drives
/// successor turns for whatever steering was left undrained. They ride this trait (rather than extra fields on
/// `ResponderInner`) because `ResponderInner` already holds the driver
/// and the probe must stay stubbable in tests. The defaults disable
/// the collision path entirely (the wake keeps the log-only skip).
#[async_trait]
pub(crate) trait BoundTurnDriver: Send + Sync + 'static {
    /// Run a turn in `session_id` (a canonical id — the responder
    /// resolves `/`-paths before calling) with `message` as the user
    /// input. `author` is the human whose post started the turn; the
    /// run acts for them (ADR-078). `None` (queued successors and
    /// completion wakes) continues under the session's natural stamp.
    /// Returns the final response text.
    async fn drive_turn(
        &self,
        session_id: &str,
        message: &str,
        author: Option<&ActorRef>,
    ) -> anyhow::Result<String>;

    /// The daemon-shared steering inbox registry this driver's turns
    /// drain, when bound. `None` (the default) disables the run-active
    /// collision conversion — the wake stays log-only.
    fn steering_inbox(&self) -> Option<Arc<peko_session::InboxRegistry>> {
        None
    }

    /// `true` while a run is in flight for `session_id` — the same
    /// check `drive_turn`'s preflight refuses on, keyed by the same
    /// canonical session id. Default: `false`.
    async fn has_active_run(&self, session_id: &str) -> bool {
        let _ = session_id;
        false
    }

    /// The stamp of the run in flight in `session_id` (ADR-120 D2).
    /// `None` (the default) when no run is active, or it is not known
    /// whom it acts for; a post then waits for its own turn.
    async fn running_stamp(&self, session_id: &str) -> Option<peko_tools_core::ActingFor> {
        let _ = session_id;
        None
    }

    /// The stamp a turn `author` starts in `session_id` gets — what
    /// [`Self::drive_turn`] would give it. `None` (the default) when it
    /// cannot be told.
    async fn stamp_for(
        &self,
        session_id: &str,
        author: &ActorRef,
    ) -> Option<peko_tools_core::ActingFor> {
        let _ = (session_id, author);
        None
    }
}

/// Whether two stamps give a run the same standing: whom it acts for, at
/// what trust, where, and whether it is the owner's request (ADR-100).
/// Where the chain started does not matter.
fn same_standing(a: &peko_tools_core::ActingFor, b: &peko_tools_core::ActingFor) -> bool {
    a.actor == b.actor
        && a.trust == b.trust
        && a.compartment == b.compartment
        && a.owner_request == b.owner_request
}

/// What the responder pushed into a running turn (ADR-120 D2), keyed by
/// the steering message's id, so what the turn did not take in is
/// handled once it ends.
#[derive(Debug, Clone)]
enum Pushed {
    /// A post steered into the run: it acts with the run's standing. If
    /// the run ends without taking it in, it gets its own turn, threaded
    /// onto it.
    Steered {
        event_id: String,
        text: String,
        author: ActorRef,
    },
    /// A note that a post with other standing waits for its own turn.
    /// That turn is coming anyway, so a note the run did not take in is
    /// dropped.
    Notice,
}

/// The note a running turn gets for a post that waits for its own turn
/// (ADR-120 D2): it hears the post now, without acting on it.
fn waiting_notice(input: &str) -> String {
    format!(
        "(Posted in the group while you work. It gets its own turn, acting for its author, \
         once this one ends. Keep it in mind, but do not act on it in this turn.)\n{}",
        peko_tools_core::neutralize_runtime_tags(input)
    )
}

/// Resolves the session a channel's turns run in (for a group channel,
/// the principal's per-channel child). Trait seam for the same reason as
/// [`BoundTurnDriver`]; the production impl is [`GroupChildResolver`].
#[async_trait]
pub(crate) trait BindingResolver: Send + Sync + 'static {
    async fn resolve(&self, binding: &str) -> anyhow::Result<String>;
}

// ---------------------------------------------------------------------------
// Shared turn machinery
// ---------------------------------------------------------------------------

/// One channel's turn state: the bound session (resolved once), the
/// turn driver, and the per-channel turn lock.
struct ResponderInner {
    channel: ChannelId,
    principal: PrincipalId,
    /// The raw binding string from `meta.json` (id or `/path`).
    binding: String,
    /// `via` attribution stamped on every reply post this responder
    /// makes: the woken session's slug path, derived from the channel
    /// wire id with the same slug rule as `ensure_group_child`.
    /// Attribution only — never an authority claim.
    via: Option<String>,
    port: Arc<dyn ChannelPort>,
    resolver: Arc<dyn BindingResolver>,
    driver: Arc<dyn BoundTurnDriver>,
    /// Resolved session id cache — the binding is resolved ONCE per
    /// responder (per channel) via `get_or_try_init`, so a failing
    /// resolution retries on the next event but a successful one is
    /// never recomputed.
    resolved_session: tokio::sync::OnceCell<String>,
    /// Serializes turns for this channel: a message arriving mid-run
    /// queues behind the in-flight turn instead of double-running the
    /// bound session.
    turn_lock: tokio::sync::Mutex<()>,
    /// What this responder pushed into running turns and has not seen
    /// settled yet (ADR-120 D2).
    pushed: std::sync::Mutex<HashMap<uuid::Uuid, (std::time::Instant, Pushed)>>,
    /// How often other pekos' posts woke or steered this peko here, and
    /// what is held past the limit (ADR-128 D2).
    wakes: WakeBook,
    /// Other pekos' posts to this peko found so far in the read of the
    /// log now in progress (ADR-128 D8), with their turn inputs.
    batch: std::sync::Mutex<Vec<(RespondCtx, String)>>,
    /// Whether a task already waits to deliver held posts.
    held_timer: std::sync::atomic::AtomicBool,
}

/// Whether a turn `author` started posts its final text by itself
/// (ADR-128 D4). A person's does, and so does a turn no post started;
/// one another peko started posts only what the peko posts.
fn replies_by_itself(author: Option<&ActorRef>) -> bool {
    !matches!(author, Some(ActorRef::Principal(_)))
}

impl ResponderInner {
    /// The session this channel's turns run in, resolved once and
    /// cached. `None` (logged) when resolution fails; the next event
    /// retries.
    async fn session(&self) -> Option<String> {
        match self
            .resolved_session
            .get_or_try_init(|| self.resolver.resolve(&self.binding))
            .await
        {
            Ok(id) => Some(id.clone()),
            Err(e) => {
                warn!(
                    channel = %self.channel,
                    binding = %self.binding,
                    "channel binding: resolution failed (will retry on next event): {e:#}"
                );
                None
            }
        }
    }

    /// Hand a post that wakes this peko to its session (ADR-120 D2).
    /// When a turn already runs there, a post with that turn's standing
    /// is steered into it, as a DM post is; a post with other standing
    /// is shown to it as a note and then gets its own turn, acting for
    /// its author. With no turn running, or none whose standing is
    /// known, the post gets its own turn.
    async fn deliver(&self, event_id: String, text: String, author: Option<ActorRef>) {
        if let Some(author) = author.as_ref() {
            if let Some(session_id) = self.session().await {
                // ADR-123 D2: the owner writing where a run was paused
                // by their hand on the computer lets it continue.
                if crate::computer::paused_session().as_deref() == Some(session_id.as_str())
                    && self
                        .driver
                        .stamp_for(&session_id, author)
                        .await
                        .is_some_and(|s| s.is_owner())
                {
                    crate::computer::owner_spoke(&session_id);
                }
                if self
                    .steer_into_running(&session_id, &event_id, &text, author)
                    .await
                {
                    return;
                }
            }
        }
        self.run_turn(event_id, text, author).await;
    }

    /// ADR-120 D2: push `text` into the run in flight in `session_id`.
    /// Returns `true` when the post was steered (the run acts with its
    /// author's standing); its leftover, if the run ends without taking
    /// it in, is settled here before returning. Returns `false` when the
    /// post still needs its own turn: no run, no steering inbox, or a
    /// run of other standing (which gets a note of the post).
    async fn steer_into_running(
        &self,
        session_id: &str,
        event_id: &str,
        text: &str,
        author: &ActorRef,
    ) -> bool {
        let Some(inbox_registry) = self.driver.steering_inbox() else {
            return false;
        };
        if !self.driver.has_active_run(session_id).await {
            return false;
        }
        let running = self.driver.running_stamp(session_id).await;
        let mine = self.driver.stamp_for(session_id, author).await;
        let steer = matches!((&running, &mine), (Some(r), Some(m)) if same_standing(r, m));
        let (message, pushed) = if steer {
            (
                SteeringMessage::from_peer(text.to_string()),
                Pushed::Steered {
                    event_id: event_id.to_string(),
                    text: text.to_string(),
                    author: author.clone(),
                },
            )
        } else {
            // Marked as someone in the conversation writing, so a long
            // call the run waits in (a `ChannelRead` watch, which no longer
            // returns this post) moves to the background and the run ends:
            // the post's own turn waits for it.
            (
                SteeringMessage::from_peer(waiting_notice(text)),
                Pushed::Notice,
            )
        };
        self.pushed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(message.id, (std::time::Instant::now(), pushed));
        inbox_registry
            .get_or_create(session_id)
            .await
            .push(message.into())
            .await;
        debug!(
            channel = %self.channel,
            session = %session_id,
            steer,
            "channel binding: post reached a running turn"
        );
        if !steer {
            return false;
        }
        // Whatever the run did not take in gets handled once it ends.
        let _turn_guard = self.turn_lock.lock().await;
        self.wait_for_no_run(session_id).await;
        self.settle_leftovers(session_id).await;
        true
    }

    /// Wait until no run is in flight in `session_id`. No deadline: runs
    /// have no time limit, and stopping the run ends the wait.
    async fn wait_for_no_run(&self, session_id: &str) {
        while self.driver.has_active_run(session_id).await {
            tokio::time::sleep(Duration::from_millis(COMPLETION_POLL_MS)).await;
        }
    }

    /// One full passive-binding cycle for `text`: resolve the binding
    /// (cached), drive the turn (serialized per channel), post the
    /// reply threaded onto the triggering event (`event_id`). Failures
    /// are log-only: a broken wake must never post errors into a channel
    /// other members read.
    async fn run_turn(&self, event_id: String, text: String, author: Option<ActorRef>) {
        let _turn_guard = self.turn_lock.lock().await;

        let Some(session_id) = self.session().await else {
            return;
        };
        // Settle what an ended run left behind (stale notes, steered
        // posts it never took in) before this post's own turn reads the
        // inbox.
        if !self.driver.has_active_run(&session_id).await {
            self.settle_leftovers(&session_id).await;
        }

        match self
            .driver
            .drive_turn(&session_id, &text, author.as_ref())
            .await
        {
            Ok(reply) => {
                if replies_by_itself(author.as_ref()) {
                    self.post_reply(&session_id, event_id, &reply).await;
                } else {
                    self.remind_unposted(&session_id, author.as_ref(), &reply)
                        .await;
                }
            }
            Err(e) => {
                // Run-active collision: another path (Agent-tool
                // resume, completion wake) is driving a run on the
                // bound session. A person's post gets its own turn once
                // that run ends; one with no author is queued as
                // steering — see the module docs' "Run-active
                // collision" bullet. Every other failure keeps the
                // log-only skip.
                if is_run_active_error(&e) {
                    match author {
                        Some(author) if self.driver.steering_inbox().is_some() => {
                            self.turn_after_active_run(&session_id, event_id, &text, &author)
                                .await;
                        }
                        _ => self.handle_busy_collision(&session_id, &text).await,
                    }
                } else {
                    warn!(
                        channel = %self.channel,
                        session = %session_id,
                        "channel binding: turn failed (posting nothing): {e:#}"
                    );
                }
            }
        }
    }

    /// Post a wake turn's reply threaded onto the post that started it
    /// (`event_id`), unless the turn chose to say nothing.
    async fn post_reply(&self, session_id: &str, event_id: String, reply: &str) {
        // Only group channels get a responder (`select_responder`).
        let Some(reply) = crate::principal::quiet_reply::postable(reply, true) else {
            debug!(
                channel = %self.channel,
                session = %session_id,
                "channel binding: turn produced no reply; posting nothing"
            );
            return;
        };
        // Phase 12a: the reply is threaded onto the triggering
        // event so `response_trigger`'s root-post-only rule
        // never reacts to it — here (self-author skip already
        // covers the local echo) and on every remote mirror the
        // reply fans out to. `via` stamps the bound session's
        // slug path (see the `ResponderInner::via` docs).
        let mut msg = PostMsg::reply(event_id, reply);
        msg.via = self.via.clone();
        if let Err(e) = self
            .port
            .post(&self.channel, &ActorRef::from(&self.principal), msg)
            .await
        {
            warn!(
                channel = %self.channel,
                "channel binding: reply post failed: {e}"
            );
        }
    }

    /// Post a successor turn's reply as a root post (its post's
    /// correlation was lost to steering).
    async fn post_root(&self, reply: &str) {
        let Some(reply) = crate::principal::quiet_reply::postable(reply, true) else {
            return;
        };
        let mut msg = PostMsg::root(reply);
        msg.via = self.via.clone();
        if let Err(e) = self
            .port
            .post(&self.channel, &ActorRef::from(&self.principal), msg)
            .await
        {
            warn!(
                channel = %self.channel,
                "channel binding: successor-turn reply post failed: {e}"
            );
        }
    }

    /// A post met a run another path drives on the bound
    /// session. Wait for that run to end, then drive the post's own
    /// turn, acting for `author` (ADR-078; for the owner in a shared
    /// group that is an ADR-100 owner request), and thread the reply
    /// onto the post. Should yet another run have started in between,
    /// wait for that one too. Still holding `turn_lock`, so the
    /// channel's posts keep their order. No deadline: runs have no time
    /// limit, and stopping the active run ends the wait.
    async fn turn_after_active_run(
        &self,
        session_id: &str,
        event_id: String,
        text: &str,
        author: &ActorRef,
    ) {
        debug!(
            channel = %self.channel,
            session = %session_id,
            "channel binding: run-active collision; the post waits for its own turn"
        );
        loop {
            self.wait_for_no_run(session_id).await;
            self.settle_leftovers(session_id).await;
            match self.driver.drive_turn(session_id, text, Some(author)).await {
                Ok(reply) => {
                    if replies_by_itself(Some(author)) {
                        self.post_reply(session_id, event_id, &reply).await;
                    } else {
                        self.remind_unposted(session_id, Some(author), &reply).await;
                    }
                    return;
                }
                Err(e) if is_run_active_error(&e) => {
                    tokio::time::sleep(Duration::from_millis(COMPLETION_POLL_MS)).await;
                }
                Err(e) => {
                    warn!(
                        channel = %self.channel,
                        session = %session_id,
                        "channel binding: turn after the active run failed (posting nothing): {e:#}"
                    );
                    return;
                }
            }
        }
    }

    /// Handle the steering an ended run left in `session_id`'s inbox
    /// (the IPC ingress path's "Gap-2" race: steering pushed during a
    /// run's FINAL iteration is never drained by it). Call with no run
    /// in flight and `turn_lock` held. Atomically drains STEERING only —
    /// completion events stay queued (P0-2, 2026-09-27) — and then, in
    /// order:
    ///
    /// - a post this responder steered (ADR-120 D2) gets its own turn,
    ///   acting for its author, threaded onto it;
    /// - a note this responder pushed is dropped (its post's own turn
    ///   follows anyway);
    /// - anything else (a post with no author, converted to steering)
    ///   gets one successor turn, posted as a ROOT post: its event
    ///   correlation was lost, so there is nothing to thread onto.
    ///   Principal-authored root posts wake no one unless they address
    ///   a peko, which that peko's wake limit bounds (ADR-128 D2).
    ///
    /// Failures are log-only. No re-queue: the steering was consumed by
    /// the drain, and an infinite retry loop is worse than the log-only
    /// skip every other failure gets.
    async fn settle_leftovers(&self, session_id: &str) {
        let Some(inbox_registry) = self.driver.steering_inbox() else {
            return;
        };
        let Some(inbox) = inbox_registry.peek_inbox(session_id).await else {
            return;
        };
        let drained_at = std::time::Instant::now();
        let leftovers = inbox.drain_steering().await;
        for envelope in leftovers {
            let pushed = self
                .pushed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&envelope.id)
                .map(|(_, pushed)| pushed);
            match pushed {
                Some(Pushed::Notice) => {}
                Some(Pushed::Steered {
                    event_id,
                    text,
                    author,
                }) => match self
                    .driver
                    .drive_turn(session_id, &text, Some(&author))
                    .await
                {
                    Ok(reply) => {
                        if replies_by_itself(Some(&author)) {
                            self.post_reply(session_id, event_id, &reply).await;
                        } else {
                            self.remind_unposted(session_id, Some(&author), &reply)
                                .await;
                        }
                    }
                    Err(e) => warn!(
                        channel = %self.channel,
                        session = %session_id,
                        "channel binding: turn for a steered post the run left failed \
                         (posting nothing): {e:#}"
                    ),
                },
                None => match self
                    .driver
                    .drive_turn(session_id, &envelope.content, None)
                    .await
                {
                    Ok(reply) => self.post_root(&reply).await,
                    Err(e) => warn!(
                        channel = %self.channel,
                        session = %session_id,
                        "channel binding: successor turn failed (posting nothing): {e:#}"
                    ),
                },
            }
        }
        // What the ended run took in itself is settled too; anything
        // pushed since the drain belongs to a newer run.
        self.pushed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, (at, _)| *at > drained_at);
    }

    /// Run-active collision recovery for a post with no author: the
    /// bound session already has a run in flight, so the wake could not
    /// drive its own turn. Push the post into the session's
    /// daemon-shared steering inbox — the running loop drains steering
    /// as user-role input at its next iteration boundary, whichever
    /// path drives that run — then, once the run ends, settle what it
    /// left ([`Self::settle_leftovers`]). Still holding `turn_lock`, so
    /// per-channel ordering is preserved. The watch has no deadline:
    /// runs have no time limit, and stopping the active run
    /// (`peko stop`, `Async stop`) ends it.
    async fn handle_busy_collision(&self, session_id: &str, text: &str) {
        let Some(inbox_registry) = self.driver.steering_inbox() else {
            warn!(
                channel = %self.channel,
                session = %session_id,
                "channel binding: run-active collision but no steering inbox is bound; \
                 dropping the wake (log-only)"
            );
            return;
        };

        // Same call shape as the IPC ingress path
        // (`PrincipalManager`'s serial-queue fallback).
        inbox_registry
            .get_or_create(session_id)
            .await
            .push(SteeringMessage::from_peer(text.to_string()).into())
            .await;
        debug!(
            channel = %self.channel,
            session = %session_id,
            "channel binding: run-active collision converted to queued steering"
        );

        self.wait_for_no_run(session_id).await;
        self.settle_leftovers(session_id).await;
        let Some(inbox) = inbox_registry.peek_inbox(session_id).await else {
            return;
        };

        // §6.1b: completions that raced the predecessor's final
        // iteration (or landed while the collision watch was waiting)
        // are still queued. Drive chained wake turns — each drains the
        // inbox at its first iteration — until it's empty, posting each
        // reply to the channel like a steering successor. Bounded so a
        // turn that keeps producing completions cannot chain forever.
        let mut chained = 0u8;
        const MAX_POST_RUN_WAKE_CHAIN: u8 = 4;
        while chained < MAX_POST_RUN_WAKE_CHAIN {
            if inbox.is_empty().await {
                break;
            }
            chained += 1;
            match self
                .driver
                .drive_turn(
                    session_id,
                    crate::async_exec::executor::wake::WAKE_TURN_MARKER,
                    None,
                )
                .await
            {
                Ok(reply) => self.post_root(&reply).await,
                Err(e) => {
                    warn!(
                        channel = %self.channel,
                        session = %session_id,
                        "channel binding: completion-wake turn failed (posting nothing): {e:#}"
                    );
                    break;
                }
            }
        }
    }
}

impl ResponderInner {
    /// A turn another peko started ended with `reply` and posted nothing
    /// (a turn that posted in the group has no final text left,
    /// `drive_conversation_turn`). When the reply is one a person's turn
    /// would have posted, ask the peko once whether the group needs it
    /// (ADR-128 D4): it posts it with `ChannelSend`, or says `NO_REPLY`.
    /// Whatever it answers, nothing is posted from here.
    async fn remind_unposted(&self, session_id: &str, author: Option<&ActorRef>, reply: &str) {
        if crate::principal::quiet_reply::postable(reply, true).is_none() {
            return;
        }
        debug!(
            channel = %self.channel,
            session = %session_id,
            "channel binding: a turn another peko started wrote a reply and posted nothing; \
             asking once"
        );
        if let Err(e) = self
            .driver
            .drive_turn(session_id, UNPOSTED_REMINDER, author)
            .await
        {
            debug!(
                channel = %self.channel,
                session = %session_id,
                "channel binding: the reminder turn did not run: {e:#}"
            );
        }
    }

    /// One turn for several posts other pekos wrote to this peko
    /// (ADR-128 D2, D8). It acts for the conversation itself (no single
    /// author), waits for a run in flight to end, and posts nothing by
    /// itself: the peko posts what the group needs with `ChannelSend`.
    async fn run_compiled(&self, message: String) {
        let _turn_guard = self.turn_lock.lock().await;
        let Some(session_id) = self.session().await else {
            return;
        };
        loop {
            self.wait_for_no_run(&session_id).await;
            self.settle_leftovers(&session_id).await;
            match self.driver.drive_turn(&session_id, &message, None).await {
                Ok(reply) => {
                    self.remind_unposted(&session_id, None, &reply).await;
                    return;
                }
                Err(e) if is_run_active_error(&e) => {
                    tokio::time::sleep(Duration::from_millis(COMPLETION_POLL_MS)).await;
                }
                Err(e) => {
                    warn!(
                        channel = %self.channel,
                        session = %session_id,
                        "channel binding: turn for held posts failed (posting nothing): {e:#}"
                    );
                    return;
                }
            }
        }
    }

    /// Make sure a task waits to deliver what the wake limit holds
    /// (ADR-128 D2): it sleeps until the limit has room, or until the
    /// book changes (the owner lifted the limit), and then wakes the peko
    /// once with everything held. Ends when nothing is held.
    fn ensure_held_timer(self: &Arc<Self>) {
        use std::sync::atomic::Ordering;
        if self.held_timer.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let now = chrono::Utc::now();
                let status = this.wakes.status(now);
                if status.held == 0 {
                    break;
                }
                // Registered before the room check, so a change made in
                // between is not missed.
                let changed = crate::principal::group_wakes::changed();
                match status.room {
                    Room::Now => {
                        if let Some(held) = this.wakes.take_held(now) {
                            let lines: Vec<String> =
                                held.posts.into_iter().map(|post| post.line).collect();
                            debug!(
                                channel = %this.channel,
                                principal = %this.principal,
                                posts = lines.len(),
                                "channel binding: the wake limit has room; delivering held posts"
                            );
                            this.run_compiled(compiled_message(&lines, held.dropped, true))
                                .await;
                        }
                    }
                    Room::At(at) => {
                        let wait = (at - now).to_std().unwrap_or_default();
                        tokio::select! {
                            () = tokio::time::sleep(wait) => {}
                            () = changed => {}
                        }
                    }
                    Room::Never => changed.await,
                }
            }
            this.held_timer.store(false, Ordering::SeqCst);
            // A post held between the last check and here.
            if this.wakes.status(chrono::Utc::now()).held > 0 {
                this.ensure_held_timer();
            }
        });
    }
}

/// Detached-task spawn: the subscriber's
/// cursor advance + persistence must not block on an LLM turn (see
/// module docs). The turn lock inside `run_turn` serializes concurrent
/// arrivals.
fn spawn_turn(inner: Arc<ResponderInner>, ctx: RespondCtx, input: String) {
    let author = match &ctx.event {
        ChannelEvent::Posted { author, .. } => ActorRef::from_str(author).ok(),
        _ => None,
    };
    tokio::spawn(async move { inner.deliver(ctx.event_id, input, author).await });
}

// ---------------------------------------------------------------------------
// GroupWakeResponder (ADR-049 Phase 3, D4)
// ---------------------------------------------------------------------------

/// `ChannelResponder` for group-tier (unbound) channels. See the
/// module docs' "ADR-049 Phase 3" section for the design; the trigger
/// is [`Self::response_trigger`].
///
/// The binding slot carries the channel's wire id and the resolver
/// find-or-creates the per-(principal, channel) group child session; the
/// reply is posted threaded onto the triggering post, as the principal.
#[derive(Clone)]
pub(crate) struct GroupWakeResponder {
    inner: Arc<ResponderInner>,
    /// The principal's name: a post addresses it when it mentions
    /// `@<name>`.
    name: String,
}

impl GroupWakeResponder {
    pub(crate) fn new(
        channel: ChannelId,
        principal: PrincipalId,
        name: String,
        port: Arc<dyn ChannelPort>,
        resolver: Arc<dyn BindingResolver>,
        driver: Arc<dyn BoundTurnDriver>,
        wakes_dir: &Path,
    ) -> Self {
        let wakes = WakeBook::open(wakes_dir, &channel);
        // The bound child is `/group-<slug>` under the trunk; derive
        // the path with the same slug rule `ensure_group_child`
        // applies, then add the display-side `sess:` scheme prefix so
        // `via` equals what `compute_path` renders for that session
        // (modulo a `-N` suffix on the rare sanitized-slug collision
        // between two channels — attribution only).
        let via = format!(
            "{}/{}",
            peko_session::path::SCHEME_PREFIX,
            crate::principal::peer_children::group_child_base_slug(channel.as_str())
        );
        Self {
            inner: Arc::new(ResponderInner {
                binding: channel.as_str().to_string(),
                channel,
                principal,
                via: Some(via),
                port,
                resolver,
                driver,
                resolved_session: tokio::sync::OnceCell::new(),
                turn_lock: tokio::sync::Mutex::new(()),
                pushed: std::sync::Mutex::new(HashMap::new()),
                wakes,
                batch: std::sync::Mutex::new(Vec::new()),
                held_timer: std::sync::atomic::AtomicBool::new(false),
            }),
            name,
        }
    }

    /// Pick up posts the wake limit held before this runtime stopped
    /// (ADR-128 D2). Call inside the tokio runtime.
    pub(crate) fn resume_held(&self) {
        if self.inner.wakes.status(chrono::Utc::now()).held > 0 {
            self.inner.ensure_held_timer();
        }
    }

    /// Another peko's post (ADR-120 D1): `(author, text)` for a post by
    /// a principal that is not this one.
    fn peer_post<'e>(&self, event: &'e ChannelEvent) -> Option<(&'e str, &'e str)> {
        match event {
            ChannelEvent::Posted { author, text, .. }
                if matches!(ActorRef::from_str(author), Ok(ActorRef::Principal(_)))
                    && !self.is_me(author) =>
            {
                Some((author.as_str(), text.as_str()))
            }
            _ => None,
        }
    }

    /// Whether another peko's post asks this one for something (ADR-120
    /// D1): it mentions it, adds a board item for it, or replies to one
    /// of its posts.
    async fn peer_asks_me(&self, event: &ChannelEvent, text: &str) -> bool {
        let ChannelEvent::Posted {
            board, reply_to, ..
        } = event
        else {
            return false;
        };
        if let Some(op) = board {
            return board_asks(self.inner.port.as_ref(), op, &self.inner.principal);
        }
        if let Some(parent) = reply_to {
            if self.asked_by_me(parent).await {
                return true;
            }
        }
        self.mentions_me(text).await
    }

    /// Whether `text` mentions this peko: its address, its Hub address,
    /// or its short name when that is unique in the group.
    async fn mentions_me(&self, text: &str) -> bool {
        let me = self
            .inner
            .port
            .actor_info(&ActorRef::from(&self.inner.principal))
            .await;
        if peko_channel::mentions(text, &me.address)
            || me
                .hub_address()
                .is_some_and(|h| peko_channel::mentions(text, &h))
        {
            return true;
        }
        if !peko_channel::mentions(text, &self.name) {
            return false;
        }
        match self.inner.port.participants(&self.inner.channel).await {
            Ok(roster) => peko_channel::short_name_unambiguous(&roster, &self.name, &me.address),
            Err(_) => false,
        }
    }

    /// Another peko's post (ADR-128 D1, D2): when it addresses this peko
    /// and the binding wakes at all, it joins the posts gathered in this
    /// read of the log ([`Self::flush_peer_posts`] acts on them).
    /// Anything else starts nothing and nudges nothing.
    async fn consider_peer_post(&self, ctx: RespondCtx, wake: WakePolicy) {
        let Some((_, text)) = self.peer_post(&ctx.event) else {
            return;
        };
        if wake == WakePolicy::Off || !self.peer_asks_me(&ctx.event, text).await {
            return;
        }
        let input = turn_input(self.inner.port.as_ref(), &self.inner.channel, &ctx.event).await;
        self.inner
            .batch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((ctx, input));
    }

    /// Act on the other pekos' posts to this peko that one read of the
    /// log brought (ADR-128 D2, D8). One post wakes the peko, or is
    /// steered into its running turn, acting for its author; several are
    /// delivered as one wake. Either spends one wake. When the limit has
    /// no room, or earlier posts are still held, they are held too.
    fn flush_peer_posts(&self) {
        let mut batch = std::mem::take(
            &mut *self
                .inner
                .batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if batch.is_empty() {
            return;
        }
        let now = chrono::Utc::now();
        if !self.inner.wakes.spend(now) {
            debug!(
                channel = %self.inner.channel,
                principal = %self.inner.principal,
                posts = batch.len(),
                "channel binding: the limit of wakes by other pekos is reached; holding"
            );
            let held = batch
                .into_iter()
                .map(|(ctx, line)| HeldPost {
                    event_id: ctx.event_id,
                    line,
                    at: now,
                })
                .collect();
            self.inner.wakes.hold(now, held);
            self.inner.ensure_held_timer();
            return;
        }
        if batch.len() == 1 {
            let (ctx, input) = batch.remove(0);
            spawn_turn(
                Arc::clone(&self.inner),
                ctx,
                format!("{input}\n{PEKO_STARTED_NOTE}"),
            );
            return;
        }
        let lines: Vec<String> = batch.into_iter().map(|(_, line)| line).collect();
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            inner.run_compiled(compiled_message(&lines, 0, false)).await;
        });
    }

    /// Whether this peko is the only one in the group (ADR-128 D1): a
    /// person's post there can be for no other peko. Read from this
    /// runtime's copy of the roster; when it cannot be read, the peko is
    /// not taken to be the only one.
    async fn only_peko_here(&self) -> bool {
        let Ok(roster) = self.inner.port.participants(&self.inner.channel).await else {
            return false;
        };
        let pekos = roster
            .iter()
            .filter(|actor| {
                matches!(
                    ActorRef::from_str(&actor.address),
                    Ok(ActorRef::Principal(_))
                )
            })
            .count();
        pekos == 1
    }

    /// The wake filter. Returns the turn input to act on, or `None` to
    /// drop the event. Fires ONLY on a `user:*`-authored ROOT post that
    /// mentions this principal (`@<name>`, case-insensitive, whole word):
    ///
    /// - Only humans start runs (ADR-072): principal-authored posts
    ///   (bare ids AND `principal:<did>` forms) never trigger, so
    ///   principals cannot wake each other into a loop; `public` is not
    ///   a user. The principal's own posts are principal-authored, so
    ///   self-suppression falls out of the same rule.
    /// - A reply is weighed like a root post (ADR-126 D7); a reply to
    ///   this principal's post wakes it whether or not it mentions it
    ///   (see `consider_response`).
    /// - Unmentioned posts don't wake anyone; the principal reads them
    ///   with ChannelRead (its turn context lists unread posts).
    ///
    /// The input is `{author}: {text}` so the woken session can tell
    /// speakers apart in a multi-party channel (see module docs).
    fn response_trigger(name: &str, event: &ChannelEvent) -> Option<String> {
        Self::human_post(event)
            .filter(|(_, text)| peko_channel::mentions(text, name))
            .map(|(author, text)| format!("{author}: {text}"))
    }

    /// Whether this channel's post `msg_id` is this principal's.
    async fn asked_by_me(&self, msg_id: &str) -> bool {
        let Ok(Some((_, ChannelEvent::Posted { author, .. }))) =
            self.inner.port.find_post(&self.inner.channel, msg_id).await
        else {
            return false;
        };
        self.is_me(&author)
    }

    /// Whether `author` is this principal.
    fn is_me(&self, author: &str) -> bool {
        let me = ActorRef::from(&self.inner.principal);
        author == self.inner.principal.0
            || author
                .parse::<ActorRef>()
                .is_ok_and(|a| self.inner.port.key_of(&a) == self.inner.port.key_of(&me))
    }

    /// Whether `author` is whom a card is `for`: that person, or any
    /// person when the card names no one.
    fn is_addressee(&self, author: &str, for_actor: Option<&str>) -> bool {
        let Ok(person) = author.parse::<ActorRef>() else {
            return false;
        };
        if !matches!(person, ActorRef::User(_) | ActorRef::Visitor(_)) {
            return false;
        }
        match for_actor.and_then(|f| f.parse::<ActorRef>().ok()) {
            Some(wanted) => self.inner.port.key_of(&person) == self.inner.port.key_of(&wanted),
            None => for_actor.is_none(),
        }
    }

    /// ADR-114 D5: whether `event` answers this principal's open question
    /// card in its own words — a root post by whom the card is for, while
    /// the principal's newest card here is still open (not answered,
    /// replaced, or already answered in words). Such a post wakes the
    /// asker as an answer through the card does, and closes the card.
    async fn answers_my_open_card(&self, event: &ChannelEvent, event_id: &str) -> bool {
        let Some((author, _)) = Self::human_root_post(event) else {
            return false;
        };
        let Ok(page) = self
            .inner
            .port
            .peek_tail(
                &self.inner.channel,
                CARD_LOOKBACK,
                Some(&event_id.to_string()),
            )
            .await
        else {
            return false;
        };
        // The open card, as (its msg_id, whom it is for).
        let mut open: Option<(Option<String>, Option<String>)> = None;
        for (_, earlier) in &page.events {
            let ChannelEvent::Posted {
                author: by,
                msg_id,
                ask,
                answers,
                ..
            } = earlier
            else {
                continue;
            };
            if self.is_me(by) {
                if let Some(ask) = ask {
                    open = Some((msg_id.clone(), ask.for_actor.clone()));
                }
                continue;
            }
            let Some((card_id, for_actor)) = &open else {
                continue;
            };
            let answered = answers
                .as_ref()
                .is_some_and(|a| card_id.as_deref() == Some(a.to.as_str()));
            let in_words = Self::human_root_post(earlier).is_some()
                && self.is_addressee(by, for_actor.as_deref());
            if answered || in_words {
                open = None;
            }
        }
        open.is_some_and(|(_, for_actor)| self.is_addressee(author, for_actor.as_deref()))
    }

    /// A human-authored (user or visitor) message, root post or reply —
    /// what a [`WakePolicy::Human`] binding wakes on (ADR-126 D7).
    /// Principal posts and reactions never qualify, whatever the policy
    /// (ADR-072, ADR-126 D2).
    fn human_post(event: &ChannelEvent) -> Option<(&str, &str)> {
        match event {
            ChannelEvent::Posted {
                author,
                text,
                react: None,
                ..
            } if matches!(
                ActorRef::from_str(author),
                Ok(ActorRef::User(_) | ActorRef::Visitor(_))
            ) =>
            {
                Some((author.as_str(), text.as_str()))
            }
            _ => None,
        }
    }

    /// A human-authored (user or visitor) root post: what answers a
    /// question card in the person's own words (ADR-114 D5), and how a
    /// board change by a person is told apart.
    fn human_root_post(event: &ChannelEvent) -> Option<(&str, &str)> {
        match event {
            ChannelEvent::Posted {
                author,
                text,
                parent: None,
                ..
            } if matches!(
                ActorRef::from_str(author),
                Ok(ActorRef::User(_) | ActorRef::Visitor(_))
            ) =>
            {
                Some((author.as_str(), text.as_str()))
            }
            _ => None,
        }
    }
}

#[async_trait]
impl ChannelResponder for GroupWakeResponder {
    async fn consider_response(&self, ctx: RespondCtx) -> peko_channel::Result<()> {
        let last_of_read = ctx.more == 0;
        let considered = self.consider(ctx).await;
        // ADR-128 D8: other pekos' posts gathered in this read of the log
        // are acted on together, once the read is through.
        if last_of_read {
            self.flush_peer_posts();
        }
        considered
    }
}

impl GroupWakeResponder {
    /// Weigh one event (see the module docs' group bullet).
    async fn consider(&self, ctx: RespondCtx) -> peko_channel::Result<()> {
        // ADR-126 D2: a reaction wakes no one.
        if !ctx.event.is_message() {
            return Ok(());
        }
        // ADR-082 D3: read the policy per post, so `Channel wake` takes
        // effect without respawning the subscriber.
        let wake = self
            .inner
            .port
            .binding_for(&self.inner.channel, &self.inner.principal)
            .await?
            .map_or(WakePolicy::Mention, |b| b.wake);
        // ADR-128 D1: another peko's post wakes this one only when it
        // addresses it.
        if self.peer_post(&ctx.event).is_some() {
            self.consider_peer_post(ctx, wake).await;
            return Ok(());
        }
        if wake == WakePolicy::Off {
            return Ok(());
        }
        // ADR-101 D2: a board change wakes no one, except an item a
        // person added for this peko, which counts as mentioning it.
        if let ChannelEvent::Posted {
            board: Some(op), ..
        } = &ctx.event
        {
            let asked = Self::human_root_post(&ctx.event).is_some()
                && board_asks(self.inner.port.as_ref(), op, &self.inner.principal);
            if asked {
                let input = self.input(&ctx.event).await;
                spawn_turn(Arc::clone(&self.inner), ctx, input);
            }
            return Ok(());
        }
        match wake {
            WakePolicy::Off => return Ok(()),
            WakePolicy::Human => {
                if Self::human_post(&ctx.event).is_some() {
                    let input = self.input(&ctx.event).await;
                    spawn_turn(Arc::clone(&self.inner), ctx, input);
                }
                return Ok(());
            }
            WakePolicy::Mention => {}
        }
        // ADR-126 D7: a person's reply to this peko's post counts as
        // mentioning it, as Discord's reply pings its author.
        if let ChannelEvent::Posted {
            reply_to: Some(parent),
            ..
        } = &ctx.event
        {
            if Self::human_post(&ctx.event).is_some() && self.asked_by_me(parent).await {
                let input = self.input(&ctx.event).await;
                spawn_turn(Arc::clone(&self.inner), ctx, input);
                return Ok(());
            }
        }
        // ADR-114 D5: an answer to this peko's questions counts as
        // mentioning it.
        let answered = match &ctx.event {
            ChannelEvent::Posted {
                answers: Some(answers),
                ..
            } if Self::human_root_post(&ctx.event).is_some() => self.asked_by_me(&answers.to).await,
            _ => false,
        };
        // ADR-114 D5: so does the addressee's own-words answer to a card
        // that is still open.
        let answered = answered || self.answers_my_open_card(&ctx.event, &ctx.event_id).await;
        if answered {
            if Self::human_root_post(&ctx.event).is_some() {
                let input = self.input(&ctx.event).await;
                spawn_turn(Arc::clone(&self.inner), ctx, input);
            }
            return Ok(());
        }
        let me = self
            .inner
            .port
            .actor_info(&ActorRef::from(&self.inner.principal))
            .await;
        let roster = self.inner.port.participants(&self.inner.channel).await?;
        // Canonical mentions always identify one actor. Short names work only
        // when the complete principal roster is known and the name is unique.
        let short_is_unique =
            peko_channel::short_name_unambiguous(&roster, &self.name, &me.address);
        let input = Self::response_trigger(&me.address, &ctx.event)
            .or_else(|| {
                me.hub_address()
                    .and_then(|h| Self::response_trigger(&h, &ctx.event))
            })
            .or_else(|| {
                short_is_unique
                    .then(|| Self::response_trigger(&self.name, &ctx.event))
                    .flatten()
            });
        // ADR-128 D1: a person's post that addresses no one is still for
        // the only peko in the group.
        if input.is_none()
            && !(Self::human_post(&ctx.event).is_some() && self.only_peko_here().await)
        {
            return Ok(());
        }
        let input = self.input(&ctx.event).await;
        spawn_turn(Arc::clone(&self.inner), ctx, input);
        Ok(())
    }

    /// The turn input for `event` (ADR-126 D6).
    async fn input(&self, event: &ChannelEvent) -> String {
        turn_input(self.inner.port.as_ref(), &self.inner.channel, event).await
    }
}

/// A post as a turn reads it (ADR-126 D5, D6): `{author} (id msg_…): {text}`,
/// or for a reply `{author} (id msg_…; replying to {quoted author}: “{start
/// of the message}”): {text}`, the quote read from this copy's own log. The
/// id is what `ChannelSend` takes as `parent` to react to the post or answer
/// it. A message this copy does not hold reads as "a message that isn't
/// loaded".
pub(crate) async fn turn_input(
    port: &dyn ChannelPort,
    channel: &ChannelId,
    event: &ChannelEvent,
) -> String {
    let ChannelEvent::Posted {
        author,
        text,
        reply_to,
        msg_id,
        ..
    } = event
    else {
        return String::new();
    };
    match crate::principal::peer_dm::post_note(
        port,
        channel,
        msg_id.as_deref(),
        reply_to.as_deref(),
    )
    .await
    {
        Some(note) => format!("{author} ({note}): {text}"),
        None => format!("{author}: {text}"),
    }
}

/// Whether a board change asks `principal` for something: an item added
/// for it (ADR-101 D2).
/// How far back a group's log is read for this principal's open question
/// card (ADR-114 D5). A card older than this no longer turns a plain post
/// into its answer.
const CARD_LOOKBACK: usize = 100;

fn board_asks(
    port: &dyn ChannelPort,
    op: &peko_protocol::channel::BoardOp,
    principal: &PrincipalId,
) -> bool {
    let peko_protocol::channel::BoardOp::Add {
        for_member: Some(member),
        ..
    } = op
    else {
        return false;
    };
    member
        .parse::<ActorRef>()
        .is_ok_and(|m| port.key_of(&m) == port.key_of(&ActorRef::from(principal)))
}

// ---------------------------------------------------------------------------
// GroupChildResolver (ADR-049 Phase 3)
// ---------------------------------------------------------------------------

/// Production [`BindingResolver`] for the group wake: the "binding" is
/// the channel's wire id and resolution find-or-creates the
/// principal's per-(principal, channel) standing child session
/// (`/group-<slug>`) and its binding record via
/// [`crate::principal::peer_children::ensure_group_binding`]. Idempotent
/// per channel — the responder's `OnceCell` caches the first
/// successful resolution, so provisioning happens at most once per
/// (principal, channel) per daemon lifetime.
pub(crate) struct GroupChildResolver {
    port: Arc<dyn ChannelPort>,
    principal: PrincipalId,
    agent_name: String,
    session_manager: Arc<RwLock<SessionManager>>,
}

impl GroupChildResolver {
    pub(crate) fn new(
        port: Arc<dyn ChannelPort>,
        principal: PrincipalId,
        agent_name: String,
        session_manager: Arc<RwLock<SessionManager>>,
    ) -> Self {
        Self {
            port,
            principal,
            agent_name,
            session_manager,
        }
    }
}

#[async_trait]
impl BindingResolver for GroupChildResolver {
    async fn resolve(&self, binding: &str) -> anyhow::Result<String> {
        let channel = ChannelId::parse(binding)
            .ok_or_else(|| anyhow::anyhow!("not a channel id: {binding}"))?;
        crate::principal::peer_children::ensure_group_binding(
            &self.port,
            &self.principal,
            &self.agent_name,
            &channel,
            &self.session_manager,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// PeerTurnDriver (production)
// ---------------------------------------------------------------------------

/// Production [`BoundTurnDriver`]: the same turn driver every ingress
/// path uses ([`PeerChildTurns::drive_turn_streaming`]), so a turn a
/// channel post starts gets exactly what a `peko send` turn gets — no
/// time limit, the session's conversation context (peer + DM channel),
/// and a cancellation token registered in the daemon's streaming-run
/// registry, so `peko stop` stops it like any other turn.
pub(crate) struct PeerTurnDriver {
    turns: Arc<PeerChildTurns>,
    /// `None` outside the daemon (tests): turns run without a stop
    /// handle.
    runs: Option<StreamingRuns>,
    principal_name: String,
}

impl PeerTurnDriver {
    pub(crate) fn new(
        turns: Arc<PeerChildTurns>,
        runs: Option<StreamingRuns>,
        principal_name: String,
    ) -> Self {
        Self {
            turns,
            runs,
            principal_name,
        }
    }
}

/// Removes this turn's entry from the streaming-run registry when the
/// turn ends — only if the entry is still this turn's (identified by
/// its `interrupt_acked` handle), never another run's.
pub(crate) struct RunRegistration {
    runs: StreamingRuns,
    session_id: String,
    marker: Arc<tokio::sync::Notify>,
}

impl Drop for RunRegistration {
    fn drop(&mut self) {
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if runs
            .get(&self.session_id)
            .is_some_and(|h| Arc::ptr_eq(&h.interrupt_acked, &self.marker))
        {
            runs.remove(&self.session_id);
        }
    }
}

/// Make a turn in `session_id` stoppable: `PrincipalStop` cancels the
/// returned token through `runs`. The registration is `None` when
/// another run already holds the session's entry (that run's preflight
/// refusal follows); hold it for as long as the turn runs.
pub(crate) fn register_run(
    runs: &StreamingRuns,
    session_id: &str,
    principal_name: &str,
) -> (tokio_util::sync::CancellationToken, Option<RunRegistration>) {
    let cancel = tokio_util::sync::CancellationToken::new();
    let marker = Arc::new(tokio::sync::Notify::new());
    let mut guard = runs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.contains_key(session_id) {
        return (cancel, None);
    }
    guard.insert(
        session_id.to_string(),
        StreamingRunHandle {
            principal_name: principal_name.to_string(),
            peer: None,
            cancel: cancel.clone(),
            interrupt_acked: Arc::clone(&marker),
        },
    );
    drop(guard);
    let registration = RunRegistration {
        runs: Arc::clone(runs),
        session_id: session_id.to_string(),
        marker,
    };
    (cancel, Some(registration))
}

#[async_trait]
impl BoundTurnDriver for PeerTurnDriver {
    async fn drive_turn(
        &self,
        session_id: &str,
        message: &str,
        author: Option<&ActorRef>,
    ) -> anyhow::Result<String> {
        let (cancel, _registration) = match self.runs.as_ref() {
            Some(runs) => register_run(runs, session_id, &self.principal_name),
            None => (tokio_util::sync::CancellationToken::new(), None),
        };
        // ADR-078: a group member's mention passes membership, not a
        // permission check, so the member is also the authority.
        let driver = match author {
            Some(author) => Some(self.turns.driver(author, author).await),
            None => None,
        };
        let acting_for = self
            .turns
            .root_stamp(
                driver.as_ref(),
                session_id,
                peko_tools_core::Origin::Ingress { channel: None },
            )
            .await;
        let channel = self.turns.session_channel(session_id).await;
        let watched = self.turns.channel_port().zip(channel.as_ref());
        let outcome = self
            .turns
            .drive_conversation_turn(
                watched,
                session_id,
                message,
                Vec::new(),
                Arc::new(|_event| {}),
                Some(cancel.clone()),
                None,
                acting_for,
            )
            .await;
        match outcome {
            Ok(outcome) => Ok(outcome.final_text),
            Err(_) if cancel.is_cancelled() => Err(anyhow::anyhow!("stopped by user")),
            Err(e) => Err(e),
        }
    }

    /// The daemon-shared steering inbox the turns drain, so
    /// collision-queued steering lands in the inbox the running loop
    /// drains.
    fn steering_inbox(&self) -> Option<Arc<peko_session::InboxRegistry>> {
        self.turns.executor().inbox_registry()
    }

    /// The same registry check the turn's preflight refuses on.
    async fn has_active_run(&self, session_id: &str) -> bool {
        self.turns
            .executor()
            .has_active_run_for_child(session_id)
            .await
    }

    /// The stamp the executor registered for the run in flight.
    async fn running_stamp(&self, session_id: &str) -> Option<peko_tools_core::ActingFor> {
        self.turns
            .executor()
            .active_run_stamp_for_child(session_id)
            .await
    }

    /// The stamp [`Self::drive_turn`] gives a turn `author` starts.
    async fn stamp_for(
        &self,
        session_id: &str,
        author: &ActorRef,
    ) -> Option<peko_tools_core::ActingFor> {
        let driver = self.turns.driver(author, author).await;
        Some(
            self.turns
                .root_stamp(
                    Some(&driver),
                    session_id,
                    peko_tools_core::Origin::Ingress { channel: None },
                )
                .await,
        )
    }
}

// ---------------------------------------------------------------------------
// Peer-post nudge (ADR-086)
// ---------------------------------------------------------------------------

/// What a post by another principal pokes (ADR-086): the daemon's cron
/// engine, which brings the recipient's attention keepalive forward. It
/// starts no run itself, so ADR-072 D1 holds.
#[async_trait]
pub(crate) trait PeerPostNudge: Send + Sync {
    async fn peer_posted(&self, principal: &PrincipalId, channel: &ChannelId);
}

/// The nudge target, bound after construction: the supervisor exists
/// before the cron engine does.
pub(crate) type PeerNudgeSlot = Arc<std::sync::OnceLock<Arc<dyn PeerPostNudge>>>;

/// Wraps a channel's responder: a post by another principal nudges the
/// recipient's keepalive, then the inner responder sees the event as
/// before.
struct PeerPostNudger {
    inner: Arc<dyn ChannelResponder>,
    port: Arc<dyn ChannelPort>,
    slot: PeerNudgeSlot,
}

impl PeerPostNudger {
    /// Whether `ctx.event` is a post by a principal other than the one
    /// this subscriber acts for.
    async fn is_peer_post(&self, ctx: &RespondCtx) -> bool {
        // ADR-126 D2: a reaction brings no keepalive forward.
        if !ctx.event.is_message() {
            return false;
        }
        let ChannelEvent::Posted { author, board, .. } = &ctx.event else {
            return false;
        };
        // A board change brings nobody's keepalive forward unless it asks
        // this peko for something (ADR-101 D2).
        if board
            .as_ref()
            .is_some_and(|op| !board_asks(self.port.as_ref(), op, &ctx.principal))
        {
            return false;
        }
        let Ok(author @ ActorRef::Principal(_)) = ActorRef::from_str(author) else {
            return false;
        };
        let me = ActorRef::from(&ctx.principal);
        self.port.actor_info(&author).await.identity_key
            != self.port.actor_info(&me).await.identity_key
    }
}

#[async_trait]
impl ChannelResponder for PeerPostNudger {
    async fn consider_response(&self, ctx: RespondCtx) -> peko_channel::Result<()> {
        if let Some(nudge) = self.slot.get() {
            if self.is_peer_post(&ctx).await {
                nudge.peer_posted(&ctx.principal, &ctx.channel).await;
            }
        }
        self.inner.consider_response(ctx).await
    }
}

/// Wraps a DM's responder: a message from a visitor (anyone but the
/// peko and its owner, ADR-113 D1) is counted in the peko's tally
/// (ADR-130), then the inner responder sees the event as before.
struct VisitorCounter {
    inner: Arc<dyn ChannelResponder>,
    port: Arc<dyn ChannelPort>,
    /// Who the peko's owner is, asked per post (an owner can change).
    trust: Arc<dyn crate::security::stamps::TrustSource>,
    tally: PathBuf,
}

impl VisitorCounter {
    /// When `ctx.event` is a visitor's message, the time it was posted.
    async fn visitor_post(&self, ctx: &RespondCtx) -> Option<chrono::DateTime<chrono::Utc>> {
        use peko_subject::ActorKeys as _;
        // ADR-126 D2: a reaction is not a message.
        if !ctx.event.is_message() {
            return None;
        }
        let ChannelEvent::Posted { author, at, .. } = &ctx.event else {
            return None;
        };
        let author = ActorRef::from_str(author).ok()?;
        let keys = peko_channel::PortKeys(self.port.as_ref());
        if keys.same_actor(&author, &ActorRef::from(&ctx.principal)) {
            return None;
        }
        if self.trust.trust_of(&author, &keys).await == peko_tools_core::Trust::Owner {
            return None;
        }
        chrono::DateTime::parse_from_rfc3339(at)
            .ok()
            .map(|at| at.with_timezone(&chrono::Utc))
    }
}

#[async_trait]
impl ChannelResponder for VisitorCounter {
    async fn consider_response(&self, ctx: RespondCtx) -> peko_channel::Result<()> {
        if let Some(at) = self.visitor_post(&ctx).await {
            let path = self.tally.clone();
            let counted = tokio::task::spawn_blocking(move || {
                crate::principal::visitors::record(&path, at, chrono::Utc::now())
            })
            .await;
            if let Ok(Err(e)) = counted {
                warn!(channel = %ctx.channel, "visitor tally not written: {e}");
            }
        }
        self.inner.consider_response(ctx).await
    }
}

/// Wrap a DM's `inner` so visitors' messages are counted in the tally at
/// `tally` (`principal::visitors::path_for`).
pub(crate) fn with_visitor_count(
    inner: Arc<dyn ChannelResponder>,
    port: Arc<dyn ChannelPort>,
    trust: Arc<dyn crate::security::stamps::TrustSource>,
    tally: PathBuf,
) -> Arc<dyn ChannelResponder> {
    Arc::new(VisitorCounter {
        inner,
        port,
        trust,
        tally,
    })
}

/// Wrap `inner` so peer posts reach the nudge in `slot` once it is set.
pub(crate) fn with_peer_nudge(
    inner: Arc<dyn ChannelResponder>,
    port: Arc<dyn ChannelPort>,
    slot: PeerNudgeSlot,
) -> Arc<dyn ChannelResponder> {
    Arc::new(PeerPostNudger { inner, port, slot })
}

// ---------------------------------------------------------------------------
// select_responder — the per-channel wiring decision
// ---------------------------------------------------------------------------

/// The per-principal driver bundle [`select_responder`] picks from:
/// the shared turn driver plus the resolver that find-or-creates the
/// per-(principal, channel) group child, and the principal's name (what
/// a user mentions to wake it).
pub(crate) struct ResponderDrivers {
    pub(crate) turn: Arc<dyn BoundTurnDriver>,
    pub(crate) group_resolver: Arc<dyn BindingResolver>,
    pub(crate) name: String,
    /// Where the principal's wake books are kept
    /// ([`crate::principal::group_wakes::dir_for`]).
    pub(crate) wakes_dir: PathBuf,
}

/// Pick the responder for one (principal, channel) pair. Pure decision
/// shared by the boot path and the post-boot hooks, and unit-tested
/// here:
///
/// - a DM gets [`NoopChannelResponder`]: a user's DM is
///   driven by the ingress path that posted it, and a post by another
///   principal never starts a run (ADR-072) — the principal reads it
///   with ChannelRead;
/// - a group with a driver bundle yields a [`GroupWakeResponder`]
///   (ADR-049 Phase 3 — bare `chan_*` and `group:<slug>` channels
///   alike), which wakes per the binding's policy (ADR-082 D3);
/// - no driver bundle (the principal has no resolvable model) keeps
///   [`NoopChannelResponder`].
pub(crate) fn select_responder(
    channel: ChannelId,
    principal: PrincipalId,
    port: Arc<dyn ChannelPort>,
    kind: ConversationKind,
    drivers: Option<ResponderDrivers>,
    nudge: PeerNudgeSlot,
) -> Arc<dyn ChannelResponder> {
    match (kind, drivers) {
        (ConversationKind::Group, Some(drivers)) => {
            let responder = GroupWakeResponder::new(
                channel,
                principal,
                drivers.name,
                port,
                drivers.group_resolver,
                drivers.turn,
                &drivers.wakes_dir,
            );
            responder.resume_held();
            Arc::new(responder)
        }
        // A group without a driver wakes nothing, and nudges nothing
        // (ADR-128 D1); only a DM's peer posts nudge the keepalive.
        (ConversationKind::Group, None) => Arc::new(NoopChannelResponder),
        (ConversationKind::Dm, _) => with_peer_nudge(Arc::new(NoopChannelResponder), port, nudge),
    }
}

/// The directory a channel's subscribers keep `cursors.json` in: the
/// channel's own directory under the store's on-disk name (`group:x` →
/// `group.3A.x`), so cursors sit beside the channel's events and the
/// path is legal on Windows.
///
/// Earlier builds kept cursors under the raw channel id
/// (`channels/group:x/`). Such a file is moved into the channel's
/// directory once, so the subscriber resumes where it was instead of
/// replaying the whole history, and the old directory is dropped if
/// nothing else is in it. When the move fails the old directory is
/// used as before, and the failure is logged.
async fn cursor_dir(runtime_dir: &Path, channel: &ChannelId) -> PathBuf {
    let channels = runtime_dir.join("channels");
    let dir = channels.join(peko_channel::fs::channel_dir_name(channel));
    let legacy_dir = channels.join(channel.as_str());
    if legacy_dir == dir {
        return dir;
    }
    let legacy = ChannelCursors::path_in(&legacy_dir);
    let current = ChannelCursors::path_in(&dir);
    if !tokio::fs::try_exists(&legacy).await.unwrap_or(false)
        || tokio::fs::try_exists(&current).await.unwrap_or(false)
    {
        return dir;
    }
    let moved = async {
        tokio::fs::create_dir_all(&dir).await?;
        tokio::fs::rename(&legacy, &current).await
    }
    .await;
    match moved {
        Ok(()) => {
            let _ = tokio::fs::remove_dir(&legacy_dir).await;
            dir
        }
        Err(e) => {
            warn!(
                channel = %channel,
                from = %legacy.display(),
                "channel subscribers: legacy cursor move failed; reading them in place: {e}"
            );
            legacy_dir
        }
    }
}

// ---------------------------------------------------------------------------
// ChannelBindingSupervisor
// ---------------------------------------------------------------------------

/// Owns the per-(principal, channel) `ChannelSubscriber` lifespan:
/// boot-time enumeration ([`Self::spawn_all`]) plus the post-boot
/// hooks ([`Self::ensure_subscriber`], called from `ChannelHost`'s
/// `channel_created` / `ensure_invitee_subscriber` impls on `AppState`).
///
/// Also owns the per-principal [`PeerTurnDriver`] cache — driver
/// construction resolves the principal's provider, so it happens once
/// per principal (first bound channel), not once per message.
///
/// Post-boot approach: hook-driven, not a periodic rescan. The IPC
/// `ChannelCreate` / `ChannelInvite` success arms call `ChannelHost`
/// hooks, and (Phase 12a) the cross-runtime invite bootstrap
/// (`TunnelHost::dm_channel_mirror_bootstrap`) calls
/// [`Self::ensure_subscriber`] right after `join_remote`, so a
/// mirrored channel gets its subscriber immediately instead of at the
/// next boot sweep.
pub(crate) struct ChannelBindingSupervisor {
    port: Arc<dyn ChannelPort>,
    meter: Arc<dyn ChannelMeter>,
    runtime_dir: PathBuf,
    principal_manager: Arc<PrincipalManager>,
    llm_resolver: Arc<peko_providers::LlmResolver>,
    observability: Arc<Observability>,
    /// (principal, channel) pairs that already have a live subscriber.
    /// Short critical sections only — a std Mutex is fine.
    spawned: std::sync::Mutex<HashSet<(PrincipalId, ChannelId)>>,
    /// Per-principal (turn driver, group resolver) bundles.
    drivers:
        tokio::sync::RwLock<HashMap<PrincipalId, (Arc<PeerTurnDriver>, Arc<GroupChildResolver>)>>,
    /// The daemon's streaming-run registry, so responder turns are
    /// stoppable like ingress turns. `None` outside the daemon.
    streaming_runs: Option<StreamingRuns>,
    /// ADR-086: where peer posts nudge the recipient's keepalive. Set by
    /// [`Self::set_peer_nudge`] once the cron engine exists; subscribers
    /// spawned earlier read it at each event.
    peer_nudge: PeerNudgeSlot,
}

impl ChannelBindingSupervisor {
    pub(crate) fn new(
        port: Arc<dyn ChannelPort>,
        meter: Arc<dyn ChannelMeter>,
        runtime_dir: PathBuf,
        principal_manager: Arc<PrincipalManager>,
        llm_resolver: Arc<peko_providers::LlmResolver>,
        observability: Arc<Observability>,
    ) -> Self {
        Self {
            port,
            meter,
            runtime_dir,
            principal_manager,
            llm_resolver,
            observability,
            spawned: std::sync::Mutex::new(HashSet::new()),
            drivers: tokio::sync::RwLock::new(HashMap::new()),
            streaming_runs: None,
            peer_nudge: Arc::default(),
        }
    }

    /// Bind the peer-post nudge (ADR-086). Only the first call binds.
    pub(crate) fn set_peer_nudge(&self, nudge: Arc<dyn PeerPostNudge>) {
        let _ = self.peer_nudge.set(nudge);
    }

    /// Register responder turns in the daemon's streaming-run registry
    /// (`AppState::streaming_runs`) so `peko stop` reaches them.
    #[must_use]
    pub(crate) fn with_streaming_runs(mut self, runs: StreamingRuns) -> Self {
        self.streaming_runs = Some(runs);
        self
    }

    /// Boot path: spawn one subscriber per (loaded principal × channel
    /// the principal is a member of). Replaces the pre-Phase-4
    /// `spawn_channel_subscribers` body; spawn-and-forget so a
    /// subscriber crash doesn't block daemon boot.
    pub(crate) async fn spawn_all(self: &Arc<Self>) -> Vec<tokio::task::JoinHandle<()>> {
        let principals = self.principal_manager.list_all().await;
        let mut handles = Vec::new();
        for principal in principals {
            let channels = match self.port.list_for_principal(&principal.id).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(
                        principal = %principal.id,
                        ?e,
                        "channel subscribers: list_for_principal failed; skipping"
                    );
                    continue;
                }
            };
            for channel in channels {
                if let Some(handle) = self.spawn_one(Arc::clone(&principal), channel).await {
                    handles.push(handle);
                }
            }
        }
        handles
    }

    /// Post-boot hook (create/invite): ensure a subscriber exists for
    /// this pair. Sync wrapper — spawns the async work so the IPC
    /// handler never blocks on it; dedup via `spawned` makes repeat
    /// hooks harmless.
    pub(crate) fn ensure_subscriber(self: &Arc<Self>, principal: PrincipalId, channel: ChannelId) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let Some(principal) = this.principal_manager.get(principal.clone()).await else {
                warn!(
                    principal = %principal,
                    channel = %channel,
                    "channel subscribers: post-boot hook for unknown principal; skipping"
                );
                return;
            };
            let _ = this.spawn_one(principal, channel).await;
        });
    }

    /// Test seam: has a subscriber been spawned for this
    /// (principal, channel) pair? `ensure_subscriber` is
    /// fire-and-forget, so tests poll this to observe the spawn.
    #[cfg(test)]
    pub(crate) fn has_subscriber(&self, principal: &PrincipalId, channel: &ChannelId) -> bool {
        self.spawned
            .lock()
            .expect("spawned mutex poisoned")
            .contains(&(principal.clone(), channel.clone()))
    }

    /// Spawn one subscriber unless the pair already has one. Returns
    /// the `JoinHandle` for a fresh spawn, `None` on dedup hit.
    async fn spawn_one(
        self: &Arc<Self>,
        principal: Arc<Principal>,
        channel: ChannelId,
    ) -> Option<tokio::task::JoinHandle<()>> {
        {
            let mut spawned = self.spawned.lock().expect("spawned mutex poisoned");
            if !spawned.insert((principal.id.clone(), channel.clone())) {
                return None;
            }
        }

        let channel_dir = cursor_dir(&self.runtime_dir, &channel).await;
        // Resume from the persisted per-member cursors so a daemon
        // restart doesn't re-observe (and, for bound channels,
        // re-fire) the channel's entire event history. A missing file
        // loads as an empty map (first-ever boot); a corrupt file
        // falls back to fresh cursors.
        let cursors = match ChannelCursors::load(&channel_dir).await {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    channel = %channel,
                    dir = %channel_dir.display(),
                    ?e,
                    "channel subscribers: cursor load failed; starting from fresh cursors"
                );
                ChannelCursors::new()
            }
        };

        let responder = self.responder_for(&principal, &channel).await;
        let sub = ChannelSubscriber::new(
            channel,
            principal.id.clone(),
            channel_dir,
            Arc::clone(&self.port),
            responder,
            Arc::clone(&self.meter),
            cursors,
            SubscriptionConfig::default(),
        );
        Some(sub.spawn())
    }

    /// Read the channel's kind from `meta.json` and pick the responder.
    /// Unreadable meta degrades to `Noop` with a warning — the
    /// meter-only subscriber still runs.
    async fn responder_for(
        &self,
        principal: &Arc<Principal>,
        channel: &ChannelId,
    ) -> Arc<dyn ChannelResponder> {
        let kind = match self.port.conversation_kind(channel).await {
            Ok(kind) => kind,
            Err(e) => {
                warn!(
                    channel = %channel,
                    "channel subscribers: kind read failed; no wake: {e}"
                );
                return with_peer_nudge(
                    Arc::new(NoopChannelResponder),
                    Arc::clone(&self.port),
                    Arc::clone(&self.peer_nudge),
                );
            }
        };
        // A DM never wakes the principal from here (see
        // `select_responder`), so it needs no driver. Its visitors'
        // messages are counted (ADR-130).
        if kind == ConversationKind::Dm {
            return with_visitor_count(
                with_peer_nudge(
                    Arc::new(NoopChannelResponder),
                    Arc::clone(&self.port),
                    Arc::clone(&self.peer_nudge),
                ),
                Arc::clone(&self.port),
                Arc::new(crate::principal::trust::PrincipalTrust::new(principal)),
                crate::principal::visitors::path_for(principal),
            );
        }
        let name = principal.config.read().await.name.clone();
        let drivers = self
            .driver_for(principal)
            .await
            .map(|(turn, group_resolver)| ResponderDrivers {
                turn: Arc::clone(&turn) as Arc<dyn BoundTurnDriver>,
                group_resolver: Arc::clone(&group_resolver) as Arc<dyn BindingResolver>,
                name,
                wakes_dir: crate::principal::group_wakes::dir_for(principal),
            });
        select_responder(
            channel.clone(),
            principal.id.clone(),
            Arc::clone(&self.port),
            kind,
            drivers,
            Arc::clone(&self.peer_nudge),
        )
    }

    /// Get or build the per-principal driver bundle. `None` (with a
    /// warning) when the principal has no resolvable model — the same
    /// configuration error that breaks `peko send`; bound channels
    /// then stay active-only until the principal config is fixed and
    /// the daemon restarts.
    ///
    /// The fast path is the read lock; the slow path holds the write
    /// lock across construction so two concurrent misses on the same
    /// principal serialize through `insert` and only the first bundle
    /// is cached. The previous `read → construct → write` pattern
    /// could build the bundle twice under concurrent first-contact,
    /// dropping the first executor on the floor mid-construction
    /// (channels then handed events to a different `SubagentExecutor`
    /// than the cached one — silent per-channel cross-talk).
    async fn driver_for(
        &self,
        principal: &Arc<Principal>,
    ) -> Option<(Arc<PeerTurnDriver>, Arc<GroupChildResolver>)> {
        if let Some(bundle) = self.drivers.read().await.get(&principal.id).cloned() {
            return Some(bundle);
        }

        let mut drivers = self.drivers.write().await;
        // Re-check under the write lock: a concurrent slow-path
        // already built and cached the bundle while we waited.
        if let Some(bundle) = drivers.get(&principal.id).cloned() {
            return Some(bundle);
        }

        // Sprint 2 Phase 6: construction is shared with the streaming
        // ingress driver (`principal::child_turns`) — one builder for
        // provider resolution, session-manager wiring, registry key,
        // quota attribution, and persona inheritance (the executor now
        // carries the principal's root agent prompt via
        // `.with_agent_config`, closing the blank-prompt gap).
        let turns = match crate::principal::child_turns::PeerChildTurns::build(
            principal,
            &self.llm_resolver,
            Arc::clone(&self.observability),
            // Channel-driven child turns drain the daemon-shared
            // registry too: a CLI/A2A send steered into the same
            // child mid-run is consumed at the next iteration
            // boundary instead of stalling in the inbox.
            Some(self.principal_manager.shared_inbox_registry()),
            self.principal_manager.tooling(),
        )
        .await
        {
            Ok(turns) => turns,
            Err(e) => {
                warn!(
                    principal = %principal.id,
                    "channel binding: cannot build a turn driver (no resolvable model); \
                     bound channels stay active-only: {e:#}"
                );
                return None;
            }
        };

        let principal_name = principal.config.read().await.name.clone();
        // The port the turns' channel checks read: without it a group
        // turn that already posted in its group with `ChannelSend` posted
        // its final text on top, and one that overlapped another session's
        // posts was never asked again (`drive_conversation_turn`).
        let turns = Arc::new(turns.with_channel_port(Some(Arc::clone(&self.port))));
        let turn_driver = Arc::new(PeerTurnDriver::new(
            Arc::clone(&turns),
            self.streaming_runs.clone(),
            principal_name,
        ));
        // ADR-049 Phase 3: the group-child resolver shares the
        // principal's session manager; provisioning is keyed by the
        // channel at resolve time.
        let group_resolver = Arc::new(GroupChildResolver::new(
            Arc::clone(&self.port),
            principal.id.clone(),
            turns.agent_name().to_string(),
            Arc::clone(turns.session_manager()),
        ));

        let bundle = (turn_driver, group_resolver);
        drivers.insert(principal.id.clone(), bundle.clone());
        Some(bundle)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use peko_channel::{ChannelConfig, ChannelStore, CreateOpts};
    use peko_session::SessionCreateOptions;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    fn pid(s: &str) -> PrincipalId {
        PrincipalId(s.to_string())
    }

    /// Sprint 6: convert a test-fixture literal to the v5-derived UUID
    /// form the runtime stores in `SessionMetadata.session_id`.
    fn sid(literal: &str) -> String {
        peko_session::SessionId::from(literal).to_string()
    }

    fn chan() -> ChannelId {
        ChannelId::generate()
    }

    #[tokio::test]
    async fn cursor_dir_uses_the_channels_on_disk_name() {
        let tmp = tempfile::tempdir().unwrap();
        let channel = ChannelId::parse("group:tinytodo").unwrap();
        assert_eq!(
            cursor_dir(tmp.path(), &channel).await,
            tmp.path().join("channels").join("group.3A.tinytodo")
        );
        // A bare id has no colon: its directory is the id itself.
        let bare = ChannelId::generate();
        assert_eq!(
            cursor_dir(tmp.path(), &bare).await,
            tmp.path().join("channels").join(bare.as_str())
        );
    }

    // The old raw-id directory (`group:tinytodo`) can only exist where
    // the file system allows a `:` in a name.
    #[cfg(unix)]
    #[tokio::test]
    async fn cursor_dir_moves_cursors_from_the_old_raw_id_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let channel = ChannelId::parse("group:tinytodo").unwrap();
        let legacy_dir = tmp.path().join("channels").join("group:tinytodo");
        let mut old = ChannelCursors::new();
        old.set(pid("prin_self"), "7".to_string());
        tokio::fs::create_dir_all(&legacy_dir).await.unwrap();
        old.save(&legacy_dir).await.unwrap();

        let dir = cursor_dir(tmp.path(), &channel).await;

        assert_eq!(dir, tmp.path().join("channels").join("group.3A.tinytodo"));
        let moved = ChannelCursors::load(&dir).await.unwrap();
        assert_eq!(moved.get(&pid("prin_self")).map(String::as_str), Some("7"));
        assert!(!legacy_dir.exists(), "the old raw-id directory is gone");
    }

    fn posted(author: &str, text: &str) -> ChannelEvent {
        ChannelEvent::Posted {
            channel: chan(),
            author: author.to_string(),
            parent: None,
            text: text.to_string(),
            at: "2026-08-15T00:00:00Z".to_string(),
            via: None,
            msg_id: None,
            reply_to: None,
            attachments: Vec::new(),
            options: Vec::new(),
            model: None,
            board: None,
            ask: None,
            answers: None,
            react: None,
        }
    }

    fn posted_reply(author: &str, parent: &str, text: &str) -> ChannelEvent {
        ChannelEvent::Posted {
            channel: chan(),
            author: author.to_string(),
            parent: Some(parent.to_string()),
            text: text.to_string(),
            at: "2026-08-15T00:00:00Z".to_string(),
            via: None,
            msg_id: None,
            reply_to: None,
            attachments: Vec::new(),
            options: Vec::new(),
            model: None,
            board: None,
            ask: None,
            answers: None,
            react: None,
        }
    }

    fn respond_ctx(
        principal: &PrincipalId,
        channel: &ChannelId,
        event_id: &str,
        event: ChannelEvent,
    ) -> RespondCtx {
        RespondCtx {
            channel: channel.clone(),
            principal: principal.clone(),
            event,
            event_id: event_id.to_string(),
            now: std::time::SystemTime::now(),
            more: 0,
        }
    }

    // -- stubs ------------------------------------------------------------

    struct StubDriver {
        calls: Arc<StdMutex<Vec<(String, String)>>>,
        reply: String,
        delay: Duration,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    impl StubDriver {
        fn new(reply: &str) -> (Self, Arc<StdMutex<Vec<(String, String)>>>) {
            let calls = Arc::new(StdMutex::new(Vec::new()));
            (
                Self {
                    calls: Arc::clone(&calls),
                    reply: reply.to_string(),
                    delay: Duration::ZERO,
                    active: Arc::new(AtomicUsize::new(0)),
                    max_active: Arc::new(AtomicUsize::new(0)),
                },
                calls,
            )
        }
    }

    #[async_trait]
    impl BoundTurnDriver for StubDriver {
        async fn drive_turn(
            &self,
            session_id: &str,
            message: &str,
            _author: Option<&ActorRef>,
        ) -> anyhow::Result<String> {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now, Ordering::SeqCst);
            if self.delay > Duration::ZERO {
                tokio::time::sleep(self.delay).await;
            }
            self.calls
                .lock()
                .unwrap()
                .push((session_id.to_string(), message.to_string()));
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(self.reply.clone())
        }
    }

    struct StubResolver {
        id: String,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl BindingResolver for StubResolver {
        async fn resolve(&self, _binding: &str) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.id.clone())
        }
    }

    fn test_store(label: &str) -> (Arc<ChannelStore>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let _ = label;
        let store = ChannelStore::new(ChannelConfig {
            runtime_dir: tmp.path().to_path_buf(),
            shared_dir: None,
        });
        (Arc::new(store), tmp)
    }

    /// Poll `cond` until it holds or the deadline expires.
    async fn eventually<F, Fut>(cond: F) -> bool
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if cond().await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    // -- responder end-to-end (stub driver, real ChannelStore) -------------

    /// A group responder for principal `label_principal`, named `self`
    /// (posts wake it with `@self`), over a fresh store.
    async fn bound_responder(
        label_principal: &str,
        driver: impl BoundTurnDriver,
        resolver: StubResolver,
    ) -> (
        GroupWakeResponder,
        Arc<ChannelStore>,
        ChannelId,
        PrincipalId,
        tempfile::TempDir,
    ) {
        let (store, tmp) = test_store(label_principal);
        let principal = pid(label_principal);
        let channel = store
            .create(&principal, CreateOpts::runtime("group"))
            .await
            .unwrap();
        let responder = GroupWakeResponder::new(
            channel.clone(),
            principal.clone(),
            "self".to_string(),
            Arc::clone(&store) as Arc<dyn ChannelPort>,
            Arc::new(resolver),
            Arc::new(driver),
            &tmp.path().join("wakes"),
        );
        (responder, store, channel, principal, tmp)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn posted_event_drives_turn_and_posts_reply() {
        let (driver, calls) = StubDriver::new("the reply");
        let resolver = StubResolver {
            id: "session-1".into(),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let (responder, store, channel, principal, _tmp) =
            bound_responder("prin_self", driver, resolver).await;

        // Write the inbound post to the store first so the reply's
        // `parent` (the triggering event's line id) validates against
        // the log — in production the subscriber only ever hands the
        // responder line ids that exist.
        let other = ActorRef::User("alice".into());
        store.invite(&channel, &principal, &other).await.unwrap();
        let line = store
            .post(&channel, &other, PostMsg::root("@self hi"))
            .await
            .unwrap();

        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                &line,
                posted("user:alice", "@self hi"),
            ))
            .await
            .unwrap();

        assert!(eventually(|| async { calls.lock().unwrap().len() == 1 }).await);
        assert_eq!(
            calls.lock().unwrap()[0],
            ("session-1".to_string(), "user:alice: @self hi".to_string())
        );

        // The reply lands on the channel as the principal, THREADED
        // onto the triggering event (Phase 12a — the root-post-only
        // trigger rule relies on replies carrying `parent`), and the
        // channel's own event log is the only record (no chat-log
        // projection anywhere on this path).
        let store2 = Arc::clone(&store);
        let channel2 = channel.clone();
        let principal2 = principal.clone();
        let line2 = line.clone();
        assert!(
            eventually(|| async {
                store2
                    .peek(&channel2, &peko_channel::Checkpoint::default())
                    .await
                    .unwrap()
                    .iter()
                    .any(|ev| {
                        matches!(ev, ChannelEvent::Posted { author, text, parent, .. }
                    if *author == format!("principal:local:{}",principal2.0)
                        && text == "the reply"
                        && parent.as_deref() == Some(line2.as_str()))
                    })
            })
            .await,
            "reply must land threaded onto the triggering event"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn self_post_is_suppressed_end_to_end() {
        let (driver, calls) = StubDriver::new("reply");
        let resolver = StubResolver {
            id: "session-1".into(),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let (responder, _store, channel, principal, _tmp) =
            bound_responder("prin_self", driver, resolver).await;

        // A post authored by the bound principal (i.e. our own reply
        // observed on the next poll tick) must not drive a turn.
        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                "1",
                posted("prin_self", "my reply"),
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn binding_resolves_once_and_is_cached() {
        let (driver, calls) = StubDriver::new("r");
        let resolve_calls = Arc::new(AtomicUsize::new(0));
        let resolver = StubResolver {
            id: "session-1".into(),
            calls: Arc::clone(&resolve_calls),
        };
        let (responder, _store, channel, principal, _tmp) =
            bound_responder("prin_self", driver, resolver).await;

        for text in ["one", "two"] {
            responder
                .consider_response(respond_ctx(
                    &principal,
                    &channel,
                    "1",
                    posted("user:alice", &format!("@self {text}")),
                ))
                .await
                .unwrap();
        }
        assert!(eventually(|| async { calls.lock().unwrap().len() == 2 }).await);
        assert_eq!(
            resolve_calls.load(Ordering::SeqCst),
            1,
            "the binding must resolve once per channel, not once per message"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_messages_serialize_turns() {
        let (mut driver, calls) = StubDriver::new("r");
        driver.delay = Duration::from_millis(100);
        let max_active = Arc::clone(&driver.max_active);
        let resolver = StubResolver {
            id: "session-1".into(),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let (responder, _store, channel, principal, _tmp) =
            bound_responder("prin_self", driver, resolver).await;

        // Two messages arriving in the same tick: both get a turn, but
        // never concurrently (the second queues behind the first).
        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                "1",
                posted("user:alice", "@self m1"),
            ))
            .await
            .unwrap();
        // Deterministic FIFO: wait until m1's turn has actually STARTED
        // (its task holds the turn mutex) before issuing m2 — spawn
        // order alone doesn't guarantee lock-acquisition order.
        assert!(eventually(|| async { calls.lock().unwrap().len() == 1 }).await);
        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                "1",
                posted("user:alice", "@self m2"),
            ))
            .await
            .unwrap();
        assert!(eventually(|| async { calls.lock().unwrap().len() == 2 }).await);
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
        // Order preserved: the queue is FIFO.
        assert_eq!(calls.lock().unwrap()[0].1, "user:alice: @self m1");
        assert_eq!(calls.lock().unwrap()[1].1, "user:alice: @self m2");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn empty_reply_posts_nothing() {
        let (driver, _calls) = StubDriver::new("   ");
        let resolver = StubResolver {
            id: "session-1".into(),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let (responder, store, channel, principal, _tmp) =
            bound_responder("prin_self", driver, resolver).await;

        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                "1",
                posted("user:alice", "@self hi"),
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let events = store
            .peek(&channel, &peko_channel::Checkpoint::default())
            .await
            .unwrap();
        assert!(
            !events
                .iter()
                .any(|ev| matches!(ev, ChannelEvent::Posted { .. })),
            "empty replies must not be posted; got {events:?}"
        );
    }

    // -- run-active collision → steering conversion ---------------------

    /// Stub driver for the collision path: the FIRST `drive_turn`
    /// fails with a caller-supplied error (the collision refusal);
    /// later calls succeed with `reply`. `has_active_run` reports
    /// `active_runs > 0` so the test controls when the "colliding
    /// run" ends, and `steering_inbox` hands out a shared registry
    /// the test can observe.
    struct CollisionStubDriver {
        calls: Arc<StdMutex<Vec<(String, String)>>>,
        authors: Arc<StdMutex<Vec<Option<String>>>>,
        reply: String,
        inbox: Arc<peko_session::InboxRegistry>,
        active_runs: Arc<AtomicUsize>,
        first_error: StdMutex<Option<anyhow::Error>>,
        /// Whom the run in flight acts for (`running_stamp`); `None`
        /// leaves its standing unknown.
        running_for: Arc<StdMutex<Option<String>>>,
    }

    impl CollisionStubDriver {
        fn new(reply: &str, first_error: anyhow::Error) -> Self {
            Self {
                first_error: StdMutex::new(Some(first_error)),
                ..Self::steady(reply)
            }
        }

        /// A driver whose turns all succeed.
        fn steady(reply: &str) -> Self {
            Self {
                calls: Arc::new(StdMutex::new(Vec::new())),
                authors: Arc::new(StdMutex::new(Vec::new())),
                reply: reply.to_string(),
                inbox: crate::async_exec::executor::standalone_inbox_registry(),
                active_runs: Arc::new(AtomicUsize::new(0)),
                first_error: StdMutex::new(None),
                running_for: Arc::new(StdMutex::new(None)),
            }
        }
    }

    /// The stamp a stub turn acting for `actor` gets.
    fn stub_stamp(actor: &str) -> peko_tools_core::ActingFor {
        peko_tools_core::ActingFor {
            actor: actor.to_string(),
            name: None,
            trust: peko_tools_core::Trust::Untrusted,
            compartment: peko_tools_core::Compartment::Conversation {
                channel: "group:stub".into(),
            },
            origin: peko_tools_core::Origin::Ingress { channel: None },
            owner_request: false,
        }
    }

    #[async_trait]
    impl BoundTurnDriver for CollisionStubDriver {
        async fn drive_turn(
            &self,
            session_id: &str,
            message: &str,
            author: Option<&ActorRef>,
        ) -> anyhow::Result<String> {
            let first_error = self.first_error.lock().unwrap().take();
            // A steady driver refuses as the executor does while a run
            // is in flight.
            if first_error.is_none() && self.active_runs.load(Ordering::SeqCst) > 0 {
                return Err(crate::session::ownership::err_run_active(session_id));
            }
            self.calls
                .lock()
                .unwrap()
                .push((session_id.to_string(), message.to_string()));
            self.authors
                .lock()
                .unwrap()
                .push(author.map(ToString::to_string));
            if let Some(err) = first_error {
                return Err(err);
            }
            Ok(self.reply.clone())
        }

        fn steering_inbox(&self) -> Option<Arc<peko_session::InboxRegistry>> {
            Some(Arc::clone(&self.inbox))
        }

        async fn has_active_run(&self, _session_id: &str) -> bool {
            self.active_runs.load(Ordering::SeqCst) > 0
        }

        async fn running_stamp(&self, _session_id: &str) -> Option<peko_tools_core::ActingFor> {
            if self.active_runs.load(Ordering::SeqCst) == 0 {
                return None;
            }
            self.running_for.lock().unwrap().as_deref().map(stub_stamp)
        }

        async fn stamp_for(
            &self,
            _session_id: &str,
            author: &ActorRef,
        ) -> Option<peko_tools_core::ActingFor> {
            Some(stub_stamp(&author.to_string()))
        }
    }

    /// A person's post that meets another path's run waits for it to
    /// end, then gets its own turn acting for its author, with the reply
    /// threaded onto the post. It is never steered into the running
    /// turn, which acts for someone else (live, 2026-11-23: the owner's
    /// "please fix it" landed in a completion wake acting for the group,
    /// and its `as_owner` hand-off was refused).
    #[tokio::test(flavor = "multi_thread")]
    async fn run_active_collision_waits_then_runs_the_post_as_its_author() {
        let driver = CollisionStubDriver::new(
            "the reply",
            crate::session::ownership::err_run_active("session-1"),
        );
        let calls = Arc::clone(&driver.calls);
        let authors = Arc::clone(&driver.authors);
        let active_runs = Arc::clone(&driver.active_runs);
        let inbox_registry = Arc::clone(&driver.inbox);
        // The colliding run is in flight when the wake hits.
        active_runs.store(1, Ordering::SeqCst);
        let resolver = StubResolver {
            id: "session-1".into(),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let (responder, store, channel, principal, _tmp) =
            bound_responder("prin_self", driver, resolver).await;

        let other = ActorRef::User("alice".into());
        store.invite(&channel, &principal, &other).await.unwrap();
        let line = store
            .post(&channel, &other, PostMsg::root("@self hi"))
            .await
            .unwrap();
        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                &line,
                posted("user:alice", "@self hi"),
            ))
            .await
            .unwrap();

        // Refused once; nothing reaches the running turn, and no second
        // turn starts while it runs.
        assert!(eventually(|| async { calls.lock().unwrap().len() == 1 }).await);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(calls.lock().unwrap().len(), 1);
        // The running turn only hears of it: a note, not steering it
        // would act on (ADR-120 D2).
        let pending = inbox_registry
            .get_or_create("session-1")
            .await
            .drain_all()
            .await;
        assert_eq!(pending.len(), 1);
        assert!(
            matches!(&pending[0], peko_session::AsyncInboxItem::Steering(m)
                if m.content.contains("do not act on it")),
            "a person's post is not steered into another run: {pending:?}"
        );

        // The run ends: the post gets its own turn, for its author.
        active_runs.store(0, Ordering::SeqCst);
        assert!(eventually(|| async { calls.lock().unwrap().len() == 2 }).await);
        assert_eq!(
            calls.lock().unwrap()[1],
            ("session-1".to_string(), "user:alice: @self hi".to_string())
        );
        assert_eq!(
            authors.lock().unwrap()[1].as_deref(),
            Some(other.to_string().as_str())
        );

        // Its reply is threaded onto the post, like any wake's.
        let store2 = Arc::clone(&store);
        let channel2 = channel.clone();
        let principal2 = principal.clone();
        assert!(
            eventually(|| async {
                store2
                    .peek(&channel2, &peko_channel::Checkpoint::default())
                    .await
                    .unwrap()
                    .iter()
                    .any(|ev| {
                        matches!(ev, ChannelEvent::Posted { author, text, parent, .. }
                    if *author == format!("principal:local:{}",principal2.0)
                        && text == "the reply"
                        && parent.is_some())
                    })
            })
            .await,
            "the reply must be threaded onto the post"
        );
    }

    /// A post with no author (none a channel delivers today, but the
    /// path stays) still converts to steering: the watcher drains the
    /// leftover once the run ends and drives a successor turn, posted
    /// as a ROOT post (the event correlation is lost in the steering).
    #[tokio::test(flavor = "multi_thread")]
    async fn run_active_collision_without_author_converts_to_steering() {
        let driver = CollisionStubDriver::new(
            "the successor reply",
            crate::session::ownership::err_run_active("session-1"),
        );
        let calls = Arc::clone(&driver.calls);
        let authors = Arc::clone(&driver.authors);
        let inbox_registry = Arc::clone(&driver.inbox);
        let resolver = StubResolver {
            id: "session-1".into(),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let (responder, store, channel, principal, _tmp) =
            bound_responder("prin_self", driver, resolver).await;
        let line = store
            .post(&channel, &ActorRef::from(&principal), PostMsg::root("tick"))
            .await
            .unwrap();
        responder
            .inner
            .run_turn(line, "tick".to_string(), None)
            .await;

        assert_eq!(calls.lock().unwrap().len(), 2);
        assert_eq!(*authors.lock().unwrap(), vec![None, None]);
        assert!(
            inbox_registry
                .get_or_create("session-1")
                .await
                .drain_all()
                .await
                .is_empty(),
            "the watcher must have drained the collision steering"
        );
        let events = store
            .peek(&channel, &peko_channel::Checkpoint::default())
            .await
            .unwrap();
        assert!(events.iter().any(|ev| matches!(ev,
            ChannelEvent::Posted { text, parent, .. }
                if text == "the successor reply" && parent.is_none())));
    }

    // -- select_responder ---------------------------------------------------

    /// Build a `ResponderDrivers` bundle from stub parts (the group
    /// resolver is a stub too — provisioning is not what these tests
    /// exercise).
    fn stub_drivers(driver: StubDriver, session_id: &str) -> ResponderDrivers {
        ResponderDrivers {
            turn: Arc::new(driver),
            group_resolver: Arc::new(StubResolver {
                id: session_id.into(),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            name: "self".to_string(),
            wakes_dir: test_wakes_dir(),
        }
    }

    /// ADR-049 Phase 3: an unbound channel with a driver bundle gets
    /// the group wake responder — it fires on `user:*` root posts that
    /// mention the principal, never on principal-authored posts or on
    /// posts that don't mention it.
    #[tokio::test(flavor = "multi_thread")]
    async fn select_responder_group_wake_for_unbound_channel_with_driver() {
        let (store, _tmp) = test_store("select-unbound");
        let principal = pid("prin_self");
        let channel = store
            .create(&principal, CreateOpts::runtime("group"))
            .await
            .unwrap();
        // A second peko, so that a person's post that addresses no one is
        // not taken to be for this one (ADR-128 D1).
        store
            .invite(
                &channel,
                &principal,
                &ActorRef::Principal("prin_peer".into()),
            )
            .await
            .unwrap();

        let (driver, calls) = StubDriver::new("r");
        let responder = select_responder(
            channel.clone(),
            principal.clone(),
            Arc::clone(&store) as Arc<dyn ChannelPort>,
            ConversationKind::Group,
            Some(stub_drivers(driver, "session-1")),
            Arc::default(),
        );
        // A principal-authored root post must NOT fire (D4 loop safety).
        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                "1",
                posted("prin_other", "hi"),
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(calls.lock().unwrap().is_empty());
        // A user post that doesn't mention the principal must NOT fire.
        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                "2",
                posted("user:alice", "hello group"),
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(calls.lock().unwrap().is_empty());
        // A user post that mentions it MUST fire. (By address: a short
        // name counts only when every peko in the roster has a known name.)
        let mention = format!("{} hello group", tag(&store, &principal).await);
        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                "3",
                posted("user:local:alice", &mention),
            ))
            .await
            .unwrap();
        assert!(eventually(|| async { !calls.lock().unwrap().is_empty() }).await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn select_responder_noop_when_driver_unavailable() {
        let (store, _tmp) = test_store("select-nodriver");
        let principal = pid("prin_self");
        let channel = store
            .ensure_dm(&principal, &ActorRef::User("a".into()), "/user-a")
            .await
            .unwrap();

        // Bound but the principal's model didn't resolve: Noop (the
        // channel behaves as active-only rather than failing per event).
        let responder = select_responder(
            channel.clone(),
            principal.clone(),
            Arc::clone(&store) as Arc<dyn ChannelPort>,
            ConversationKind::Dm,
            None,
            Arc::default(),
        );
        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                "1",
                posted("prin_other", "hi"),
            ))
            .await
            .unwrap();
    }

    /// A bound (DM) channel never wakes the principal from the
    /// subscriber, even with a driver: a user's DM is driven by its
    /// ingress path, and another principal's post never starts a run
    /// (ADR-072).
    #[tokio::test(flavor = "multi_thread")]
    async fn select_responder_noop_for_bound_channel_even_with_driver() {
        let (store, _tmp) = test_store("select-bound");
        let principal = pid("prin_self");
        let channel = store
            .ensure_dm(&principal, &ActorRef::User("a".into()), "/user-a")
            .await
            .unwrap();

        let (driver, calls) = StubDriver::new("r");
        let responder = select_responder(
            channel.clone(),
            principal.clone(),
            Arc::clone(&store) as Arc<dyn ChannelPort>,
            ConversationKind::Dm,
            Some(stub_drivers(driver, "session-1")),
            Arc::default(),
        );
        for (line, author) in [("1", "prin_other"), ("2", "user:alice")] {
            responder
                .consider_response(respond_ctx(
                    &principal,
                    &channel,
                    line,
                    posted(author, "@self hi"),
                ))
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(calls.lock().unwrap().is_empty());
    }

    // -- GroupWakeResponder (ADR-049 Phase 3, D4) ----------------------------

    /// The D4 trigger: fires on `user:*` posts, replies too (ADR-126
    /// D7). Principal posts (bare id or `principal:<did>`), `public`,
    /// and non-Posted events never fire.
    #[test]
    fn group_trigger_fires_only_on_user_posts() {
        let principal = "self";

        // Fires: a user root post that mentions the principal (input
        // carries the speaker prefix).
        let ev = posted("user:local:alice", "@self hello group");
        assert_eq!(
            GroupWakeResponder::response_trigger(principal, &ev),
            Some("user:local:alice: @self hello group".to_string())
        );
        // Never fires: a user post that doesn't mention it.
        let ev = posted("user:alice", "hello group");
        assert_eq!(GroupWakeResponder::response_trigger(principal, &ev), None);

        // Never fires: principal-authored root posts — the D4
        // loop-safety rule (the responder's own replies are
        // principal-authored, so self-suppression falls out too).
        for author in [
            "prin_other",
            "prin_self",
            "prin_bob@runtime-B",
            "principal:did:peko:principal:abc123",
            "public",
        ] {
            let ev = posted(author, "@self a principal speaking");
            assert_eq!(
                GroupWakeResponder::response_trigger(principal, &ev),
                None,
                "author {author} must not trigger a group wake"
            );
        }

        // Fires: a user-authored reply that mentions it (ADR-126 D7).
        let ev = posted_reply("user:alice", "3", "@self threaded follow-up");
        assert_eq!(
            GroupWakeResponder::response_trigger(principal, &ev),
            Some("user:alice: @self threaded follow-up".to_string()),
            "a person's reply is weighed like their root post"
        );

        // Never fires: channel bookkeeping events.
        let channel = chan();
        for ev in [
            ChannelEvent::Created {
                channel: channel.clone(),
                creator: "prin_other".into(),
                name: "group".into(),
                at: "2026-08-15T00:00:00Z".into(),
            },
            ChannelEvent::MemberJoined {
                channel: channel.clone(),
                member: "user:alice".into(),
                at: "2026-08-15T00:00:00Z".into(),
            },
            ChannelEvent::MemberLeft {
                channel,
                member: "user:alice".into(),
                at: "2026-08-15T00:00:00Z".into(),
            },
        ] {
            assert_eq!(GroupWakeResponder::response_trigger(principal, &ev), None);
        }
    }

    fn group_responder(
        label_principal: &str,
        driver: StubDriver,
        resolver: StubResolver,
        port: Arc<dyn ChannelPort>,
        channel: ChannelId,
    ) -> GroupWakeResponder {
        GroupWakeResponder::new(
            channel,
            pid(label_principal),
            "self".to_string(),
            port,
            Arc::new(resolver),
            Arc::new(driver),
            &test_wakes_dir(),
        )
    }

    /// A place for a test responder's wake book that no other test uses.
    fn test_wakes_dir() -> PathBuf {
        std::env::temp_dir().join(format!("peko-test-wakes-{}", uuid::Uuid::new_v4()))
    }

    /// ADR-086: the responder wrapper nudges on another principal's
    /// posts only: not on the principal's own, nor on a human's (those
    /// reach it through ingress or the group wake).
    #[tokio::test(flavor = "multi_thread")]
    async fn peer_nudge_fires_only_for_other_principals_posts() {
        struct Recorder(std::sync::Mutex<Vec<(PrincipalId, ChannelId)>>);
        #[async_trait]
        impl PeerPostNudge for Recorder {
            async fn peer_posted(&self, principal: &PrincipalId, channel: &ChannelId) {
                self.0
                    .lock()
                    .unwrap()
                    .push((principal.clone(), channel.clone()));
            }
        }
        let (store, _tmp) = test_store("peer-nudge");
        let port = Arc::clone(&store) as Arc<dyn ChannelPort>;
        let me = pid("prin_self");
        let peer = ActorRef::Principal("prin_peer".into());
        let channel = store
            .create(&me, CreateOpts::runtime("crew"))
            .await
            .unwrap();
        store.invite(&channel, &me, &peer).await.unwrap();
        store
            .invite(&channel, &me, &ActorRef::User("alice".into()))
            .await
            .unwrap();

        let recorder = Arc::new(Recorder(std::sync::Mutex::default()));
        let slot: PeerNudgeSlot = Arc::default();
        let responder = with_peer_nudge(
            Arc::new(NoopChannelResponder),
            Arc::clone(&port),
            Arc::clone(&slot),
        );
        let deliver = |author: ActorRef| {
            let store = Arc::clone(&store);
            let channel = channel.clone();
            let me = me.clone();
            let responder = Arc::clone(&responder);
            async move {
                let line = store
                    .post(&channel, &author, PostMsg::root("news"))
                    .await
                    .unwrap();
                let (_, event) = store
                    .peek_with_ids(&channel, &peko_channel::Checkpoint::default())
                    .await
                    .unwrap()
                    .into_iter()
                    .rev()
                    .find(|(id, _)| *id == line)
                    .unwrap();
                responder
                    .consider_response(respond_ctx(&me, &channel, &line, event))
                    .await
                    .unwrap();
            }
        };

        // Before the cron engine binds, nothing is nudged.
        deliver(peer.clone()).await;
        let _ = slot.set(Arc::clone(&recorder) as Arc<dyn PeerPostNudge>);
        deliver(ActorRef::from(&me)).await;
        deliver(ActorRef::User("alice".into())).await;
        assert!(recorder.0.lock().unwrap().is_empty());
        deliver(peer).await;
        assert_eq!(*recorder.0.lock().unwrap(), vec![(me, channel)]);
    }

    /// ADR-130: a DM's posts by anyone but the peko and its owner are
    /// counted in its visitor tally; reactions are not messages.
    #[tokio::test]
    async fn visitor_count_counts_only_others_messages() {
        struct OwnerIsAlice;
        #[async_trait]
        impl crate::security::stamps::TrustSource for OwnerIsAlice {
            async fn trust_of(
                &self,
                actor: &ActorRef,
                keys: &(dyn peko_subject::ActorKeys + Sync),
            ) -> peko_tools_core::Trust {
                if keys.same_actor(actor, &ActorRef::User("alice".into())) {
                    peko_tools_core::Trust::Owner
                } else {
                    peko_tools_core::Trust::Untrusted
                }
            }
            async fn owner(&self) -> ActorRef {
                ActorRef::User("alice".into())
            }
        }

        let (store, tmp) = test_store("visitor-count");
        let port = Arc::clone(&store) as Arc<dyn ChannelPort>;
        let me = pid("prin_self");
        let visitor = ActorRef::User("bob".into());
        let peer = ActorRef::Principal("prin_peer".into());
        let channel = store
            .create(&me, CreateOpts::runtime("visits"))
            .await
            .unwrap();
        for member in [&visitor, &peer, &ActorRef::User("alice".into())] {
            store.invite(&channel, &me, member).await.unwrap();
        }
        let tally = tmp.path().join("local/visitors.json");
        let responder = with_visitor_count(
            Arc::new(NoopChannelResponder),
            Arc::clone(&port),
            Arc::new(OwnerIsAlice),
            tally.clone(),
        );
        let deliver = |author: ActorRef, msg: PostMsg| {
            let store = Arc::clone(&store);
            let channel = channel.clone();
            let me = me.clone();
            let responder = Arc::clone(&responder);
            async move {
                let line = store.post(&channel, &author, msg).await.unwrap();
                let (_, event) = store
                    .peek_with_ids(&channel, &peko_channel::Checkpoint::default())
                    .await
                    .unwrap()
                    .into_iter()
                    .rev()
                    .find(|(id, _)| *id == line)
                    .unwrap();
                let msg_id = match &event {
                    ChannelEvent::Posted { msg_id, .. } => msg_id.clone(),
                    _ => None,
                };
                responder
                    .consider_response(respond_ctx(&me, &channel, &line, event))
                    .await
                    .unwrap();
                msg_id
            }
        };

        deliver(ActorRef::from(&me), PostMsg::root("hello")).await;
        deliver(
            ActorRef::User("alice".into()),
            PostMsg::root("from the owner"),
        )
        .await;
        let first = deliver(visitor.clone(), PostMsg::root("hi from bob")).await;
        deliver(peer, PostMsg::root("hi from another peko")).await;
        // A visitor's reaction is not a message.
        let mut react = PostMsg::root("");
        react.react = Some(peko_protocol::channel::ReactOp {
            to: first.expect("posts carry a msg_id"),
            emoji: "👍".into(),
            remove: false,
        });
        deliver(visitor, react).await;

        let now = chrono::Utc::now();
        let counted = crate::principal::visitors::read(&tally, now);
        assert_eq!((counted.daily, counted.weekly), (2, 2));
    }

    /// Group wake end-to-end: a user root post on a group channel
    /// drives exactly one turn in the resolved per-(principal,
    /// channel) session, and the reply lands back on the group as the
    /// principal, threaded onto the triggering post.
    #[tokio::test(flavor = "multi_thread")]
    async fn group_wake_user_post_resumes_session_and_posts_reply() {
        let (store, _tmp) = test_store("group-wake");
        let port = Arc::clone(&store) as Arc<dyn ChannelPort>;
        let principal = pid("prin_self");
        let channel = store
            .create(&principal, CreateOpts::runtime("group"))
            .await
            .unwrap();
        store
            .invite(&channel, &principal, &ActorRef::User("alice".into()))
            .await
            .unwrap();

        let (driver, calls) = StubDriver::new("the group reply");
        let responder = group_responder(
            "prin_self",
            driver,
            StubResolver {
                id: "session-1".into(),
                calls: Arc::new(AtomicUsize::new(0)),
            },
            port,
            channel.clone(),
        );

        // The user posts to the group (Phase 2 path: ChannelPost with
        // a `user:<id>` sender — posted by the handler, driven by
        // nobody else, so the subscriber is the ONLY turn driver).
        let line = store
            .post(
                &channel,
                &ActorRef::User("alice".into()),
                PostMsg::root("@self hello group"),
            )
            .await
            .unwrap();
        let events = store
            .peek_with_ids(&channel, &peko_channel::Checkpoint::default())
            .await
            .unwrap();
        let (_, user_event) = events
            .into_iter()
            .find(|(_, ev)| matches!(ev, ChannelEvent::Posted { author, .. } if author == "user:local:alice"))
            .expect("user post is in the log");

        responder
            .consider_response(respond_ctx(&principal, &channel, &line, user_event))
            .await
            .unwrap();

        // One turn, in the resolved session, with the speaker prefix.
        assert!(eventually(|| async { calls.lock().unwrap().len() == 1 }).await);
        let (session, input) = calls.lock().unwrap()[0].clone();
        assert_eq!(session, "session-1");
        assert_eq!(id_free(&input), "user:local:alice: @self hello group");

        // The reply lands on the group as the principal, THREADED
        // onto the triggering user post.
        let store2 = Arc::clone(&store);
        let channel2 = channel.clone();
        let principal2 = principal.clone();
        let line2 = line.clone();
        assert!(
            eventually(|| async {
                store2
                    .peek(&channel2, &peko_channel::Checkpoint::default())
                    .await
                    .unwrap()
                    .iter()
                    .any(|ev| {
                        matches!(ev, ChannelEvent::Posted { author, text, parent, .. }
                    if *author == format!("principal:local:{}",principal2.0)
                        && text == "the group reply"
                        && parent.as_deref() == Some(line2.as_str()))
                    })
            })
            .await,
            "reply must land on the group threaded onto the user post"
        );
    }

    /// A failed group-wake turn stays log-only: a broken wake must not
    /// post errors into a channel many members read.
    #[tokio::test(flavor = "multi_thread")]
    async fn group_wake_failure_posts_nothing() {
        let (store, _tmp) = test_store("group-fail");
        let port = Arc::clone(&store) as Arc<dyn ChannelPort>;
        let principal = pid("prin_self");
        let channel = store
            .create(&principal, CreateOpts::runtime("group"))
            .await
            .unwrap();
        store
            .invite(&channel, &principal, &ActorRef::User("alice".into()))
            .await
            .unwrap();
        let driver = CollisionStubDriver::new("unused", anyhow::anyhow!("provider exploded"));
        let calls = Arc::clone(&driver.calls);
        let responder = GroupWakeResponder::new(
            channel.clone(),
            principal.clone(),
            "self".to_string(),
            port,
            Arc::new(StubResolver {
                id: "session-1".into(),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(driver),
            &test_wakes_dir(),
        );
        let line = store
            .post(
                &channel,
                &ActorRef::User("alice".into()),
                PostMsg::root("@self hello group"),
            )
            .await
            .unwrap();
        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                &line,
                posted("user:local:alice", "@self hello group"),
            ))
            .await
            .unwrap();
        assert!(eventually(|| async { calls.lock().unwrap().len() == 1 }).await);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let events = store
            .peek(&channel, &peko_channel::Checkpoint::default())
            .await
            .unwrap();
        assert!(
            !events
                .iter()
                .any(|ev| matches!(ev, ChannelEvent::Posted { author, .. }
                if *author == format!("principal:local:{}", principal.0))),
            "a group failure posts nothing; got {events:?}"
        );
    }

    /// A principal-authored root post on the group channel drives NO
    /// turn — the D4 loop-safety invariant end-to-end (the group
    /// responder's own reply must not retrigger it either; that case
    /// is the same author shape).
    #[tokio::test(flavor = "multi_thread")]
    async fn group_wake_ignores_principal_posts_end_to_end() {
        let (store, _tmp) = test_store("group-no-wake");
        let port = Arc::clone(&store) as Arc<dyn ChannelPort>;
        let principal = pid("prin_self");
        let other = pid("prin_other");
        let channel = store
            .create(&principal, CreateOpts::runtime("group"))
            .await
            .unwrap();
        store
            .invite(&channel, &principal, &ActorRef::from(&other))
            .await
            .unwrap();

        let (driver, calls) = StubDriver::new("unused");
        let responder = group_responder(
            "prin_self",
            driver,
            StubResolver {
                id: "session-1".into(),
                calls: Arc::new(AtomicUsize::new(0)),
            },
            port,
            channel.clone(),
        );

        let line = store
            .post(
                &channel,
                &ActorRef::from(&other),
                PostMsg::root("agent talking"),
            )
            .await
            .unwrap();
        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                &line,
                posted("prin_other", "agent talking"),
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            calls.lock().unwrap().is_empty(),
            "principal-authored posts must never drive a group wake"
        );
    }

    /// The production [`GroupChildResolver`] find-or-creates one
    /// standing child per channel wire id: idempotent per channel,
    /// distinct across channels (the per-(principal, channel) session
    /// keying, D4).
    #[tokio::test(flavor = "multi_thread")]
    async fn group_child_resolver_keys_sessions_per_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _store_tmp) = test_store("group-resolver");
        let principal = pid("prin_self");
        for id in ["group:eng-standup", "group:ops"] {
            store
                .create(
                    &principal,
                    CreateOpts::runtime(id).with_id(ChannelId::parse(id).unwrap()),
                )
                .await
                .unwrap();
        }
        let manager = SessionManager::new().with_sessions_dir_internal(tmp.path().join("sessions"));
        let resolver = GroupChildResolver::new(
            Arc::clone(&store) as Arc<dyn ChannelPort>,
            principal.clone(),
            "test-agent".to_string(),
            Arc::new(RwLock::new(manager)),
        );

        let eng = resolver.resolve("group:eng-standup").await.unwrap();
        assert_eq!(
            eng,
            resolver.resolve("group:eng-standup").await.unwrap(),
            "same channel must resolve to the same session"
        );
        let ops = resolver.resolve("group:ops").await.unwrap();
        assert_ne!(eng, ops, "distinct channels must get distinct sessions");
        // ADR-082 D2: provisioning records the binding at the default policy.
        let binding = store
            .binding_for(&ChannelId::parse("group:eng-standup").unwrap(), &principal)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(binding.session, eng);
        assert_eq!(binding.wake, WakePolicy::Mention);
    }

    /// ADR-082 D3: the group wake reads the binding's policy per post.
    #[tokio::test(flavor = "multi_thread")]
    async fn group_wake_follows_the_binding_policy() {
        let (store, _tmp) = test_store("wake-policy");
        let principal = pid("prin_self");
        let channel = store
            .create(&principal, CreateOpts::runtime("group"))
            .await
            .unwrap();
        store.bind(&channel, &principal, "session-1").await.unwrap();
        let (driver, calls) = StubDriver::new("r");
        let responder = select_responder(
            channel.clone(),
            principal.clone(),
            Arc::clone(&store) as Arc<dyn ChannelPort>,
            ConversationKind::Group,
            Some(stub_drivers(driver, "session-1")),
            Arc::default(),
        );

        store
            .set_wake(&channel, &principal, WakePolicy::Off)
            .await
            .unwrap();
        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                "1",
                posted("user:local:alice", "@self are you there"),
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(calls.lock().unwrap().is_empty(), "off wakes nothing");

        store
            .set_wake(&channel, &principal, WakePolicy::Human)
            .await
            .unwrap();
        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                "2",
                posted("prin_other", "a principal speaking"),
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            calls.lock().unwrap().is_empty(),
            "principal posts never wake, whatever the policy"
        );
        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                "3",
                posted("user:local:alice", "no mention needed"),
            ))
            .await
            .unwrap();
        assert!(eventually(|| async { !calls.lock().unwrap().is_empty() }).await);
    }

    /// ADR-101 D2: a board change wakes no one, whatever the policy,
    /// except an item a person added for this peko.
    #[tokio::test(flavor = "multi_thread")]
    async fn board_posts_wake_only_the_peko_an_item_is_for() {
        let (store, _tmp) = test_store("board-wake");
        let principal = pid("prin_self");
        let channel = store
            .create(&principal, CreateOpts::runtime("group"))
            .await
            .unwrap();
        store.bind(&channel, &principal, "session-1").await.unwrap();
        let (driver, calls) = StubDriver::new("r");
        let responder = select_responder(
            channel.clone(),
            principal.clone(),
            Arc::clone(&store) as Arc<dyn ChannelPort>,
            ConversationKind::Group,
            Some(stub_drivers(driver, "session-1")),
            Arc::default(),
        );
        store
            .set_wake(&channel, &principal, WakePolicy::Human)
            .await
            .unwrap();
        let board_post = |author: &str, for_member: Option<&str>| {
            let mut ev = posted(author, "board: alice added “run the tests”");
            if let ChannelEvent::Posted { board, .. } = &mut ev {
                *board = Some(peko_protocol::channel::BoardOp::Add {
                    title: "run the tests".into(),
                    for_member: for_member.map(str::to_string),
                });
            }
            ev
        };
        for (line, ev) in [
            ("1", board_post("user:local:alice", None)),
            (
                "2",
                board_post("user:local:alice", Some("principal:local:prin_other")),
            ),
            (
                "3",
                board_post("prin_other", Some("principal:local:prin_self")),
            ),
        ] {
            responder
                .consider_response(respond_ctx(&principal, &channel, line, ev))
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            calls.lock().unwrap().is_empty(),
            "board changes wake no one"
        );

        store
            .set_wake(&channel, &principal, WakePolicy::Mention)
            .await
            .unwrap();
        responder
            .consider_response(respond_ctx(
                &principal,
                &channel,
                "4",
                board_post("user:local:alice", Some("principal:local:prin_self")),
            ))
            .await
            .unwrap();
        assert!(eventually(|| async { !calls.lock().unwrap().is_empty() }).await);
    }

    /// ADR-114 D5: under the default `Mention` policy, a person's answer
    /// to this peko's questions wakes it; answers naming anything else
    /// do not.
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_to_my_questions_wake_me() {
        let (store, _tmp) = test_store("answer-wake");
        let principal = pid("prin_self");
        let channel = store
            .create(&principal, CreateOpts::runtime("group"))
            .await
            .unwrap();
        // A second peko, so that a person's post that addresses no one is
        // not taken to be for this one (ADR-128 D1).
        store
            .invite(
                &channel,
                &principal,
                &ActorRef::Principal("prin_peer".into()),
            )
            .await
            .unwrap();
        store.bind(&channel, &principal, "session-1").await.unwrap();
        let (driver, calls) = StubDriver::new("r");
        let responder = select_responder(
            channel.clone(),
            principal.clone(),
            Arc::clone(&store) as Arc<dyn ChannelPort>,
            ConversationKind::Group,
            Some(stub_drivers(driver, "session-1")),
            Arc::default(),
        );
        // For someone else, so Alice's post counts only by what it names
        // (an own-words answer is the next test's).
        let card = peko_protocol::channel::Ask {
            for_actor: Some("user:local:bob".into()),
            questions: vec![peko_protocol::channel::Question {
                text: "Which day?".into(),
                options: vec!["Tue".into(), "Thu".into()],
                multi: false,
            }],
        };
        let (_, asked) = store
            .post_with_event(
                &channel,
                &ActorRef::from(&principal),
                PostMsg::root("When?").with_ask(Some(card)),
            )
            .await
            .unwrap();
        let ChannelEvent::Posted {
            msg_id: Some(card_id),
            ..
        } = asked
        else {
            panic!("the card has an id");
        };
        let answer = |to: &str| {
            let mut ev = posted("user:local:alice", "Which day? → Tue");
            if let ChannelEvent::Posted { answers, .. } = &mut ev {
                *answers = Some(peko_protocol::channel::Answers {
                    to: to.to_string(),
                    picks: vec![peko_protocol::channel::Pick {
                        chosen: vec!["Tue".into()],
                        other: None,
                    }],
                });
            }
            ev
        };
        responder
            .consider_response(respond_ctx(&principal, &channel, "5", answer("msg_other")))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            calls.lock().unwrap().is_empty(),
            "answers to someone else's post do not wake"
        );
        responder
            .consider_response(respond_ctx(&principal, &channel, "6", answer(&card_id)))
            .await
            .unwrap();
        assert!(eventually(|| async { !calls.lock().unwrap().is_empty() }).await);
    }

    /// ADR-114 D5: a person's plain post answers the peko's open card in
    /// their own words — it wakes the asker without a mention and closes
    /// the card, so their next plain post does not. A card `for` someone
    /// waits for that person.
    #[tokio::test(flavor = "multi_thread")]
    async fn own_words_answer_to_an_open_card_wakes_me_once() {
        let (store, _tmp) = test_store("own-words-wake");
        let principal = pid("prin_self");
        let channel = store
            .create(&principal, CreateOpts::runtime("group"))
            .await
            .unwrap();
        // A second peko, so that a person's post that addresses no one is
        // not taken to be for this one (ADR-128 D1).
        store
            .invite(
                &channel,
                &principal,
                &ActorRef::Principal("prin_peer".into()),
            )
            .await
            .unwrap();
        store.bind(&channel, &principal, "session-1").await.unwrap();
        let (driver, calls) = StubDriver::new("r");
        let responder = select_responder(
            channel.clone(),
            principal.clone(),
            Arc::clone(&store) as Arc<dyn ChannelPort>,
            ConversationKind::Group,
            Some(stub_drivers(driver, "session-1")),
            Arc::default(),
        );
        let me = ActorRef::from(&principal);
        let ask = |for_actor: Option<&str>| peko_protocol::channel::Ask {
            for_actor: for_actor.map(str::to_string),
            questions: vec![peko_protocol::channel::Question {
                text: "Which day?".into(),
                options: vec!["Tue".into(), "Thu".into()],
                multi: false,
            }],
        };
        // A person's plain post, written to the log and handed to the
        // responder as the subscriber would.
        let say = |who: &'static str, text: &'static str| {
            let store = Arc::clone(&store);
            let channel = channel.clone();
            let me = me.clone();
            async move {
                store
                    .post_attributed_with_event(&channel, &me, who, PostMsg::root(text))
                    .await
                    .unwrap()
            }
        };
        let woke = || calls.lock().unwrap().len();

        store
            .post_with_event(
                &channel,
                &me,
                PostMsg::root("When?").with_ask(Some(ask(None))),
            )
            .await
            .unwrap();
        let (line, ev) = say("user:local:alice", "Tuesday works").await;
        responder
            .consider_response(respond_ctx(&principal, &channel, &line, ev))
            .await
            .unwrap();
        assert!(
            eventually(|| async { woke() == 1 }).await,
            "the answer in words wakes"
        );
        let (line, ev) = say("user:local:alice", "also, lunch was great").await;
        responder
            .consider_response(respond_ctx(&principal, &channel, &line, ev))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(woke(), 1, "the card closed with the first answer");

        store
            .post_with_event(
                &channel,
                &me,
                PostMsg::root("Bob?").with_ask(Some(ask(Some("user:local:bob")))),
            )
            .await
            .unwrap();
        let (line, ev) = say("user:local:alice", "I'd say Thursday").await;
        responder
            .consider_response(respond_ctx(&principal, &channel, &line, ev))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(woke(), 1, "a card for Bob waits for Bob");
        let (line, ev) = say("user:local:bob", "Thursday").await;
        responder
            .consider_response(respond_ctx(&principal, &channel, &line, ev))
            .await
            .unwrap();
        assert!(
            eventually(|| async { woke() == 2 }).await,
            "Bob's words wake"
        );
    }

    // -- real session store ---------------------------------------------------

    /// Build a session manager over a tempdir with a root session and a
    /// spawned child carrying slug `user-a` — the standing-child shape
    /// from Phase 2.
    async fn store_with_standing_child() -> (Arc<RwLock<SessionManager>>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let manager = SessionManager::new()
            .with_sessions_dir_internal(tmp.path().join("sessions"))
            .with_agent_name("test-agent")
            .with_user("alice");
        let manager = Arc::new(RwLock::new(manager));
        let peer = peko_auth::ActorRef::User("alice".into());
        let anchor = sid("root:user:alice");
        let child_id_str = sid("child-1");
        {
            let mut mgr = manager.write().await;
            mgr.create_session(
                "test-agent",
                &peer,
                SessionCreateOptions::new()
                    .with_session_id(&anchor)
                    .with_trigger("user"),
            )
            .await
            .unwrap();
            mgr.create_session(
                "test-agent",
                &peer,
                SessionCreateOptions::new()
                    .with_session_id(&child_id_str)
                    .with_parent(&anchor)
                    .with_trigger("spawn"),
            )
            .await
            .unwrap();
        }
        {
            let mgr = manager.read().await;
            mgr.set_session_slug(&child_id_str, Some("user-a".to_string()))
                .await
                .unwrap();
        }
        (manager, tmp)
    }

    // -- PeerTurnDriver (real ingress turn path, no provider) ---------------

    fn peer_turn_driver(
        manager: Arc<RwLock<SessionManager>>,
        runs: Option<StreamingRuns>,
    ) -> PeerTurnDriver {
        // Unique agent name per driver (mirrors the subagent integration
        // tests' counter pattern).
        static CTR: AtomicUsize = AtomicUsize::new(0);
        let agent_name = format!(
            "channel-binding-test-{}",
            CTR.fetch_add(1, Ordering::Relaxed)
        );
        let executor = crate::agents::subagent_executor::SubagentExecutor::new(
            Arc::clone(&manager),
            &agent_name,
            PrincipalId::generate(),
            crate::tools::runtime::ToolingRuntime::standalone(),
        );
        let turns = PeerChildTurns::from_parts(executor, manager, sid("root:user:alice"));
        PeerTurnDriver::new(Arc::new(turns), runs, "self".to_string())
    }

    /// The responder's turn is the ingress turn: it runs in the bound
    /// session with the channel's message, and its stop handle is
    /// registered for the turn and removed after it.
    #[tokio::test(flavor = "multi_thread")]
    async fn driver_runs_a_real_turn_in_the_bound_session() {
        let (manager, _tmp) = store_with_standing_child().await;
        let runs: StreamingRuns = Arc::default();
        let driver = peer_turn_driver(manager, Some(Arc::clone(&runs)));

        // No provider configured: the resume path short-circuits to
        // its stub completion text (which embeds the task message). The
        // assertion pins guard passage + turn execution + output
        // extraction — the message text flowing into the turn.
        let reply = driver
            .drive_turn(&sid("child-1"), "hello from the channel", None)
            .await
            .unwrap();
        assert!(
            reply.contains("hello from the channel"),
            "the channel message must reach the turn body: {reply}"
        );
        assert!(runs.lock().unwrap().is_empty(), "stop handle removed");
    }

    /// A registered turn is cancelled through the registry and leaves it
    /// when its guard drops; a session that already has a run keeps that
    /// run's handle.
    #[test]
    fn register_run_makes_a_turn_stoppable_once() {
        let runs: StreamingRuns = Arc::default();
        let (cancel, registration) = register_run(&runs, "sess-1", "kit");
        assert!(registration.is_some());
        let (other, second) = register_run(&runs, "sess-1", "kit");
        assert!(second.is_none(), "the session already has a run");
        runs.lock().unwrap()["sess-1"].cancel.cancel();
        assert!(cancel.is_cancelled() && !other.is_cancelled());
        drop(second);
        assert!(runs.lock().unwrap().contains_key("sess-1"));
        drop(registration);
        assert!(runs.lock().unwrap().is_empty());
    }

    /// Another run's stop handle is neither replaced nor removed by a
    /// responder turn on the same session.
    #[tokio::test(flavor = "multi_thread")]
    async fn driver_leaves_another_runs_stop_handle_alone() {
        let (manager, _tmp) = store_with_standing_child().await;
        let runs: StreamingRuns = Arc::default();
        let theirs = tokio_util::sync::CancellationToken::new();
        runs.lock().unwrap().insert(
            sid("child-1"),
            StreamingRunHandle {
                principal_name: "self".into(),
                peer: None,
                cancel: theirs.clone(),
                interrupt_acked: Arc::new(tokio::sync::Notify::new()),
            },
        );
        let driver = peer_turn_driver(manager, Some(Arc::clone(&runs)));
        let _ = driver.drive_turn(&sid("child-1"), "hi", None).await;
        let runs = runs.lock().unwrap();
        let kept = runs.get(&sid("child-1")).expect("their entry survives");
        theirs.cancel();
        assert!(kept.cancel.is_cancelled(), "still their token");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn driver_surfaces_guard_refusals_as_errors() {
        let (manager, _tmp) = store_with_standing_child().await;
        let driver = peer_turn_driver(manager, None);

        // The root session itself is not a spawned session — the
        // resume guard refuses it (bound sessions must be spawned).
        let err = driver
            .drive_turn(&sid("root:user:alice"), "hi", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("spawn"), "{err}");
    }

    // -- restart semantics (Phase 0 cursors + anti-loop across ticks) -------

    /// A simulated daemon restart must not re-fire channel history
    /// through the responder: boot 1's subscriber persists its cursor,
    /// boot 2's subscriber loads it and sees only NEW events. The one
    /// new event here is boot 1's own reply post — which the anti-loop
    /// filter suppresses. This is the full PEKO.md "channel/session
    /// separation" loop-closure invariant exercised end to end.
    #[tokio::test(flavor = "multi_thread")]
    async fn restart_does_not_refire_history() {
        let (store, tmp) = test_store("restart");
        let port = Arc::clone(&store) as Arc<dyn ChannelPort>;
        let principal = pid("prin_self");
        let other = ActorRef::User("alice".into());
        let channel = store
            .create(&principal, CreateOpts::runtime("group"))
            .await
            .unwrap();
        store.invite(&channel, &principal, &other).await.unwrap();
        store
            .post(&channel, &other, PostMsg::root("@self before-restart"))
            .await
            .unwrap();

        let channel_dir = tmp.path().join("channels").join(channel.as_str());
        let (driver, calls) = StubDriver::new("the reply");
        let mk_responder = |driver: StubDriver| {
            GroupWakeResponder::new(
                channel.clone(),
                principal.clone(),
                "self".to_string(),
                Arc::clone(&port),
                Arc::new(StubResolver {
                    id: "session-1".into(),
                    calls: Arc::new(AtomicUsize::new(0)),
                }),
                Arc::new(driver),
                &test_wakes_dir(),
            )
        };

        // Boot 1: fresh cursors (first-ever boot starts at offset 0 by
        // design), the pre-restart post drives exactly one turn.
        let mut sub1 = ChannelSubscriber::new(
            channel.clone(),
            principal.clone(),
            channel_dir.clone(),
            Arc::clone(&port),
            Arc::new(mk_responder(driver)),
            peko_channel::cost::noop_meter(),
            ChannelCursors::load(&channel_dir).await.unwrap(),
            SubscriptionConfig::default(),
        );
        sub1.tick_once().await.unwrap();
        assert!(eventually(|| async { calls.lock().unwrap().len() == 1 }).await);
        // Let the detached turn task finish so its reply post is in the
        // log before boot 2 ticks.
        let store2 = Arc::clone(&store);
        let channel2 = channel.clone();
        assert!(
            eventually(|| async {
                store2
                    .peek(&channel2, &peko_channel::Checkpoint::default())
                    .await
                    .unwrap()
                    .iter()
                    .any(
                        |ev| matches!(ev, ChannelEvent::Posted { text, .. } if text == "the reply"),
                    )
            })
            .await
        );

        // Boot 2 ("daemon restart"): a fresh subscriber over LOADED
        // cursors. History is not re-delivered; the only new event is
        // boot 1's self-authored reply, which must not drive a turn.
        let (driver2, _calls2) = StubDriver::new("unused");
        let mut sub2 = ChannelSubscriber::new(
            channel.clone(),
            principal.clone(),
            channel_dir.clone(),
            Arc::clone(&port),
            Arc::new(mk_responder(driver2)),
            peko_channel::cost::noop_meter(),
            ChannelCursors::load(&channel_dir).await.unwrap(),
            SubscriptionConfig::default(),
        );
        let delivered = sub2.tick_once().await.unwrap();
        assert_eq!(
            delivered.len(),
            1,
            "only boot 1's reply post is new after restart; got {delivered:?}"
        );
        assert!(matches!(
            delivered[0],
            ChannelEvent::Posted { ref author, .. } if *author == format!("principal:local:{}", principal.0)
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "self-authored reply observed after restart must not drive a turn"
        );
    }

    // -- ADR-120: a group is a live conversation ---------------------------

    /// Post `msg` as `author` and hand the stored event to `responder`.
    async fn post_and_consider(
        store: &Arc<ChannelStore>,
        responder: &dyn ChannelResponder,
        me: &PrincipalId,
        channel: &ChannelId,
        author: &ActorRef,
        msg: PostMsg,
    ) -> String {
        let line = store.post(channel, author, msg).await.unwrap();
        let (_, event) = store
            .peek_with_ids(channel, &peko_channel::Checkpoint::default())
            .await
            .unwrap()
            .into_iter()
            .rev()
            .find(|(id, _)| *id == line)
            .unwrap();
        responder
            .consider_response(respond_ctx(me, channel, &line, event))
            .await
            .unwrap();
        line
    }

    /// How a post mentions `me` unambiguously: by its address.
    async fn tag(store: &Arc<ChannelStore>, me: &PrincipalId) -> String {
        format!("@{}", store.actor_info(&ActorRef::from(me)).await.address)
    }

    /// A group with `me`, another peko and a person.
    async fn peko_group(
        driver: impl BoundTurnDriver,
    ) -> (
        GroupWakeResponder,
        Arc<ChannelStore>,
        ChannelId,
        PrincipalId,
        ActorRef,
        tempfile::TempDir,
    ) {
        let resolver = StubResolver {
            id: "session-1".into(),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let (responder, store, channel, me, tmp) =
            bound_responder("prin_self", driver, resolver).await;
        let peer = ActorRef::Principal("prin_peer".into());
        store.invite(&channel, &me, &peer).await.unwrap();
        store
            .invite(&channel, &me, &ActorRef::User("alice".into()))
            .await
            .unwrap();
        (responder, store, channel, me, peer, tmp)
    }

    /// The wake book of the responder [`peko_group`] built.
    fn wake_book(tmp: &tempfile::TempDir, channel: &ChannelId) -> WakeBook {
        WakeBook::open(&tmp.path().join("wakes"), channel)
    }

    /// Whether the group's log holds a post with `text`.
    async fn posted_in(store: &Arc<ChannelStore>, channel: &ChannelId, text: &str) -> bool {
        store
            .peek(channel, &peko_channel::Checkpoint::default())
            .await
            .unwrap()
            .iter()
            .any(|ev| matches!(ev, ChannelEvent::Posted { text: t, .. } if t == text))
    }

    /// ADR-120 D1, ADR-128 D4: another peko's mention wakes this one,
    /// acting for that peko. The turn is told nothing is posted unless it
    /// posts, and its final text is not posted.
    #[tokio::test]
    async fn a_pekos_mention_wakes_the_peko_it_names_and_posts_nothing_by_itself() {
        let driver = CollisionStubDriver::steady("on it");
        let calls = Arc::clone(&driver.calls);
        let authors = Arc::clone(&driver.authors);
        let (responder, store, channel, me, peer, tmp) = peko_group(driver).await;

        post_and_consider(
            &store,
            &responder,
            &me,
            &channel,
            &peer,
            PostMsg::root(format!(
                "{} can you review the parser?",
                tag(&store, &me).await
            )),
        )
        .await;

        // The turn, then one reminder: it wrote a reply and posted nothing.
        assert!(eventually(|| async { calls.lock().unwrap().len() == 2 }).await);
        assert_eq!(calls.lock().unwrap()[1].1, UNPOSTED_REMINDER);
        assert_eq!(
            authors.lock().unwrap()[1].as_deref(),
            Some(peer.to_string().as_str()),
            "the reminder keeps the turn's standing"
        );
        let expected = format!(
            "{}: {} can you review the parser?\n{PEKO_STARTED_NOTE}",
            store.actor_info(&peer).await.address,
            tag(&store, &me).await
        );
        assert_eq!(id_free(&calls.lock().unwrap()[0].1), expected);
        assert_eq!(
            authors.lock().unwrap()[0].as_deref(),
            Some(peer.to_string().as_str())
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(calls.lock().unwrap().len(), 2, "it is asked once only");
        assert!(
            !posted_in(&store, &channel, "on it").await,
            "a turn another peko started posts only what the peko posts"
        );
        assert_eq!(
            wake_book(&tmp, &channel)
                .status(chrono::Utc::now())
                .spent_hour,
            1,
            "the wake is counted"
        );
    }

    /// ADR-128 D1: a peko's post that addresses no one starts no turn.
    #[tokio::test]
    async fn a_pekos_unaddressed_post_starts_nothing() {
        let driver = CollisionStubDriver::steady("x");
        let calls = Arc::clone(&driver.calls);
        let (responder, store, channel, me, peer, tmp) = peko_group(driver).await;

        post_and_consider(
            &store,
            &responder,
            &me,
            &channel,
            &peer,
            PostMsg::root("tests pass on my side"),
        )
        .await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(calls.lock().unwrap().is_empty());
        let status = wake_book(&tmp, &channel).status(chrono::Utc::now());
        assert_eq!((status.spent_hour, status.held), (0, 0));
    }

    /// ADR-120 D1: a peko's reply to this peko's post wakes it: the
    /// answer reaches the asker without polling.
    #[tokio::test]
    async fn a_pekos_reply_to_my_post_wakes_me() {
        let driver = CollisionStubDriver::steady("NO_REPLY");
        let calls = Arc::clone(&driver.calls);
        let (responder, store, channel, me, peer, _tmp) = peko_group(driver).await;
        let mine = store
            .post(
                &channel,
                &ActorRef::from(&me),
                PostMsg::root("which file has the bug?"),
            )
            .await
            .unwrap();

        post_and_consider(
            &store,
            &responder,
            &me,
            &channel,
            &peer,
            PostMsg::reply(mine, "src/parse.rs, line 40"),
        )
        .await;

        assert!(eventually(|| async { calls.lock().unwrap().len() == 1 }).await);
    }

    /// ADR-128 D2: past the limit of wakes by other pekos, their posts to
    /// this peko are held; once the owner lets it continue (or the limit
    /// frees up) it is woken once with all of them, acting for the
    /// conversation, and posts nothing by itself.
    #[tokio::test(flavor = "multi_thread")]
    async fn past_the_wake_limit_posts_are_held_then_delivered_in_one_wake() {
        use crate::principal::group_wakes::Limit;
        let driver = CollisionStubDriver::steady("NO_REPLY");
        let calls = Arc::clone(&driver.calls);
        let authors = Arc::clone(&driver.authors);
        let (responder, store, channel, me, peer, tmp) = peko_group(driver).await;
        let book = wake_book(&tmp, &channel);
        book.set_limits(chrono::Utc::now(), Some(Limit::Count(1)), None);
        let mention = tag(&store, &me).await;

        for text in ["first", "second", "third"] {
            post_and_consider(
                &store,
                &responder,
                &me,
                &channel,
                &peer,
                PostMsg::root(format!("{mention} {text}")),
            )
            .await;
        }
        assert!(eventually(|| async { calls.lock().unwrap().len() == 1 }).await);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(calls.lock().unwrap().len(), 1, "only the first one woke it");
        let status = book.status(chrono::Utc::now());
        assert_eq!((status.spent_hour, status.held), (1, 2));
        assert!(status.asks_owner, "the owner is asked");

        book.reset(chrono::Utc::now());
        assert!(eventually(|| async { calls.lock().unwrap().len() == 2 }).await);
        let compiled = calls.lock().unwrap()[1].1.clone();
        assert!(compiled.contains("over your limit of wakes"), "{compiled}");
        let (second, third) = (
            compiled.find(&format!("{mention} second")),
            compiled.find(&format!("{mention} third")),
        );
        assert!(
            second.is_some() && second < third,
            "oldest first: {compiled}"
        );
        assert!(
            !compiled.contains(&format!("{mention} first")),
            "{compiled}"
        );
        assert_eq!(
            authors.lock().unwrap()[1],
            None,
            "it acts for the conversation, not one author"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!posted_in(&store, &channel, "NO_REPLY").await);
        let status = book.status(chrono::Utc::now());
        assert_eq!(
            (status.spent_hour, status.held),
            (1, 0),
            "the reset forgot the first wake; delivering the held posts spent one"
        );
    }

    /// ADR-128 D2: a person's post is never counted and never held.
    #[tokio::test]
    async fn a_person_reaches_a_peko_that_spent_its_wakes() {
        use crate::principal::group_wakes::Limit;
        let driver = CollisionStubDriver::steady("here");
        let calls = Arc::clone(&driver.calls);
        let (responder, store, channel, me, peer, tmp) = peko_group(driver).await;
        let book = wake_book(&tmp, &channel);
        book.set_limits(chrono::Utc::now(), Some(Limit::Count(0)), None);
        let mention = tag(&store, &me).await;

        post_and_consider(
            &store,
            &responder,
            &me,
            &channel,
            &peer,
            PostMsg::root(format!("{mention} ping")),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(calls.lock().unwrap().is_empty(), "the peko's post is held");

        post_and_consider(
            &store,
            &responder,
            &me,
            &channel,
            &ActorRef::User("alice".into()),
            PostMsg::root(format!("{mention} are you there?")),
        )
        .await;
        assert!(eventually(|| async { calls.lock().unwrap().len() == 1 }).await);
        assert!(
            eventually(|| async { posted_in(&store, &channel, "here").await }).await,
            "a person's turn posts its final reply"
        );
        let status = book.status(chrono::Utc::now());
        assert_eq!((status.spent_hour, status.held), (0, 1));
    }

    /// ADR-128 D8: other pekos' posts to this peko found in one read of
    /// the log (this runtime catching up) are delivered as one wake.
    #[tokio::test(flavor = "multi_thread")]
    async fn posts_found_in_one_read_are_delivered_as_one_wake() {
        let driver = CollisionStubDriver::steady("NO_REPLY");
        let calls = Arc::clone(&driver.calls);
        let (responder, store, channel, me, peer, tmp) = peko_group(driver).await;
        let mention = tag(&store, &me).await;
        let mut lines = Vec::new();
        for text in ["one", "two", "three"] {
            lines.push(
                store
                    .post(&channel, &peer, PostMsg::root(format!("{mention} {text}")))
                    .await
                    .unwrap(),
            );
        }
        let events = store
            .peek_with_ids(&channel, &peko_channel::Checkpoint::default())
            .await
            .unwrap();
        let batch: Vec<_> = events
            .into_iter()
            .filter(|(id, _)| lines.contains(id))
            .collect();
        let total = batch.len();
        for (index, (id, event)) in batch.into_iter().enumerate() {
            let mut ctx = respond_ctx(&me, &channel, &id, event);
            ctx.more = total - index - 1;
            responder.consider_response(ctx).await.unwrap();
        }

        assert!(eventually(|| async { calls.lock().unwrap().len() == 1 }).await);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "one turn for the three posts");
        assert!(
            calls[0].1.contains("before you could be woken"),
            "{}",
            calls[0].1
        );
        for text in ["one", "two", "three"] {
            assert!(calls[0].1.contains(&format!("{mention} {text}")));
        }
        assert_eq!(
            wake_book(&tmp, &channel)
                .status(chrono::Utc::now())
                .spent_hour,
            1
        );
    }

    /// ADR-128 D1: a person's post that addresses no one wakes the only
    /// peko in a group, and no peko in a group that has several.
    #[tokio::test]
    async fn an_unaddressed_post_by_a_person_wakes_only_a_lone_peko() {
        let driver = CollisionStubDriver::steady("hello");
        let calls = Arc::clone(&driver.calls);
        let (responder, store, channel, me, _peer, _tmp) = peko_group(driver).await;
        let alice = ActorRef::User("alice".into());
        post_and_consider(
            &store,
            &responder,
            &me,
            &channel,
            &alice,
            PostMsg::root("anyone around?"),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            calls.lock().unwrap().is_empty(),
            "two pekos: it is for neither"
        );

        let lone = CollisionStubDriver::steady("hello");
        let lone_calls = Arc::clone(&lone.calls);
        let resolver = StubResolver {
            id: "session-1".into(),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let (responder, store, channel, me, _tmp) =
            bound_responder("prin_lone", lone, resolver).await;
        store.invite(&channel, &me, &alice).await.unwrap();
        post_and_consider(
            &store,
            &responder,
            &me,
            &channel,
            &alice,
            PostMsg::root("anyone around?"),
        )
        .await;
        assert!(eventually(|| async { lone_calls.lock().unwrap().len() == 1 }).await);
    }

    /// ADR-120 D2: a post with the running turn's standing is steered
    /// into it, as a DM post is. Taken in by the run, it needs nothing
    /// more.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_post_with_the_running_turns_standing_is_steered_into_it() {
        let driver = CollisionStubDriver::steady("done");
        let calls = Arc::clone(&driver.calls);
        let active = Arc::clone(&driver.active_runs);
        let inbox = Arc::clone(&driver.inbox);
        *driver.running_for.lock().unwrap() = Some(ActorRef::User("alice".into()).to_string());
        active.store(1, Ordering::SeqCst);
        let (responder, store, channel, me, _peer, _tmp) = peko_group(driver).await;
        let alice = ActorRef::User("alice".into());

        post_and_consider(
            &store,
            &responder,
            &me,
            &channel,
            &alice,
            PostMsg::root(format!(
                "{} stop, I've got the tests",
                tag(&store, &me).await
            )),
        )
        .await;

        let session_inbox = inbox.get_or_create("session-1").await;
        assert!(eventually(|| async { session_inbox.len().await == 1 }).await);
        // The run takes it in at its next step, then ends.
        let steered = session_inbox.drain_steering().await;
        assert_eq!(steered.len(), 1);
        assert!(steered[0].from_peer);
        assert_eq!(
            id_free(&steered[0].content),
            format!(
                "{}: {} stop, I've got the tests",
                store.actor_info(&alice).await.address,
                tag(&store, &me).await
            )
        );
        active.store(0, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            calls.lock().unwrap().is_empty(),
            "a steered post the run took in starts no turn"
        );
    }

    /// ADR-120 D2: a steered post the run ended without taking in gets
    /// its own turn, acting for its author, threaded onto it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_steered_post_the_run_left_gets_its_own_turn() {
        let driver = CollisionStubDriver::steady("got it");
        let calls = Arc::clone(&driver.calls);
        let authors = Arc::clone(&driver.authors);
        let active = Arc::clone(&driver.active_runs);
        let inbox = Arc::clone(&driver.inbox);
        *driver.running_for.lock().unwrap() = Some(ActorRef::User("alice".into()).to_string());
        active.store(1, Ordering::SeqCst);
        let (responder, store, channel, me, _peer, _tmp) = peko_group(driver).await;
        let alice = ActorRef::User("alice".into());

        post_and_consider(
            &store,
            &responder,
            &me,
            &channel,
            &alice,
            PostMsg::root(format!("{} also update the docs", tag(&store, &me).await)),
        )
        .await;
        let session_inbox = inbox.get_or_create("session-1").await;
        assert!(eventually(|| async { session_inbox.len().await == 1 }).await);

        active.store(0, Ordering::SeqCst);
        assert!(eventually(|| async { calls.lock().unwrap().len() == 1 }).await);
        let expected = format!(
            "{}: {} also update the docs",
            store.actor_info(&alice).await.address,
            tag(&store, &me).await
        );
        assert_eq!(id_free(&calls.lock().unwrap()[0].1), expected);
        assert_eq!(
            authors.lock().unwrap()[0].as_deref(),
            Some(alice.to_string().as_str())
        );
        assert!(session_inbox.is_empty().await);
        let (store2, channel2) = (Arc::clone(&store), channel.clone());
        assert!(
            eventually(|| async {
                store2
                    .peek(&channel2, &peko_channel::Checkpoint::default())
                    .await
                    .unwrap()
                    .iter()
                    .any(|ev| {
                        matches!(ev, ChannelEvent::Posted { text, parent, .. }
                        if text == "got it" && parent.is_some())
                    })
            })
            .await
        );
    }

    /// ADR-120 D2: a post with other standing than the running turn's is
    /// only shown to it, as a note, and gets its own turn once the run
    /// ends; the note is not a turn of its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_post_with_other_standing_is_noted_then_gets_its_own_turn() {
        let driver = CollisionStubDriver::steady("sure");
        let calls = Arc::clone(&driver.calls);
        let authors = Arc::clone(&driver.authors);
        let active = Arc::clone(&driver.active_runs);
        let inbox = Arc::clone(&driver.inbox);
        *driver.running_for.lock().unwrap() = Some(ActorRef::User("bob".into()).to_string());
        active.store(1, Ordering::SeqCst);
        let (responder, store, channel, me, _peer, _tmp) = peko_group(driver).await;
        let alice = ActorRef::User("alice".into());

        post_and_consider(
            &store,
            &responder,
            &me,
            &channel,
            &alice,
            PostMsg::root(format!("{} deploy it", tag(&store, &me).await)),
        )
        .await;
        let session_inbox = inbox.get_or_create("session-1").await;
        assert!(eventually(|| async { session_inbox.len().await == 1 }).await);
        let pending = session_inbox.drain_steering().await;
        assert!(
            pending[0].from_peer && pending[0].content.contains("do not act on it"),
            "a note moves a long call aside but is not acted on: {pending:?}"
        );
        // Not taken in by the run: put it back for the settle below.
        session_inbox
            .push(
                SteeringMessage {
                    id: pending[0].id,
                    content: pending[0].content.clone(),
                    queued_at: pending[0].queued_at,
                    from_peer: true,
                }
                .into(),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(calls.lock().unwrap().is_empty());

        active.store(0, Ordering::SeqCst);
        assert!(eventually(|| async { calls.lock().unwrap().len() == 1 }).await);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(calls.lock().unwrap().len(), 1, "the note gets no turn");
        let expected = format!(
            "{}: {} deploy it",
            store.actor_info(&alice).await.address,
            tag(&store, &me).await
        );
        assert_eq!(id_free(&calls.lock().unwrap()[0].1), expected);
        assert_eq!(
            authors.lock().unwrap()[0].as_deref(),
            Some(alice.to_string().as_str())
        );
    }

    /// The `msg_id` of the post on `line`.
    async fn msg_id_at(store: &Arc<ChannelStore>, channel: &ChannelId, line: &str) -> String {
        store
            .peek_with_ids(channel, &peko_channel::Checkpoint::default())
            .await
            .unwrap()
            .into_iter()
            .find(|(id, _)| id == line)
            .and_then(|(_, ev)| match ev {
                ChannelEvent::Posted { msg_id, .. } => msg_id,
                _ => None,
            })
            .unwrap()
    }

    /// ADR-126 D2: a reaction wakes no one, whatever the policy, and
    /// spends no wake.
    #[tokio::test]
    async fn reactions_wake_no_one_and_spend_nothing() {
        let driver = CollisionStubDriver::steady("NO_REPLY");
        let calls = Arc::clone(&driver.calls);
        let (responder, store, channel, me, peer, tmp) = peko_group(driver).await;
        let alice = ActorRef::User("alice".into());
        store.bind(&channel, &me, "session-1").await.unwrap();
        store
            .set_wake(&channel, &me, WakePolicy::Human)
            .await
            .unwrap();
        let mine = store
            .post(
                &channel,
                &ActorRef::from(&me),
                PostMsg::root("the tests pass now"),
            )
            .await
            .unwrap();
        let mine = msg_id_at(&store, &channel, &mine).await;
        for (by, emoji) in [(&alice, "👍"), (&peer, "🎉")] {
            post_and_consider(
                &store,
                &responder,
                &me,
                &channel,
                by,
                PostMsg::reaction(mine.clone(), emoji, false),
            )
            .await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            calls.lock().unwrap().is_empty(),
            "a reaction starts no turn"
        );
        assert_eq!(
            wake_book(&tmp, &channel)
                .status(chrono::Utc::now())
                .spent_hour,
            0,
            "a reaction spends no wake"
        );

        // Many peko reactions later, another peko's mention still wakes:
        // the reactions count for nothing.
        store
            .set_wake(&channel, &me, WakePolicy::Mention)
            .await
            .unwrap();
        let root = store
            .post(&channel, &alice, PostMsg::root("ok team"))
            .await
            .unwrap();
        let root = msg_id_at(&store, &channel, &root).await;
        for i in 0..30 {
            let emoji = char::from_u32(0x1F600 + (i as u32 % 20))
                .unwrap()
                .to_string();
            let remove = i >= 20;
            store
                .post(
                    &channel,
                    &peer,
                    PostMsg::reaction(root.clone(), emoji, remove),
                )
                .await
                .unwrap();
        }
        post_and_consider(
            &store,
            &responder,
            &me,
            &channel,
            &peer,
            PostMsg::root(format!("{} over to you", tag(&store, &me).await)),
        )
        .await;
        assert!(eventually(|| async { calls.lock().unwrap().len() == 1 }).await);
    }

    /// ADR-126 D7: a person's reply to this peko's post wakes it under
    /// `mention`, quoting the post (D6); under `off`, nothing wakes.
    #[tokio::test]
    async fn a_persons_reply_to_my_post_wakes_me_under_mention_not_off() {
        let driver = CollisionStubDriver::steady("on it");
        let calls = Arc::clone(&driver.calls);
        let (responder, store, channel, me, _peer, _tmp) = peko_group(driver).await;
        let alice = ActorRef::User("alice".into());
        let mine = store
            .post(
                &channel,
                &ActorRef::from(&me),
                PostMsg::root("which branch should I use?"),
            )
            .await
            .unwrap();
        post_and_consider(
            &store,
            &responder,
            &me,
            &channel,
            &alice,
            PostMsg::reply(mine.clone(), "main is fine"),
        )
        .await;
        assert!(eventually(|| async { calls.lock().unwrap().len() == 1 }).await);
        let input = calls.lock().unwrap()[0].1.clone();
        assert!(
            input.contains("replying to") && input.contains("“which branch should I use?”"),
            "{input}"
        );
        assert!(input.ends_with(": main is fine"), "{input}");

        store.bind(&channel, &me, "session-1").await.unwrap();
        store
            .set_wake(&channel, &me, WakePolicy::Off)
            .await
            .unwrap();
        post_and_consider(
            &store,
            &responder,
            &me,
            &channel,
            &alice,
            PostMsg::reply(mine, "or dev"),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(calls.lock().unwrap().len(), 1, "off wakes nothing");
    }

    /// ADR-126 D6: a reply's turn input quotes the message from this
    /// copy's log; one this copy does not hold reads as not loaded.
    #[tokio::test]
    async fn a_reply_quotes_its_message_or_says_it_is_not_loaded() {
        let (store, _tmp) = test_store("reply-quote");
        let me = pid("prin_self");
        let channel = store.create(&me, CreateOpts::runtime("g")).await.unwrap();
        let alice = ActorRef::User("alice".into());
        store.invite(&channel, &me, &alice).await.unwrap();
        let line = store
            .post(
                &channel,
                &alice,
                PostMsg::root("please check the median for even lengths"),
            )
            .await
            .unwrap();
        let id = msg_id_at(&store, &channel, &line).await;
        let mut reply = posted("user:local:alice", "ping");
        if let ChannelEvent::Posted { reply_to, .. } = &mut reply {
            *reply_to = Some(id);
        }
        let port: &dyn ChannelPort = store.as_ref();
        let input = turn_input(port, &channel, &reply).await;
        assert!(
            input.starts_with("user:local:alice (replying to ")
                && input.contains("“please check the median for even lengths”"),
            "{input}"
        );
        if let ChannelEvent::Posted { reply_to, .. } = &mut reply {
            *reply_to = Some("msg_not_here".into());
        }
        let input = turn_input(port, &channel, &reply).await;
        assert_eq!(
            input,
            "user:local:alice (replying to a message that isn't loaded): ping"
        );
        assert_eq!(
            turn_input(port, &channel, &posted("user:local:alice", "hi")).await,
            "user:local:alice: hi"
        );
    }

    /// A turn's input without the `(id msg_…)` note on its post, which
    /// it must have (ADR-126 D5).
    fn id_free(input: &str) -> String {
        let start = input
            .find(" (id msg_")
            .unwrap_or_else(|| panic!("no post id in {input:?}"));
        let end = start + input[start..].find(')').expect("closed note") + 1;
        format!("{}{}", &input[..start], &input[end..])
    }

    /// ADR-126 D5: a turn names the post that started it by its id, so the
    /// peko can react to that post or answer it, also when it replies.
    #[tokio::test]
    async fn a_turn_names_its_post_by_id() {
        let (store, _tmp) = test_store("turn-id");
        let me = pid("prin_self");
        let channel = store.create(&me, CreateOpts::runtime("g")).await.unwrap();
        let alice = ActorRef::User("alice".into());
        store.invite(&channel, &me, &alice).await.unwrap();
        let line = store
            .post(&channel, &alice, PostMsg::root("which branch?"))
            .await
            .unwrap();
        let quoted = msg_id_at(&store, &channel, &line).await;
        let mut post = posted("user:local:alice", "hi");
        if let ChannelEvent::Posted { msg_id, .. } = &mut post {
            *msg_id = Some("msg_abc".into());
        }
        let port: &dyn ChannelPort = store.as_ref();
        assert_eq!(
            turn_input(port, &channel, &post).await,
            "user:local:alice (id msg_abc): hi"
        );
        if let ChannelEvent::Posted { reply_to, .. } = &mut post {
            *reply_to = Some(quoted);
        }
        let input = turn_input(port, &channel, &post).await;
        assert!(
            input.starts_with("user:local:alice (id msg_abc; replying to ")
                && input.contains("“which branch?”")
                && input.ends_with("): hi"),
            "{input}"
        );
    }
}
