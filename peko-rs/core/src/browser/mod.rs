//! A peko's browser, and its owner's hand on it (ADR-119).
//!
//! Each peko has one headless Chromium browser with a profile of its
//! own, started on first use and stopped when idle. The peko drives it
//! with the `Browser` tool. Its owner can watch a tab and take control
//! of it from any of their devices, to sign in or pass a check; while
//! they hold control the peko's calls change nothing and nothing from
//! that time reaches the model or the ledger.

pub mod cdp;
pub mod engine;
pub mod install;
pub mod verbs;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::principal::manager::PrincipalManager;
use crate::principal::Principal;
pub use engine::{Browser, Frame, Snapshot, TabInfo, Transport};

/// The tool's wire name.
pub const BROWSER_TOOL_NAME: &str = "Browser";

/// How a peko's request for its owner's hand starts, in the owner's
/// conversation with it.
pub const BROWSER_NOTICE_PREFIX: &str = "🌐 Browser needs you";

/// A browser nobody has used or watched for this long is stopped.
pub const IDLE_STOP: Duration = Duration::from_mins(15);

/// A viewer the runtime has not heard from for this long is gone
/// (ADR-108 D4's rule for a phone's watch).
pub const VIEWER_GONE: Duration = Duration::from_secs(90);

/// How long a request for the owner's hand waits.
pub const HANDOFF_TTL_HOURS: i64 = 24;

/// Longest reason a hand-off may give.
pub const MAX_WHY_CHARS: usize = 300;

/// A peko's request that its owner step in.
#[derive(Debug, Clone)]
pub struct Handoff {
    pub why: String,
    pub tab: String,
    pub url: String,
    /// The session that asked, woken when the owner is done.
    pub session_id: Option<String>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct Controller {
    viewer: String,
    since: DateTime<Utc>,
    last_seen: Instant,
}

#[derive(Default)]
struct State {
    controller: Option<Controller>,
    handoff: Option<Handoff>,
    /// Secrets the peko typed, taken out of what it reads back.
    typed: Vec<(String, String)>,
    last_used: Option<Instant>,
}

/// One peko's browser and who has a hand on it.
pub struct PekoBrowser {
    principal_id: String,
    profile: PathBuf,
    browser: tokio::sync::Mutex<Option<Arc<Browser>>>,
    state: Mutex<State>,
}

impl PekoBrowser {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn touch(&self) {
        self.state().last_used = Some(Instant::now());
    }

    /// The running browser, if any.
    pub async fn running(&self) -> Option<Arc<Browser>> {
        self.browser.lock().await.clone()
    }

    /// Refuse the peko's own call while its owner has control (D4).
    pub fn peko_may_act(&self) -> Result<()> {
        if let Some(controller) = self.state().controller.as_ref() {
            anyhow::bail!(
                "your owner is using the browser (since {}); nothing was done. End your turn: \
                 you are told when they are finished.",
                controller.since.format("%H:%M UTC")
            );
        }
        Ok(())
    }

    /// Remember a secret the peko typed, so it is taken out of what the
    /// page shows back.
    pub fn remember_typed(&self, name: &str, value: &str) {
        if value.len() < crate::principal::secret_requests::MIN_REDACTED_LEN {
            return;
        }
        let mut state = self.state();
        if !state.typed.iter().any(|(n, v)| n == name && v == value) {
            state.typed.push((name.to_string(), value.to_string()));
        }
    }

    /// `text` with every secret the peko typed replaced by its name.
    #[must_use]
    pub fn scrub(&self, text: &str) -> String {
        let state = self.state();
        let mut scrubbed = text.to_string();
        for (name, value) in &state.typed {
            if scrubbed.contains(value.as_str()) {
                scrubbed = scrubbed.replace(value.as_str(), &format!("[secret:{name}]"));
            }
        }
        scrubbed
    }

    /// The hand-off waiting for the owner, if it has not lapsed.
    #[must_use]
    pub fn handoff(&self) -> Option<Handoff> {
        let mut state = self.state();
        let lapsed = state
            .handoff
            .as_ref()
            .is_some_and(|h| h.at + chrono::Duration::hours(HANDOFF_TTL_HOURS) < Utc::now());
        if lapsed {
            state.handoff = None;
        }
        state.handoff.clone()
    }

