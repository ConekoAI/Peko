//! One running browser: its tabs, reading and acting on a page, and the
//! picture of a tab a viewer watches (ADR-119 D1, D2, D5).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio::sync::Notify;

use super::cdp::{self, Cdp, Event};
use crate::tools::builtin::process::{spawn_in_own_group, KillTreeOnDrop};

/// The size a tab is laid out to when no viewer says otherwise.
pub const DEFAULT_WIDTH: u32 = 1280;
pub const DEFAULT_HEIGHT: u32 = 800;

/// Most tabs one browser keeps open.
pub const MAX_TABS: usize = 12;

/// The largest picture of a page sent to a viewer, as base64 text
/// (256 KiB of JPEG).
pub const MAX_FRAME_BASE64: usize = 256 * 1024 * 4 / 3;

/// How long a page may take to finish loading before it is read as it is.
const LOAD_WAIT: Duration = Duration::from_secs(15);

/// How long the browser may take to start.
const START_WAIT: Duration = Duration::from_secs(20);

/// How the browser is driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// A pipe the browser inherits; nothing else can reach it.
    Pipe,
    /// A WebSocket on a loopback port the browser picks.
    Port,
}

impl Transport {
    /// The pipe where one can be passed, else the port.
    #[must_use]
    pub fn for_this_platform() -> Self {
        if cfg!(unix) && std::env::var_os("PEKO_BROWSER_TRANSPORT").is_none_or(|v| v != "port") {
            Self::Pipe
        } else {
            Self::Port
        }
    }
}

/// One open tab.
#[derive(Debug, Clone)]
struct Tab {
    target: String,
    session: String,
    /// The isolated script context reads and clicks share; gone after a
    /// navigation.
    world: Option<i64>,
}

/// A tab as it is listed.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct TabInfo {
    pub tab: String,
    pub url: String,
    pub title: String,
}

/// A page as text.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub url: String,
    pub title: String,
    pub text: String,
    /// Characters of the page's text in all.
    pub total_chars: usize,
}

/// One picture of a watched tab.
#[derive(Debug, Clone)]
pub struct Frame {
    pub seq: u64,
    pub tab: String,
    pub width: u32,
    pub height: u32,
    /// JPEG, base64.
    pub data: Arc<str>,
}

struct Watch {
    tab: String,
    session: String,
    width: u32,
    height: u32,
    quality: u32,
    latest: Option<Frame>,
    last_asked: Instant,
}

#[derive(Default)]
struct Tabs {
    by_id: BTreeMap<String, Tab>,
    next: u32,
}

/// A running browser.
pub struct Browser {
    cdp: Arc<Cdp>,
    tabs: tokio::sync::Mutex<Tabs>,
    user_agent: String,
    /// `158.0.8127.36`
    full_version: String,
    watch: Mutex<Option<Watch>>,
    seq: std::sync::atomic::AtomicU64,
    frame_ready: Notify,
    /// What a page's `alert` / `confirm` last said, per tab session.
    dialogs: Mutex<BTreeMap<String, String>>,
    _child: Mutex<Option<Child>>,
    _tree: KillTreeOnDrop,
}

impl std::fmt::Debug for Browser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Browser").finish_non_exhaustive()
    }
}

impl Browser {
    /// Start `executable` headless on `profile` and connect to it.
    pub async fn launch(
        executable: &Path,
        profile: &Path,
        transport: Transport,
    ) -> Result<Arc<Self>> {
        std::fs::create_dir_all(profile)
            .with_context(|| format!("create {}", profile.display()))?;
        let port_file = profile.join("DevToolsActivePort");
        let _ = std::fs::remove_file(&port_file);

        let mut command = Command::new(executable);
        command
            .arg("--headless=new")
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg(format!("--window-size={DEFAULT_WIDTH},{DEFAULT_HEIGHT}"))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                // The profile is the peko's own; it does not ask the OS
                // keychain for a key (no prompt on the owner's screen).
                "--use-mock-keychain",
                "--password-store=basic",
                "--disable-sync",
                "--disable-component-update",
                "--disable-features=Translate,MediaRouter",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let (cdp, child, tree) = match transport {
            Transport::Pipe => launch_on_pipe(command)?,
            Transport::Port => {
                command.arg("--remote-debugging-port=0");
                let (child, tree) = spawn_in_own_group(&mut command)
                    .with_context(|| format!("start {}", executable.display()))?;
                let url = wait_for_port(&port_file).await?;
                (cdp::over_websocket(&url).await?, child, tree)
            }
        };

        let version =
            tokio::time::timeout(START_WAIT, cdp.call(None, "Browser.getVersion", json!({})))
                .await
                .map_err(|_| anyhow!("the browser did not start in time"))??;
        // It is Chrome; a headless one names itself differently and many
        // sites turn that name away.
        let user_agent = version["userAgent"]
            .as_str()
            .unwrap_or_default()
            .replace("HeadlessChrome", "Chrome");
        let full_version = version["product"]
            .as_str()
            .and_then(|p| p.split('/').nth(1))
            .unwrap_or_default()
            .to_string();
        cdp.call(None, "Target.setDiscoverTargets", json!({"discover": true}))
            .await?;
        // No downloads in this version (ADR-119 D2).
        let _ = cdp
            .call(
                None,
                "Browser.setDownloadBehavior",
                json!({"behavior": "deny"}),
            )
            .await;

        let browser = Arc::new(Self {
            cdp,
            tabs: tokio::sync::Mutex::default(),
            user_agent,
            full_version,
            watch: Mutex::new(None),
            seq: std::sync::atomic::AtomicU64::new(0),
            frame_ready: Notify::new(),
            dialogs: Mutex::default(),
            _child: Mutex::new(Some(child)),
            _tree: tree,
        });
        browser.listen();
        Ok(browser)
    }

    /// Whether the browser still answers.
    pub async fn alive(&self) -> bool {
        self.cdp
            .call(None, "Browser.getVersion", json!({}))
            .await
            .is_ok()
    }

    /// React to what the browser reports: closed tabs, page dialogs, and
    /// pictures of the watched tab.
    fn listen(self: &Arc<Self>) {
        let mut events = self.cdp.subscribe();
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                let Some(browser) = weak.upgrade() else { break };
                browser.on_event(&event).await;
            }
        });
    }

    async fn on_event(&self, event: &Event) {
        match event.method.as_str() {
            "Target.targetDestroyed" => {
                let Some(target) = event.params["targetId"].as_str() else {
                    return;
                };
                self.tabs
                    .lock()
                    .await
                    .by_id
                    .retain(|_, tab| tab.target != target);
            }
            "Page.javascriptDialogOpening" => {
                // Nothing can answer a dialog later, and it blocks the
                // page: accept it and remember what it said.
                let Some(session) = event.session.as_deref() else {
                    return;
                };
                let message = event.params["message"].as_str().unwrap_or_default();
                self.dialogs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(session.to_string(), message.chars().take(300).collect());
                let _ = self
                    .cdp
                    .call(
                        Some(session),
                        "Page.handleJavaScriptDialog",
                        json!({"accept": true}),
                    )
                    .await;
            }
            "Page.frameNavigated" if event.params["frame"]["parentId"].is_null() => {
                let Some(session) = event.session.as_deref() else {
                    return;
                };
                for tab in self.tabs.lock().await.by_id.values_mut() {
                    if tab.session == session {
                        tab.world = None;
                    }
                }
            }
            "Page.screencastFrame" => {
                let Some(session) = event.session.as_deref() else {
                    return;
                };
                let _ = self
                    .cdp
                    .call(
                        Some(session),
                        "Page.screencastFrameAck",
                        json!({"sessionId": event.params["sessionId"]}),
                    )
                    .await;
                let Some(data) = event.params["data"].as_str() else {
                    return;
                };
                let too_large = data.len() > MAX_FRAME_BASE64;
                let mut lower_to = None;
                {
                    let mut watch = self.watch.lock().unwrap_or_else(|e| e.into_inner());
                    let Some(watch) = watch.as_mut().filter(|w| w.session == session) else {
                        return;
                    };
                    if too_large && watch.quality > 20 {
                        watch.quality = watch.quality.saturating_sub(15).max(20);
                        lower_to = Some((watch.width, watch.height, watch.quality));
                    } else if !too_large {
                        let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        watch.latest = Some(Frame {
                            seq,
                            tab: watch.tab.clone(),
                            width: watch.width,
                            height: watch.height,
                            data: Arc::from(data),
                        });
                    }
                }
                if let Some((width, height, quality)) = lower_to {
                    let _ = self
                        .cdp
                        .call(Some(session), "Page.stopScreencast", json!({}))
                        .await;
                    let _ = self.start_screencast(session, width, height, quality).await;
                } else {
                    self.frame_ready.notify_waiters();
                }
            }
            _ => {}
        }
    }

    // ─── Tabs ──────────────────────────────────────────────────────

    async fn attach(&self, target: &str) -> Result<Tab> {
        let attached = self
            .cdp
            .call(
                None,
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
            )
            .await?;
        let session = attached["sessionId"]
            .as_str()
            .ok_or_else(|| anyhow!("the browser attached no session"))?
            .to_string();
        self.cdp
            .call(Some(&session), "Page.enable", json!({}))
            .await?;
        if !self.user_agent.is_empty() {
            let _ = self
                .cdp
                .call(
                    Some(&session),
                    "Emulation.setUserAgentOverride",
                    json!({
                        "userAgent": self.user_agent,
                        "userAgentMetadata": client_hints(&self.full_version),
                    }),
                )
                .await;
        }
        Ok(Tab {
            target: target.to_string(),
            session,
            world: None,
        })
    }

    /// Every page tab the browser has, attaching the ones a page opened
    /// itself.
    async fn sync_tabs(&self) -> Result<()> {
        let targets = self.cdp.call(None, "Target.getTargets", json!({})).await?;
        let pages: Vec<String> = targets["targetInfos"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["type"] == "page")
            .filter_map(|t| t["targetId"].as_str().map(str::to_string))
            .collect();
        let mut tabs = self.tabs.lock().await;
        tabs.by_id.retain(|_, tab| pages.contains(&tab.target));
        for target in pages {
            if tabs.by_id.values().any(|tab| tab.target == target) {
                continue;
            }
            // A tab on its way out is still listed for a moment.
            let Ok(tab) = self.attach(&target).await else {
                continue;
            };
            tabs.next += 1;
            let id = format!("t{}", tabs.next);
            tabs.by_id.insert(id, tab);
        }
        Ok(())
    }

    /// The open tabs, oldest first.
    pub async fn tabs(&self) -> Result<Vec<TabInfo>> {
        self.sync_tabs().await?;
        let targets = self.cdp.call(None, "Target.getTargets", json!({})).await?;
        let tabs = self.tabs.lock().await;
        let mut listed: Vec<(u32, TabInfo)> = tabs
            .by_id
            .iter()
            .map(|(id, tab)| {
                let info = targets["targetInfos"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|t| t["targetId"] == tab.target.as_str());
                let text = |key: &str| {
                    info.and_then(|t| t[key].as_str())
                        .unwrap_or_default()
                        .to_string()
                };
                (
                    id[1..].parse().unwrap_or(0),
                    TabInfo {
                        tab: id.clone(),
                        url: text("url"),
                        title: text("title"),
                    },
                )
            })
            .collect();
        listed.sort_by_key(|(n, _)| *n);
        Ok(listed.into_iter().map(|(_, info)| info).collect())
    }

    async fn tab(&self, id: &str) -> Result<Tab> {
        if let Some(tab) = self.tabs.lock().await.by_id.get(id) {
            return Ok(tab.clone());
        }
        self.sync_tabs().await?;
        self.tabs
            .lock()
            .await
            .by_id
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("no tab `{id}`; `tabs` lists the open ones"))
    }

    async fn new_tab(&self) -> Result<String> {
        self.sync_tabs().await?;
        // A fresh browser starts with one empty tab: use it.
        let blank = self
            .tabs()
            .await?
            .into_iter()
            .find(|t| t.url == "about:blank" || t.url.starts_with("chrome://newtab"));
        if let Some(blank) = blank {
            return Ok(blank.tab);
        }
        anyhow::ensure!(
            self.tabs.lock().await.by_id.len() < MAX_TABS,
            "{MAX_TABS} tabs are open; close one first"
        );
        let created = self
            .cdp
            .call(None, "Target.createTarget", json!({"url": "about:blank"}))
            .await?;
        let target = created["targetId"]
            .as_str()
            .ok_or_else(|| anyhow!("the browser opened no tab"))?;
        let tab = self.attach(target).await?;
        let mut tabs = self.tabs.lock().await;
        tabs.next += 1;
        let id = format!("t{}", tabs.next);
        tabs.by_id.insert(id.clone(), tab);
        Ok(id)
    }

    /// Close a tab.
    pub async fn close(&self, id: &str) -> Result<()> {
        let tab = self.tab(id).await?;
        self.cdp
            .call(None, "Target.closeTarget", json!({"targetId": tab.target}))
            .await?;
        self.tabs.lock().await.by_id.remove(id);
        // Closing finishes a moment later; wait so the tab is not listed
        // (and picked up again) meanwhile.
        for _ in 0..20 {
            let targets = self.cdp.call(None, "Target.getTargets", json!({})).await?;
            let listed = targets["targetInfos"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|t| t["targetId"] == tab.target.as_str());
            if !listed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let watched = self
            .watch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|w| w.tab == id);
        if watched {
            *self.watch.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        Ok(())
    }

    // ─── Scripts ───────────────────────────────────────────────────

    async fn world(&self, id: &str, tab: &Tab) -> Result<i64> {
        if let Some(world) = tab.world {
            return Ok(world);
        }
        let tree = self
            .cdp
            .call(Some(&tab.session), "Page.getFrameTree", json!({}))
            .await?;
        let frame = tree["frameTree"]["frame"]["id"]
            .as_str()
            .ok_or_else(|| anyhow!("the tab has no page"))?;
        let created = self
            .cdp
            .call(
                Some(&tab.session),
                "Page.createIsolatedWorld",
                json!({"frameId": frame, "worldName": "peko"}),
            )
            .await?;
        let world = created["executionContextId"]
            .as_i64()
            .ok_or_else(|| anyhow!("the tab gave no script context"))?;
        if let Some(stored) = self.tabs.lock().await.by_id.get_mut(id) {
            stored.world = Some(world);
        }
        Ok(world)
    }

    async fn forget_world(&self, id: &str) {
        if let Some(stored) = self.tabs.lock().await.by_id.get_mut(id) {
            stored.world = None;
        }
    }

    /// Run `expression` in the tab's own script context, apart from the
    /// page's scripts, and return its value.
    async fn eval(&self, id: &str, expression: &str) -> Result<Value> {
        let mut last = anyhow!("the page is not ready");
        for _ in 0..2 {
            let tab = self.tab(id).await?;
            let world = self.world(id, &tab).await?;
            let answer = self
                .cdp
                .call(
                    Some(&tab.session),
                    "Runtime.evaluate",
                    json!({
                        "expression": expression,
                        "contextId": world,
                        "returnByValue": true,
                        "awaitPromise": true,
                    }),
                )
                .await;
            match answer {
                Ok(result) => {
                    if let Some(details) = result.get("exceptionDetails") {
                        bail!(
                            "the page script failed: {}",
                            details["exception"]["description"]
                                .as_str()
                                .or_else(|| details["text"].as_str())
                                .unwrap_or("unknown error")
                        );
                    }
                    return Ok(result["result"]["value"].clone());
                }
                // The page moved on and took the context with it.
                Err(e) => {
                    self.forget_world(id).await;
                    last = e;
                }
            }
        }
        Err(last)
    }

    #[cfg(test)]
    pub(crate) async fn eval_for_tests(&self, id: &str, expression: &str) -> Value {
        self.eval(id, expression).await.unwrap()
    }

    /// Wait for the page to finish loading (or [`LOAD_WAIT`]).
    async fn settle(&self, id: &str) {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let until = Instant::now() + LOAD_WAIT;
        while Instant::now() < until {
            if let Ok(state) = self.eval(id, "document.readyState").await {
                if state == "complete" {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// What a page dialog said since the last time this was asked.
    async fn take_dialog(&self, id: &str) -> Option<String> {
        let tab = self.tab(id).await.ok()?;
        self.dialogs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&tab.session)
    }

    // ─── What a peko does ──────────────────────────────────────────

    /// Open `url` in tab `id`, or in a new tab. Returns the tab.
    pub async fn open(&self, url: &str, id: Option<&str>) -> Result<String> {
        let parsed = url::Url::parse(url).map_err(|e| anyhow!("`{url}` is not an address: {e}"))?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "only http and https addresses open in the browser"
        );
        let id = match id {
            Some(id) => {
                self.tab(id).await?;
                id.to_string()
            }
            None => self.new_tab().await?,
        };
        let tab = self.tab(&id).await?;
        let navigated = self
            .cdp
            .call(Some(&tab.session), "Page.navigate", json!({"url": url}))
            .await?;
        if let Some(error) = navigated["errorText"].as_str().filter(|e| !e.is_empty()) {
            // A download or a blocked request is not a page.
            bail!("{url} did not open: {error}");
        }
        self.forget_world(&id).await;
        self.settle(&id).await;
        Ok(id)
    }

    /// The page in tab `id` as text, from character `offset`, at most
    /// `max_chars`.
    pub async fn read(&self, id: &str, offset: usize, max_chars: usize) -> Result<Snapshot> {
        let value = self.eval(id, SNAPSHOT_JS).await?;
        let text = value["text"].as_str().unwrap_or_default();
        let total_chars = text.chars().count();
        let text: String = text.chars().skip(offset).take(max_chars).collect();
        Ok(Snapshot {
            url: value["url"].as_str().unwrap_or_default().to_string(),
            title: value["title"].as_str().unwrap_or_default().to_string(),
            text,
            total_chars,
        })
    }

    /// Where control `reference` is on the page, scrolled into view.
    async fn locate(&self, id: &str, reference: u64) -> Result<Value> {
        let found = self
            .eval(id, &format!("({LOCATE_JS})({reference})"))
            .await?;
        if let Some(error) = found["error"].as_str() {
            bail!(
                "{}",
                match error {
                    "stale" => format!(
                        "[{reference}] is not on the page any more; `read` it again for fresh \
                         references"
                    ),
                    "hidden" => format!("[{reference}] is not visible on the page"),
                    other => other.to_string(),
                }
            );
        }
        Ok(found)
    }

    /// Click control `reference`.
    pub async fn click(&self, id: &str, reference: u64) -> Result<()> {
        let at = self.locate(id, reference).await?;
        let (x, y) = (
            at["x"].as_f64().unwrap_or(0.0),
            at["y"].as_f64().unwrap_or(0.0),
        );
        let tab = self.tab(id).await?;
        self.mouse_click(&tab.session, x, y).await?;
        self.settle(id).await;
        Ok(())
    }

    /// Put `text` into control `reference`, replacing what it holds; for
    /// a list, choose the option `text` names.
    pub async fn type_text(
        &self,
        id: &str,
        reference: u64,
        text: &str,
        submit: bool,
    ) -> Result<()> {
        let at = self.locate(id, reference).await?;
        let tab = self.tab(id).await?;
        if at["tag"] == "select" {
            let chosen = self
                .eval(id, &format!("({SELECT_JS})({reference}, {})", json!(text)))
                .await?;
            if let Some(error) = chosen["error"].as_str() {
                bail!("{error}");
            }
        } else {
            let (x, y) = (
                at["x"].as_f64().unwrap_or(0.0),
                at["y"].as_f64().unwrap_or(0.0),
            );
            self.mouse_click(&tab.session, x, y).await?;
            self.eval(id, &format!("({FOCUS_JS})({reference})")).await?;
            self.cdp
                .call(
                    Some(&tab.session),
                    "Input.insertText",
                    json!({"text": text}),
                )
                .await?;
        }
        if submit {
            self.key(&tab.session, "Enter").await?;
            self.settle(id).await;
        }
        Ok(())
    }

    /// Press one key in tab `id`.
    pub async fn press(&self, id: &str, key: &str) -> Result<()> {
        let tab = self.tab(id).await?;
        self.key(&tab.session, key).await?;
        self.settle(id).await;
        Ok(())
    }

    /// A JPEG of what tab `id` shows.
    pub async fn screenshot(&self, id: &str) -> Result<Vec<u8>> {
        use base64::Engine;
        let tab = self.tab(id).await?;
        let shot = self
            .cdp
            .call(
                Some(&tab.session),
                "Page.captureScreenshot",
                json!({"format": "jpeg", "quality": 70}),
            )
            .await?;
        base64::engine::general_purpose::STANDARD
            .decode(shot["data"].as_str().unwrap_or_default())
            .context("the browser returned no picture")
    }

    /// The tab's address, title, and what a dialog said, after an action.
    pub async fn after(&self, id: &str) -> Value {
        let info = self
            .eval(id, "({url: location.href, title: document.title})")
            .await
            .unwrap_or(Value::Null);
        let mut result = json!({
            "tab": id,
            "url": info["url"].as_str().unwrap_or_default(),
            "title": info["title"].as_str().unwrap_or_default(),
        });
        if let Some(dialog) = self.take_dialog(id).await {
            result["dialog"] = json!(dialog);
        }
        result
    }

    // ─── Input ─────────────────────────────────────────────────────

    async fn mouse(&self, session: &str, kind: &str, x: f64, y: f64, extra: Value) -> Result<()> {
        let mut params = json!({"type": kind, "x": x, "y": y});
        if let (Some(params), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            params.extend(extra.clone());
        }
        self.cdp
            .call(Some(session), "Input.dispatchMouseEvent", params)
            .await
            .map(|_| ())
    }

    async fn mouse_click(&self, session: &str, x: f64, y: f64) -> Result<()> {
        self.mouse(session, "mouseMoved", x, y, json!({})).await?;
        let button = json!({"button": "left", "clickCount": 1});
        self.mouse(session, "mousePressed", x, y, button.clone())
            .await?;
        self.mouse(session, "mouseReleased", x, y, button).await
    }

    async fn key(&self, session: &str, key: &str) -> Result<()> {
        let (code, key_code, text) = key_definition(key).ok_or_else(|| {
            anyhow!(
                "unknown key `{key}`; use one of: {}",
                KEYS.iter().map(|k| k.0).collect::<Vec<_>>().join(", ")
            )
        })?;
        let mut down = json!({
            "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
            "key": key, "code": code,
            "windowsVirtualKeyCode": key_code,
        });
        if let Some(text) = text {
            down["text"] = json!(text);
        }
        self.cdp
            .call(Some(session), "Input.dispatchKeyEvent", down)
            .await?;
        self.cdp
            .call(
                Some(session),
                "Input.dispatchKeyEvent",
                json!({
                    "type": "keyUp", "key": key, "code": code,
                    "windowsVirtualKeyCode": key_code,
                }),
            )
            .await
            .map(|_| ())
    }

    /// Apply what a viewer did to tab `id`: pointer events, keys, and
    /// pieces of text (ADR-119 D5). Coordinates are in the size the
    /// viewer asked the tab to be laid out to.
    pub async fn input(&self, id: &str, events: &[Value]) -> Result<()> {
        let tab = self.tab(id).await?;
        let session = tab.session.as_str();
        for event in events {
            let number = |key: &str| event[key].as_f64().unwrap_or(0.0);
            let (x, y) = (number("x"), number("y"));
            let button = json!({"button": "left", "clickCount": 1});
            match event["t"].as_str().unwrap_or_default() {
                "click" => self.mouse_click(session, x, y).await?,
                "move" => {
                    let held = if event["held"].as_bool().unwrap_or(false) {
                        json!({"button": "left", "buttons": 1})
                    } else {
                        json!({})
                    };
                    self.mouse(session, "mouseMoved", x, y, held).await?;
                }
                "down" => self.mouse(session, "mousePressed", x, y, button).await?,
                "up" => self.mouse(session, "mouseReleased", x, y, button).await?,
                "wheel" => {
                    self.mouse(
                        session,
                        "mouseWheel",
                        x,
                        y,
                        json!({"deltaX": number("dx"), "deltaY": number("dy")}),
                    )
                    .await?;
                }
                "key" => {
                    self.key(session, event["key"].as_str().unwrap_or_default())
                        .await?;
                }
                "text" => {
                    let text = event["text"].as_str().unwrap_or_default();
                    if !text.is_empty() {
                        self.cdp
                            .call(Some(session), "Input.insertText", json!({"text": text}))
                            .await?;
                    }
                }
                "back" => {
                    self.eval(id, "history.back()").await?;
                }
                "reload" => {
                    self.cdp
                        .call(Some(session), "Page.reload", json!({}))
                        .await?;
                }
                other => bail!("unknown input `{other}`"),
            }
        }
        Ok(())
    }

    // ─── The picture a viewer watches ──────────────────────────────

    async fn start_screencast(
        &self,
        session: &str,
        width: u32,
        height: u32,
        quality: u32,
    ) -> Result<()> {
        let scale = pixel_ratio(width);
        self.cdp
            .call(
                Some(session),
                "Page.startScreencast",
                json!({
                    "format": "jpeg",
                    "quality": quality,
                    "maxWidth": width * scale,
                    "maxHeight": height * scale,
                    "everyNthFrame": 1,
                }),
            )
            .await
            .map(|_| ())
    }

    /// The newest picture of tab `id` laid out `width` × `height`, once
    /// there is one newer than `after`; `None` when `wait` passes first.
    /// Asking starts the watch; a watch nobody asks about is ended by
    /// [`Self::end_idle_watch`].
    pub async fn frame(
        &self,
        id: &str,
        width: u32,
        height: u32,
        after: u64,
        wait: Duration,
    ) -> Result<Option<Frame>> {
        let width = width.clamp(240, 1920);
        let height = height.clamp(240, 1920);
        let same = {
            let mut watch = self.watch.lock().unwrap_or_else(|e| e.into_inner());
            match watch.as_mut() {
                Some(w) if w.tab == id && w.width == width && w.height == height => {
                    w.last_asked = Instant::now();
                    true
                }
                _ => false,
            }
        };
        if !same {
            self.end_watch().await;
            let tab = self.tab(id).await?;
            let scale = pixel_ratio(width);
            self.cdp
                .call(
                    Some(&tab.session),
                    "Emulation.setDeviceMetricsOverride",
                    json!({
                        "width": width, "height": height,
                        "deviceScaleFactor": scale, "mobile": width < 600,
                    }),
                )
                .await?;
            let _ = self
                .cdp
                .call(Some(&tab.session), "Page.bringToFront", json!({}))
                .await;
            *self.watch.lock().unwrap_or_else(|e| e.into_inner()) = Some(Watch {
                tab: id.to_string(),
                session: tab.session.clone(),
                width,
                height,
                quality: 60,
                latest: None,
                last_asked: Instant::now(),
            });
            self.start_screencast(&tab.session, width, height, 60)
                .await?;
        }
        let until = Instant::now() + wait;
        loop {
            let ready = self.frame_ready.notified();
            let newest = self
                .watch
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .and_then(|w| w.latest.clone());
            if let Some(frame) = newest.filter(|f| f.seq > after) {
                return Ok(Some(frame));
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, ready).await.is_err() {
                return Ok(None);
            }
        }
    }

    /// Stop sending pictures and lay the tab out at its usual size again.
    pub async fn end_watch(&self) {
        let ended = self.watch.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(watch) = ended {
            let _ = self
                .cdp
                .call(Some(&watch.session), "Page.stopScreencast", json!({}))
                .await;
            let _ = self
                .cdp
                .call(
                    Some(&watch.session),
                    "Emulation.clearDeviceMetricsOverride",
                    json!({}),
                )
                .await;
        }
    }

    /// End the watch when no viewer asked for a picture within `idle`.
    /// Returns whether a watch is still running.
    pub async fn end_idle_watch(&self, idle: Duration) -> bool {
        let state = self
            .watch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|w| w.last_asked.elapsed() > idle);
        match state {
            Some(true) => {
                self.end_watch().await;
                false
            }
            Some(false) => true,
            None => false,
        }
    }
}

/// What the browser says of itself in client hints: the same Chrome,
/// without the headless name (overriding the name alone would leave
/// these empty).
fn client_hints(full_version: &str) -> Value {
    let major = full_version.split('.').next().unwrap_or_default();
    let brands = |version: &str| {
        json!([
            {"brand": "Chromium", "version": version},
            {"brand": "Google Chrome", "version": version},
            {"brand": "Not?A_Brand", "version": "99"},
        ])
    };
    json!({
        "brands": brands(major),
        "fullVersionList": brands(full_version),
        "platform": match std::env::consts::OS {
            "macos" => "macOS",
            "windows" => "Windows",
            _ => "Linux",
        },
        "platformVersion": "",
        "architecture": if std::env::consts::ARCH == "aarch64" { "arm" } else { "x86" },
        "model": "",
        "mobile": false,
    })
}

/// Pictures for a phone-sized view are sent at twice its size, so they
/// stay sharp on its screen.
fn pixel_ratio(width: u32) -> u32 {
    if width < 600 {
        2
    } else {
        1
    }
}

/// `(name, code, key code, text)` of the keys a peko or a viewer may press.
const KEYS: &[(&str, &str, u32, Option<&str>)] = &[
    ("Enter", "Enter", 13, Some("\r")),
    ("Tab", "Tab", 9, None),
    ("Escape", "Escape", 27, None),
    ("Backspace", "Backspace", 8, None),
    ("Delete", "Delete", 46, None),
    ("Space", "Space", 32, Some(" ")),
    ("ArrowUp", "ArrowUp", 38, None),
    ("ArrowDown", "ArrowDown", 40, None),
    ("ArrowLeft", "ArrowLeft", 37, None),
    ("ArrowRight", "ArrowRight", 39, None),
    ("PageUp", "PageUp", 33, None),
    ("PageDown", "PageDown", 34, None),
    ("Home", "Home", 36, None),
    ("End", "End", 35, None),
];

fn key_definition(key: &str) -> Option<(&'static str, u32, Option<&'static str>)> {
    KEYS.iter()
        .find(|k| k.0.eq_ignore_ascii_case(key))
        .map(|k| (k.1, k.2, k.3))
}

/// The names of the keys [`Browser::press`] takes.
#[must_use]
pub fn key_names() -> Vec<&'static str> {
    KEYS.iter().map(|k| k.0).collect()
}