    /// Who holds control, and since when.
    #[must_use]
    pub fn controller(&self) -> Option<(String, DateTime<Utc>)> {
        self.state()
            .controller
            .as_ref()
            .map(|c| (c.viewer.clone(), c.since))
    }
}

/// What the daemon lends the browsers: where they live, and how a
/// waiting session is woken.
pub struct BrowserDesk {
    browser_dir: PathBuf,
    inbox: Arc<peko_session::InboxRegistry>,
    manager: Weak<PrincipalManager>,
    pekos: Mutex<HashMap<String, Arc<PekoBrowser>>>,
    installing: Mutex<InstallState>,
    audit: Option<Arc<peko_observability::Observability>>,
}

/// Where a download of the runtime's own browser stands.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct InstallState {
    pub running: bool,
    pub done_bytes: u64,
    pub total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

static DESK: OnceLock<RwLock<Option<Arc<BrowserDesk>>>> = OnceLock::new();

fn desk_slot() -> &'static RwLock<Option<Arc<BrowserDesk>>> {
    DESK.get_or_init(|| RwLock::new(None))
}

/// Install the desk; the daemon calls this once its inboxes and
/// principals exist, and it starts the idle watch.
pub fn install_browser_desk(
    runtime_dir: &std::path::Path,
    inbox: Arc<peko_session::InboxRegistry>,
    manager: Weak<PrincipalManager>,
    audit: Arc<peko_observability::Observability>,
) -> Arc<BrowserDesk> {
    let mut desk = BrowserDesk::new(runtime_dir, inbox, manager);
    desk.audit = Some(audit);
    let desk = Arc::new(desk);
    *desk_slot().write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&desk));
    let weak = Arc::downgrade(&desk);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(20)).await;
            let Some(desk) = weak.upgrade() else { break };
            desk.tidy().await;
        }
    });
    desk
}

/// Write a viewer's picture (base64 JPEG) where the app on this computer
/// reads it: one file per peko, replaced whole each time.
pub fn write_frame_file(
    browser_dir: &std::path::Path,
    principal_id: &str,
    base64_jpeg: &str,
) -> Result<PathBuf> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_jpeg)
        .map_err(|e| anyhow!("the picture is not base64: {e}"))?;
    let safe: String = principal_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    anyhow::ensure!(!safe.is_empty(), "no peko to name the picture after");
    let dir = browser_dir.join("view");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{safe}.jpg"));
    let tmp = dir.join(format!("{safe}.jpg.tmp"));
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Install a desk with nothing behind it but a folder (tests of the
/// paths that reach it).
#[cfg(test)]
pub(crate) fn install_test_desk(runtime_dir: &std::path::Path) -> Arc<BrowserDesk> {
    let desk = Arc::new(BrowserDesk::new(
        runtime_dir,
        Arc::new(peko_session::InboxRegistry::new(
            crate::async_exec::executor::executor::default_inbox_factory(),
        )),
        Weak::new(),
    ));
    *desk_slot().write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&desk));
    desk
}

/// The installed desk, if any (none in CLI one-shots and most tests).
#[must_use]
pub fn browser_desk() -> Option<Arc<BrowserDesk>> {
    desk_slot()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// What `control` changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlChange {
    Took,
    Released,
    /// Nothing changed (already held by this viewer, or not held).
    Unchanged,
}

impl BrowserDesk {
    #[must_use]
    pub fn new(
        runtime_dir: &std::path::Path,
        inbox: Arc<peko_session::InboxRegistry>,
        manager: Weak<PrincipalManager>,
    ) -> Self {
        Self {
            browser_dir: install::browser_dir(runtime_dir),
            inbox,
            manager,
            pekos: Mutex::default(),
            installing: Mutex::default(),
            audit: None,
        }
    }

    /// A loaded principal by id.
    pub async fn principal(&self, principal_id: &str) -> Option<Arc<Principal>> {
        let manager = self.manager.upgrade()?;
        crate::daemon::cron_engine::resolve_principal(
            &manager,
            &peko_subject::PrincipalId(principal_id.to_string()),
        )
        .await
    }