#[cfg(unix)]
fn launch_on_pipe(mut command: Command) -> Result<(Arc<Cdp>, Child, KillTreeOnDrop)> {
    use std::os::fd::AsRawFd;
    let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
    let fd = theirs.as_raw_fd();
    command.arg("--remote-debugging-pipe");
    // SAFETY: only async-signal-safe calls (`dup2`, `fcntl`) run between
    // fork and exec. The browser reads its commands on fd 3 and writes
    // its answers on fd 4; both are our socket's other end.
    unsafe {
        command.pre_exec(move || {
            for target in [3, 4] {
                if fd != target && libc::dup2(fd, target) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(target, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let (child, tree) = spawn_in_own_group(&mut command).context("start the browser")?;
    drop(theirs);
    ours.set_nonblocking(true)?;
    let (read, write) = tokio::net::UnixStream::from_std(ours)?.into_split();
    Ok((cdp::over_pipe(read, write), child, tree))
}

#[cfg(not(unix))]
fn launch_on_pipe(_command: Command) -> Result<(Arc<Cdp>, Child, KillTreeOnDrop)> {
    bail!("the browser's pipe is not available on this platform")
}

/// Wait for the browser to write the port it listens on.
async fn wait_for_port(port_file: &Path) -> Result<String> {
    let until = Instant::now() + START_WAIT;
    loop {
        if let Ok(text) = std::fs::read_to_string(port_file) {
            let mut lines = text.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                if port.parse::<u16>().is_ok() && path.starts_with('/') {
                    return Ok(format!("ws://127.0.0.1:{port}{path}"));
                }
            }
        }
        anyhow::ensure!(Instant::now() < until, "the browser did not start in time");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The page as text, with a numbered reference on each control. Runs in
/// the tab's isolated context; the references live there until the page
/// navigates.
const SNAPSHOT_JS: &str = r#"(() => {
  const refs = [];
  globalThis.__pekoRefs = refs;
  const out = [];
  const push = s => { out.push(s); };
  const clean = s => (s || '').replace(/\s+/g, ' ').trim();
  const SKIP = new Set(['SCRIPT','STYLE','NOSCRIPT','TEMPLATE','HEAD','META','LINK','TITLE']);
  const BLOCK = new Set(['P','DIV','SECTION','ARTICLE','HEADER','FOOTER','MAIN','NAV','ASIDE','UL','OL','LI','TABLE','TR','FORM','FIELDSET','H1','H2','H3','H4','H5','H6','BLOCKQUOTE','PRE','DL','DT','DD','HR','BR','DETAILS','FIGURE']);
  const ROLES = new Set(['button','link','checkbox','radio','tab','menuitem','option','switch','combobox','textbox','searchbox']);
  const visible = el => {
    const s = getComputedStyle(el);
    return s.display !== 'none' && s.visibility !== 'hidden';
  };
  const labelOf = el => {
    let label = el.getAttribute('aria-label');
    if (!label && el.labels && el.labels.length) label = el.labels[0].innerText;
    if (!label && el.id) {
      const by = el.ownerDocument.querySelector('label[for="' + CSS.escape(el.id) + '"]');
      if (by) label = by.innerText;
    }
    if (!label && !['INPUT','TEXTAREA','SELECT'].includes(el.tagName)) label = el.innerText;
    if (!label) label = el.getAttribute('placeholder') || el.getAttribute('title') || el.getAttribute('alt') || el.getAttribute('name') || '';
    if (!label) { const img = el.querySelector && el.querySelector('img[alt]'); if (img) label = img.alt; }
    return clean(label).slice(0, 120);
  };
  const describe = el => {
    const tag = el.tagName;
    const role = (el.getAttribute('role') || '').toLowerCase();
    if (tag === 'A' && el.hasAttribute('href')) return 'link';
    if (tag === 'BUTTON' || tag === 'SUMMARY') return 'button';
    if (tag === 'SELECT') return 'list';
    if (tag === 'TEXTAREA') return 'field';
    if (tag === 'INPUT') {
      const type = (el.type || 'text').toLowerCase();
      if (type === 'hidden') return null;
      if (['submit','button','reset','image'].includes(type)) return 'button';
      if (type === 'checkbox' || type === 'radio') return type;
      return type === 'text' ? 'field' : 'field (' + type + ')';
    }
    if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) return 'editable';
    if (ROLES.has(role)) return role;
    if (el.hasAttribute('onclick')) return 'clickable';
    if (getComputedStyle(el).cursor === 'pointer' && !(el.parentElement && getComputedStyle(el.parentElement).cursor === 'pointer')) return 'clickable';
    return null;
  };
  const control = (el, kind) => {
    refs.push(el);
    let line = '[' + refs.length + '] ' + kind;
    const label = labelOf(el);
    if (label) line += ' "' + label + '"';
    const tag = el.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA') {
      const type = (el.type || '').toLowerCase();
      if (type === 'checkbox' || type === 'radio') line += el.checked ? ' (checked)' : ' (not checked)';
      else if (type === 'password') line += el.value ? ' = (filled)' : ' = (empty)';
      else if (el.value) line += ' = "' + clean(el.value).slice(0, 120) + '"';
      else line += ' = (empty)';
    } else if (tag === 'SELECT') {
      const chosen = el.selectedOptions[0];
      line += ' = "' + clean(chosen ? chosen.text : '') + '"';
      const options = Array.from(el.options).slice(0, 30).map(o => clean(o.text)).filter(Boolean);
      if (options.length) line += ' options: ' + options.join(' | ');
    }
    if (el.disabled) line += ' (disabled)';
    push('\n' + line + '\n');
  };
  const walk = node => {
    if (node.nodeType === 3) { const t = node.nodeValue; if (t && t.trim()) push(t.replace(/\s+/g, ' ')); return; }
    if (node.nodeType !== 1) return;
    const el = node;
    if (SKIP.has(el.tagName)) return;
    if (!visible(el)) return;
    const kind = describe(el);
    if (kind) { control(el, kind); return; }
    if (el.tagName === 'IMG') { const alt = clean(el.alt); if (alt) push(' (image: ' + alt.slice(0, 120) + ') '); return; }
    if (el.tagName === 'SVG' || el.tagName === 'svg' || el.tagName === 'CANVAS' || el.tagName === 'VIDEO') return;
    if (el.tagName === 'IFRAME') {
      try { if (el.contentDocument && el.contentDocument.body) { push('\n'); walk(el.contentDocument.body); push('\n'); } }
      catch (_) { push('\n(a frame from ' + clean(el.src).slice(0, 120) + ' that cannot be read)\n'); }
      return;
    }
    const block = BLOCK.has(el.tagName);
    if (block) push('\n');
    if (/^H[1-6]$/.test(el.tagName)) push('#'.repeat(+el.tagName[1]) + ' ');
    if (el.tagName === 'LI') push('- ');
    if (el.shadowRoot) for (const child of el.shadowRoot.childNodes) walk(child);
    for (const child of el.childNodes) walk(child);
    if (block) push('\n');
  };
  if (document.body) walk(document.body);
  const text = out.join('').replace(/[ \t]+\n/g, '\n').replace(/\n[ \t]+/g, '\n').replace(/\n{3,}/g, '\n\n').replace(/[ \t]{2,}/g, ' ').trim();
  return { url: location.href, title: document.title, text };
})()"#;

/// Scroll a control into view and say where its middle is.
const LOCATE_JS: &str = r#"(n) => {
  const refs = globalThis.__pekoRefs;
  const el = refs && refs[n - 1];
  if (!el || !el.isConnected) return { error: 'stale' };
  el.scrollIntoView({ block: 'center', inline: 'center' });
  const r = el.getBoundingClientRect();
  if (r.width === 0 && r.height === 0) return { error: 'hidden' };
  let x = r.left + r.width / 2, y = r.top + r.height / 2;
  let win = el.ownerDocument.defaultView;
  while (win && win !== win.top && win.frameElement) {
    const fr = win.frameElement.getBoundingClientRect();
    x += fr.left; y += fr.top;
    win = win.parent;
  }
  return { x, y, tag: el.tagName.toLowerCase() };
}"#;

/// Focus a control and select what it holds, so typed text replaces it.
const FOCUS_JS: &str = r#"(n) => {
  const el = globalThis.__pekoRefs && globalThis.__pekoRefs[n - 1];
  if (!el) return false;
  el.focus();
  if (typeof el.select === 'function') { try { el.select(); } catch (_) {} }
  else if (el.isContentEditable) {
    const range = el.ownerDocument.createRange();
    range.selectNodeContents(el);
    const selection = el.ownerDocument.defaultView.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
  }
  return true;
}"#;

/// Choose the option of a list whose text or value is `wanted`.
const SELECT_JS: &str = r#"(n, wanted) => {
  const el = globalThis.__pekoRefs && globalThis.__pekoRefs[n - 1];
  if (!el) return { error: 'stale' };
  const norm = s => (s || '').replace(/\s+/g, ' ').trim().toLowerCase();
  const option = Array.from(el.options).find(o => norm(o.text) === norm(wanted) || o.value === wanted);
  if (!option) return { error: 'the list has no option "' + wanted + '"' };
  el.value = option.value;
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
  return { ok: true };
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_found_whatever_their_case() {
        assert_eq!(key_definition("enter"), Some(("Enter", 13, Some("\r"))));
        assert_eq!(key_definition("PageDown"), Some(("PageDown", 34, None)));
        assert!(key_definition("F13").is_none());
    }

    #[test]
    fn a_phone_sized_view_gets_sharper_pictures() {
        assert_eq!(pixel_ratio(390), 2);
        assert_eq!(pixel_ratio(1280), 1);
    }
}