    /// The browser to start, if this computer has one.
    #[must_use]
    pub fn found(&self) -> Option<install::Found> {
        install::find(&self.browser_dir)
    }

    /// Where the runtime keeps its own browser and the pictures it
    /// hands a viewer on this computer.
    #[must_use]
    pub fn browser_dir(&self) -> &std::path::Path {
        &self.browser_dir
    }

    /// Where a download stands.
    #[must_use]
    pub fn install_state(&self) -> InstallState {
        self.installing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Start downloading Chrome for Testing into the runtime's folder,
    /// in the background (the owner's own action: `peko browser
    /// install`). Returns `false` when one is already running.
    pub fn start_install(self: &Arc<Self>) -> bool {
        {
            let mut state = self.installing.lock().unwrap_or_else(|e| e.into_inner());
            if state.running {
                return false;
            }
            *state = InstallState {
                running: true,
                ..InstallState::default()
            };
        }
        let desk = Arc::clone(self);
        tokio::spawn(async move {
            let progress = Arc::clone(&desk);
            let result = install::download(&desk.browser_dir, move |done, total| {
                let mut state = progress
                    .installing
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                state.done_bytes = done;
                state.total_bytes = total;
            })
            .await;
            let mut state = desk.installing.lock().unwrap_or_else(|e| e.into_inner());
            state.running = false;
            state.error = result.err().map(|e| format!("{e:#}"));
        });
        true
    }

    /// Record who took or released control of a peko's browser.
    pub async fn audit_control(&self, peko: &PekoBrowser, viewer: &str, change: &str) {
        let Some(audit) = self.audit.as_ref() else {
            return;
        };
        let _ = audit
            .audit_security(
                "browser.owner_control",
                Some(&peko.principal_id),
                json!({ "viewer": viewer, "change": change }),
            )
            .await;
    }

    /// The peko's browser record (not started by this).
    pub fn peko(&self, principal: &Principal) -> Arc<PekoBrowser> {
        let mut pekos = self.pekos.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(pekos.entry(principal.id.0.clone()).or_insert_with(|| {
            // `<local>/sessions` is the sessions dir; the profile sits
            // beside it and is in no snapshot.
            let sessions = principal.memory.sessions_dir();
            let local = sessions.parent().map_or(sessions.clone(), PathBuf::from);
            Arc::new(PekoBrowser {
                principal_id: principal.id.0.clone(),
                profile: local.join("browser").join("profile"),
                browser: tokio::sync::Mutex::new(None),
                state: Mutex::default(),
            })
        }))
    }

    /// The peko's browser, started if it is not running.
    pub async fn start(&self, peko: &PekoBrowser) -> Result<Arc<Browser>> {
        let mut slot = peko.browser.lock().await;
        if let Some(browser) = slot.as_ref() {
            if browser.alive().await {
                peko.touch();
                return Ok(Arc::clone(browser));
            }
            *slot = None;
        }
        let found = self.found().ok_or_else(|| {
            anyhow!(
                "this computer has no Chrome, Edge, Brave or Chromium to browse with. Ask your \
                 owner to install one, or to run `peko browser install` so Peko downloads its \
                 own (about 190 MB)."
            )
        })?;
        let browser = Browser::launch(
            &found.executable,
            &peko.profile,
            Transport::for_this_platform(),
        )
        .await?;
        *slot = Some(Arc::clone(&browser));
        peko.touch();
        Ok(browser)
    }

    /// Stop the peko's browser. Returns whether one was running.
    pub async fn stop(&self, peko: &PekoBrowser) -> bool {
        let stopped = peko.browser.lock().await.take().is_some();
        let released = peko.state().controller.take().is_some();
        if released {
            self.owner_done(peko, "The browser was stopped.").await;
        }
        stopped
    }

    // ─── The peko asks for its owner's hand (D3) ───────────────────

    /// File a hand-off and tell the owner. Returns `false` when one was
    /// already waiting (it is replaced).
    pub async fn ask_owner(
        &self,
        principal: &Principal,
        peko: &PekoBrowser,
        handoff: Handoff,
    ) -> bool {
        let site = url::Url::parse(&handoff.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| handoff.url.clone());
        let why = handoff.why.clone();
        let fresh = peko.state().handoff.replace(handoff).is_none();
        let Some(messenger) = crate::principal::messenger::global_messenger() else {
            return fresh;
        };
        let name = principal.name().await;
        let note = format!(
            "{BROWSER_NOTICE_PREFIX}: {name} is on {site} and needs your hand. Why: {why}\n\n\
             Open {name}'s browser in the Peko app, take control, and release it when you are \
             done."
        );
        let owner = principal.config.read().await.owner.clone();
        let delivered = messenger
            .deliver_note(
                &principal.id.0,
                &owner,
                &note,
                None,
                peko_session::events::MessageSource::Agent,
                Some("browser"),
            )
            .await;
        if !matches!(delivered, Ok(true)) {
            tracing::warn!(peko = %name, "browser hand-off stored; no owner conversation to announce it in");
        }
        fresh
    }

    /// The owner let go: wake the session that asked, if one did.
    async fn owner_done(&self, peko: &PekoBrowser, note: &str) {
        let Some(handoff) = peko.state().handoff.take() else {
            return;
        };
        let Some(session) = handoff.session_id else {
            return;
        };
        let task_id = format!("browser:{}", uuid::Uuid::new_v4().simple());
        let result = json!({
            "status": "owner_done",
            "tab": handoff.tab,
            "note": format!("{note} Read the page again before you go on."),
        });
        let event = peko_session::CompletionEvent {
            task_id: task_id.clone(),
            tool_name: BROWSER_TOOL_NAME.to_string(),
            result: result.clone(),
            status: peko_tools_core::AsyncTaskStatus::Completed {
                result: peko_tools_core::ToolResult::success(result),
            },
            completed_at: Utc::now(),
            parent_session_key: session.clone(),
        };
        let inbox = self
            .inbox
            .get_or_create(&peko_session::inbox_key(&peko.principal_id, &session))
            .await;
        inbox
            .push(peko_session::InboxItem::Completion(event).into())
            .await;
        crate::async_exec::executor::wake::notify_completion_wake(
            crate::async_exec::executor::wake::CompletionWakeNotice {
                session_key: session,
                task_id,
                tool_name: BROWSER_TOOL_NAME.to_string(),
                principal_id: Some(peko.principal_id.clone()),
                via_steering: false,
            },
        );
    }

    // ─── What a viewer does (D4, D5) ───────────────────────────────

    /// The browser as a viewer sees it: whether it runs, its tabs, who
    /// holds control, and whether the peko waits for its owner.
    pub async fn view(&self, peko: &PekoBrowser) -> Value {
        let running = peko.running().await;
        let tabs = match running.as_ref() {
            Some(browser) => browser.tabs().await.unwrap_or_default(),
            None => Vec::new(),
        };
        json!({
            "running": running.is_some(),
            "available": self.found().is_some(),
            "tabs": tabs,
            "controller": peko.controller().map(|(viewer, since)| json!({
                "viewer": viewer, "since": since.to_rfc3339(),
            })),
            "handoff": peko.handoff().map(|h| json!({
                "why": h.why, "tab": h.tab, "url": h.url, "at": h.at.to_rfc3339(),
            })),
        })
    }

    /// `viewer` takes or releases control.
    pub async fn control(
        &self,
        peko: &PekoBrowser,
        viewer: &str,
        take: bool,
    ) -> Result<ControlChange> {
        if take {
            // A hand-off is for a page: have the browser up for it.
            self.start(peko).await?;
            let mut state = peko.state();
            match state.controller.as_mut() {
                Some(c) if c.viewer == viewer => {
                    c.last_seen = Instant::now();
                    Ok(ControlChange::Unchanged)
                }
                Some(c) => Err(anyhow!(
                    "another of your devices has control ({})",
                    c.viewer
                )),
                None => {
                    state.controller = Some(Controller {
                        viewer: viewer.to_string(),
                        since: Utc::now(),
                        last_seen: Instant::now(),
                    });
                    Ok(ControlChange::Took)
                }
            }
        } else {
            let held = {
                let mut state = peko.state();
                if state
                    .controller
                    .as_ref()
                    .is_some_and(|c| c.viewer == viewer)
                {
                    state.controller = None;
                    true
                } else {
                    false
                }
            };
            if !held {
                return Ok(ControlChange::Unchanged);
            }
            self.forget_frame_file(peko);
            self.owner_done(peko, "Your owner is finished with the browser.")
                .await;
            Ok(ControlChange::Released)
        }
    }

    /// Remove the picture handed to the app on this computer: nothing
    /// of a page stays on disk once nobody is looking at it.
    fn forget_frame_file(&self, peko: &PekoBrowser) {
        let safe: String = peko
            .principal_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .collect();
        let _ = std::fs::remove_file(self.browser_dir.join("view").join(format!("{safe}.jpg")));
    }

    /// A viewer is still there: keep its control alive.
    fn seen(peko: &PekoBrowser, viewer: &str) {
        let mut state = peko.state();
        state.last_used = Some(Instant::now());
        if let Some(c) = state.controller.as_mut().filter(|c| c.viewer == viewer) {
            c.last_seen = Instant::now();
        }
    }

    /// The newest picture of a tab for `viewer` (see [`Browser::frame`]).
    pub async fn frame(
        &self,
        peko: &PekoBrowser,
        viewer: &str,
        tab: &str,
        size: (u32, u32),
        after: u64,
        wait: Duration,
    ) -> Result<Option<Frame>> {
        Self::seen(peko, viewer);
        let browser = self.start(peko).await?;
        browser.frame(tab, size.0, size.1, after, wait).await
    }

    /// Apply what `viewer` did to a tab; only the viewer holding control
    /// may.
    pub async fn input(
        &self,
        peko: &PekoBrowser,
        viewer: &str,
        tab: &str,
        events: &[Value],
    ) -> Result<()> {
        anyhow::ensure!(
            peko.state()
                .controller
                .as_ref()
                .is_some_and(|c| c.viewer == viewer),
            "take control of the browser first"
        );
        Self::seen(peko, viewer);
        let browser = peko
            .running()
            .await
            .ok_or_else(|| anyhow!("the browser is not running"))?;
        browser.input(tab, events).await
    }

    /// Open `url` for a viewer holding control (its address bar).
    pub async fn navigate(
        &self,
        peko: &PekoBrowser,
        viewer: &str,
        tab: Option<&str>,
        url: &str,
    ) -> Result<String> {
        anyhow::ensure!(
            peko.state()
                .controller
                .as_ref()
                .is_some_and(|c| c.viewer == viewer),
            "take control of the browser first"
        );
        Self::seen(peko, viewer);
        let browser = self.start(peko).await?;
        browser.open(url, tab).await
    }

    /// Release control of viewers that went away, end watches nobody
    /// asks about, and stop browsers nobody uses.
    pub async fn tidy(&self) {
        let pekos: Vec<Arc<PekoBrowser>> = self
            .pekos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for peko in pekos {
            let gone = {
                let mut state = peko.state();
                if state
                    .controller
                    .as_ref()
                    .is_some_and(|c| c.last_seen.elapsed() > VIEWER_GONE)
                {
                    state.controller.take().map(|c| c.viewer)
                } else {
                    None
                }
            };
            if let Some(viewer) = gone {
                self.forget_frame_file(&peko);
                self.audit_control(&peko, &viewer, "viewer_gone").await;
                self.owner_done(
                    &peko,
                    "Your owner's device went away while it had the browser.",
                )
                .await;
            }
            let Some(browser) = peko.running().await else {
                continue;
            };
            let watched = browser.end_idle_watch(Duration::from_secs(20)).await;
            if !watched {
                self.forget_frame_file(&peko);
            }
            let idle = {
                let state = peko.state();
                state.controller.is_none()
                    && state.last_used.is_none_or(|at| at.elapsed() > IDLE_STOP)
            };
            if idle && !watched && peko.handoff().is_none() {
                tracing::info!(peko = %peko.principal_id, "browser idle; stopping it");
                *peko.browser.lock().await = None;
            }
        }
    }
}

#[cfg(test)]
mod tests;
