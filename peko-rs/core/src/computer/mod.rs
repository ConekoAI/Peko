//! A peko uses its owner's computer (ADR-123).
//!
//! The `Computer` tool sees the screen and clicks and types, on macOS and
//! Windows alike. This module is the part both share: the [`Platform`]
//! each system implements (`macos.rs`, `windows.rs`, `unsupported.rs`),
//! and the [`ComputerDesk`] that puts guards around it:
//!
//! - **Frames** (D1): every screenshot is a frame; input names a frame and
//!   a point in that picture's pixels, and the desk maps it back to the
//!   screen, re-reading the window's bounds first.
//! - **The focus guard** (D3): before input reaches a window, its app is
//!   in front and it is the app's front window, or nothing is sent.
//! - **One hand** (D2): the owner's hardware input pauses the run using
//!   the screen; only one run acts at a time; a locked screen or a system
//!   prompt stops everything.
//! - **Keep awake** (D2): the display stays on while a run uses the
//!   computer, and for a minute after.

pub mod keys;
pub mod picture;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported;
#[cfg(windows)]
mod windows;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

pub use keys::{Chord, Key, Mods};
pub use picture::Image;

use crate::principal::manager::PrincipalManager;

/// The tool's wire name.
pub const COMPUTER_TOOL_NAME: &str = "Computer";

/// The title of Peko Desktop's indicator strip. It is left out of the
/// windows the peko lists and the screens it captures, so the peko never
/// sees or clicks it.
pub const INDICATOR_TITLE: &str = "Peko is using this computer";

/// Frames kept per run (D1).
pub const KEPT_FRAMES: usize = 8;

/// Screenshots kept on disk per peko (D1).
pub const KEPT_SCREENSHOTS: usize = 50;

/// A run that has made no input for this long gives up the screen to
/// another (D2).
pub const HAND_IDLE: Duration = Duration::from_secs(30);

/// A run counts as using the computer this long after its last action:
/// the owner's input pauses it, and Peko Desktop shows its indicator.
pub const IN_USE: Duration = Duration::from_secs(30);

/// The display is kept awake this long after the last action (D2).
pub const AWAKE_LINGER: Duration = Duration::from_mins(1);

/// The longest a screenshot waits before it is taken.
pub const MAX_AFTER_MS: u64 = 10_000;

/// How long the focus guard waits for a window it brought forward (D3).
pub const FOCUS_WAIT: Duration = Duration::from_millis(500);

/// Text is sent in pieces of at most this many UTF-16 units, checked for
/// the owner's input and the focus between them (D2, D4).
pub const TEXT_PIECE: usize = 20;

/// Steps a drag moves in.
const DRAG_STEPS: u32 = 12;

/// Frames of runs that have done nothing for this long are dropped.
const FRAMES_IDLE: Duration = Duration::from_mins(30);

/// A stopped run stays refused this long.
const STOPPED_FOR: Duration = Duration::from_mins(10);

// ─── What a platform gives ─────────────────────────────────────────────

/// A rectangle in desktop coordinates: points on macOS, physical pixels
/// on Windows; origin at the top left of the main screen, y down.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    #[must_use]
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    #[must_use]
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }

    fn same_size(&self, other: (f64, f64)) -> bool {
        (self.width - other.0).abs() <= 1.0 && (self.height - other.1).abs() <= 1.0
    }
}

/// A display.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Screen {
    pub id: u64,
    pub bounds: Rect,
    /// Pixels per desktop unit (2 on a Retina display; on Windows the
    /// display's scale setting, 1.5 for 150 %).
    pub scale: f64,
    pub main: bool,
}

/// An app's window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowInfo {
    pub id: u64,
    pub pid: u32,
    pub app: String,
    pub title: String,
    pub bounds: Rect,
    pub minimized: bool,
}

/// What is in front: the frontmost app and its front window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Front {
    pub pid: u32,
    pub app: String,
    pub window: Option<u64>,
}

/// What a screenshot shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Screen(u64),
    Window(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    fn parse(name: Option<&str>) -> Result<Self> {
        match name.unwrap_or("left") {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "middle" => Ok(Self::Middle),
            other => Err(anyhow!("`button` is left, right or middle, not `{other}`")),
        }
    }
}

/// One event the platform posts. Points are in desktop coordinates.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// Move the pointer; `held` when a button is down (a drag).
    Move {
        x: f64,
        y: f64,
        held: Option<Button>,
    },
    /// Press or release a button; `count` is the click count (2 for the
    /// second click of a double click).
    Press {
        x: f64,
        y: f64,
        button: Button,
        down: bool,
        count: u32,
    },
    /// Wheel in lines: `dy` > 0 scrolls down, `dx` > 0 right.
    Scroll { x: f64, y: f64, dx: i32, dy: i32 },
    /// A key with modifiers: modifiers down, key down and up, modifiers up.
    Chord(Chord),
    /// Text inserted as text, whatever the keyboard layout.
    Text(String),
}

/// What the system has granted the engine (macOS); `None` where the
/// system asks for nothing.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Permissions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_recording: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accessibility: Option<bool>,
    /// The app the system holds responsible for the engine, which gets the
    /// grants.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub responsible_app: Option<String>,
}

/// One system's way of seeing the screen and sending input. Every method
/// may block; the desk calls them off the async runtime.
pub trait Platform: Send + Sync {
    /// `macos`, `windows`, or the system it does not support.
    fn system(&self) -> &'static str;
    /// Why `Computer` cannot work here, if it cannot (an unsupported
    /// system, a Windows service with no desktop).
    fn unavailable(&self) -> Option<String>;
    fn permissions(&self) -> Permissions;
    /// Ask the system for the grants (opens its prompts and settings).
    /// Returns what was opened.
    fn request_permissions(&self) -> Result<Vec<String>>;
    fn screens(&self) -> Result<Vec<Screen>>;
    /// Windows on screen, front to back, minimized ones last.
    fn windows(&self) -> Result<Vec<WindowInfo>>;
    fn front(&self) -> Result<Front>;
    /// Bring the window's app and the window to the front.
    fn raise(&self, window: &WindowInfo) -> Result<()>;
    /// The app whose window takes a click at this point, if known.
    fn owner_at(&self, x: f64, y: f64) -> Option<(u32, String)>;
    /// A locked screen, a screen saver or a secure desktop in the way.
    fn obstruction(&self) -> Option<String>;
    /// Whether a password field or system prompt holds the keyboard
    /// (macOS secure input; Windows UAC or the sign-in desktop).
    fn secure_input(&self) -> bool;
    /// Why input cannot reach this window, if it cannot (Windows: a
    /// window of an elevated process).
    fn unreachable(&self, window: &WindowInfo) -> Option<String>;
    /// The pixels of a screen or a window, at the display's resolution.
    fn capture(&self, target: Target) -> Result<Image>;
    fn post(&self, event: &InputEvent) -> Result<()>;
    /// Start calling `owner_input` for hardware input (events the runtime
    /// did not post). Called once.
    fn watch_input(&self, owner_input: Arc<dyn Fn() + Send + Sync>) -> Result<()>;
    /// Keep the display awake until the returned value is dropped.
    fn hold_display_awake(&self) -> Option<Box<dyn Send>>;
}

/// This system's platform.
#[must_use]
pub fn native_platform() -> Arc<dyn Platform> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(macos::MacPlatform::new())
    }
    #[cfg(windows)]
    {
        Arc::new(windows::WindowsPlatform::new())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Arc::new(unsupported::Unsupported)
    }
}

// ─── Who acts, and what stops them ─────────────────────────────────────

/// The run calling the tool.
#[derive(Debug, Clone)]
pub struct Caller {
    pub principal_id: String,
    pub peko: String,
    pub session_id: Option<String>,
    pub run_id: String,
}

/// Why an action did nothing. Each becomes a result the peko is told to
/// act on (end its turn, take a new screenshot, ask its owner).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The owner touched the computer (D2).
    Paused,
    /// Another run is using the screen (D2).
    Busy(String),
    /// The owner pressed Stop.
    Stopped,
    /// The input would not reach the window it names (D3).
    Focus(String),
    /// The frame or window is out of date.
    Stale(String),
    /// A locked screen, a system prompt, an unreachable window.
    Blocked(String),
}

impl Refusal {
    #[must_use]
    pub fn status(&self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::Busy(_) => "busy",
            Self::Stopped => "stopped",
            Self::Focus(_) => "not_in_front",
            Self::Stale(_) => "stale",
            Self::Blocked(_) => "blocked",
        }
    }

    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Paused => "Your owner is using the computer, so you are paused: nothing was \
                             sent. End your turn now; you are told when you may continue."
                .into(),
            Self::Busy(who) => format!(
                "{who} is using the screen; nothing was sent. Try again later or end your turn."
            ),
            Self::Stopped => "Your owner stopped your use of the computer. End your turn.".into(),
            Self::Focus(why) | Self::Stale(why) | Self::Blocked(why) => why.clone(),
        }
    }

    /// The result the tool returns for it.
    #[must_use]
    pub fn result(&self) -> Value {
        json!({
            "success": false,
            "status": self.status(),
            "error": self.message(),
        })
    }
}

/// An action's outcome: done, refused, or failed.
#[derive(Debug)]
pub enum Outcome<T> {
    Done(T),
    Refused(Refusal),
}

impl<T> From<Refusal> for Outcome<T> {
    fn from(r: Refusal) -> Self {
        Self::Refused(r)
    }
}

/// What a screenshot gave.
#[derive(Debug, Clone)]
pub struct Shot {
    pub frame: Frame,
    pub jpeg: Vec<u8>,
    pub window: Option<WindowInfo>,
    pub screen: Option<Screen>,
}

/// A screenshot the peko may act on (D1).
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub id: u64,
    pub target: Target,
    /// The area shown, relative to the target's top left, in desktop
    /// units.
    pub area: Rect,
    /// The picture's size in pixels.
    pub picture: (u32, u32),
    /// The target's size when the frame was taken.
    pub target_size: (f64, f64),
}

impl Frame {
    /// The point at picture pixel `(x, y)`, relative to the target.
    fn local(&self, x: f64, y: f64) -> Result<(f64, f64), Refusal> {
        let (w, h) = (f64::from(self.picture.0), f64::from(self.picture.1));
        if !(0.0..w).contains(&x) || !(0.0..h).contains(&y) {
            return Err(Refusal::Stale(format!(
                "({x}, {y}) is outside frame {} ({}×{} pixels); nothing was sent",
                self.id, self.picture.0, self.picture.1
            )));
        }
        Ok((
            self.area.x + (x + 0.5) * self.area.width / w,
            self.area.y + (y + 0.5) * self.area.height / h,
        ))
    }

    /// The area at picture pixels `(x, y, w, h)`, relative to the target
    /// and clamped to what the frame shows.
    fn local_rect(&self, x: f64, y: f64, w: f64, h: f64) -> Result<Rect, Refusal> {
        let (pw, ph) = (f64::from(self.picture.0), f64::from(self.picture.1));
        if w < 1.0 || h < 1.0 {
            return Err(Refusal::Stale(
                "a region needs a width and height of at least 1".into(),
            ));
        }
        let x0 = x.clamp(0.0, pw - 1.0);
        let y0 = y.clamp(0.0, ph - 1.0);
        let x1 = (x + w).clamp(x0 + 1.0, pw);
        let y1 = (y + h).clamp(y0 + 1.0, ph);
        let sx = self.area.width / pw;
        let sy = self.area.height / ph;
        Ok(Rect::new(
            self.area.x + x0 * sx,
            self.area.y + y0 * sy,
            (x1 - x0) * sx,
            (y1 - y0) * sy,
        ))
    }
}

/// What the screenshot is asked to show.
#[derive(Debug, Clone, Default)]
pub struct ShotRequest {
    pub screen: Option<u64>,
    pub window: Option<u64>,
    /// `(frame, x, y, width, height)`: part of an earlier frame, in its
    /// pixels.
    pub region: Option<(u64, f64, f64, f64, f64)>,
    pub after_ms: u64,
}

/// One input action.
#[derive(Debug, Clone)]
pub enum Input {
    Focus {
        window: u64,
    },
    Click {
        frame: u64,
        x: f64,
        y: f64,
        button: Button,
        count: u32,
        window: u64,
    },
    Drag {
        frame: u64,
        from: (f64, f64),
        to: (f64, f64),
        window: u64,
    },
    Move {
        frame: u64,
        x: f64,
        y: f64,
    },
    Scroll {
        frame: u64,
        x: f64,
        y: f64,
        dx: i32,
        dy: i32,
        window: u64,
    },
    Type {
        text: String,
        /// The secret's name, when `text` is a secret's value.
        secret: Option<String>,
        window: u64,
    },
    Key {
        chord: Chord,
        window: u64,
    },
}

impl Input {
    pub fn parse_button(name: Option<&str>) -> Result<Button> {
        Button::parse(name)
    }

    fn window(&self) -> Option<u64> {
        match self {
            Self::Focus { window }
            | Self::Click { window, .. }
            | Self::Drag { window, .. }
            | Self::Scroll { window, .. }
            | Self::Type { window, .. }
            | Self::Key { window, .. } => Some(*window),
            Self::Move { .. } => None,
        }
    }

    /// What the indicator shows: never the text typed.
    fn summary(&self, app: &str) -> String {
        match self {
            Self::Focus { .. } => format!("bringing {app} to the front"),
            Self::Click { count: 2, .. } => format!("double-clicking in {app}"),
            Self::Click {
                button: Button::Right,
                ..
            } => format!("right-clicking in {app}"),
            Self::Click { .. } => format!("clicking in {app}"),
            Self::Drag { .. } => format!("dragging in {app}"),
            Self::Move { .. } => "moving the pointer".to_string(),
            Self::Scroll { .. } => format!("scrolling in {app}"),
            Self::Type {
                secret: Some(name), ..
            } => format!("typing the secret {name} in {app}"),
            Self::Type { text, .. } => {
                format!("typing {} characters in {app}", text.chars().count())
            }
            Self::Key { chord, .. } => format!("pressing {} in {app}", chord_name(chord)),
        }
    }
}

/// `cmd+shift+a`, as the chord was named.
#[must_use]
pub fn chord_name(chord: &Chord) -> String {
    let mut parts = Vec::new();
    if chord.mods.cmd {
        parts.push("cmd".to_string());
    }
    if chord.mods.ctrl {
        parts.push("ctrl".to_string());
    }
    if chord.mods.alt {
        parts.push("alt".to_string());
    }
    if chord.mods.shift {
        parts.push("shift".to_string());
    }
    if chord.mods.win {
        parts.push("win".to_string());
    }
    parts.push(chord.key.name());
    parts.join("+")
}

/// What an input action did.
#[derive(Debug, Clone)]
pub struct Acted {
    pub window: Option<WindowInfo>,
    pub done: String,
}

// ─── The desk ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Hand {
    caller: Caller,
    last_used: Instant,
    last_input: Option<Instant>,
}

/// A run the owner's input paused.
#[derive(Debug, Clone, Serialize)]
pub struct Pause {
    pub principal_id: String,
    pub peko: String,
    pub session_id: Option<String>,
    pub run_id: String,
    pub since: DateTime<Utc>,
}

/// The latest thing a peko did on the screen, for the indicator.
#[derive(Debug, Clone)]
struct Activity {
    caller: Caller,
    summary: String,
    at: DateTime<Utc>,
    when: Instant,
}

/// A run the owner stopped.
#[derive(Debug, Clone, Serialize)]
pub struct Stopped {
    pub principal_id: String,
    pub peko: String,
    pub session_id: Option<String>,
    pub run_id: String,
}

#[derive(Default)]
struct RunFrames {
    frames: VecDeque<Frame>,
    touched: Option<Instant>,
}

#[derive(Default)]
struct DeskState {
    hand: Option<Hand>,
    paused: Option<Pause>,
    stopped: HashMap<String, Instant>,
    frames: HashMap<String, RunFrames>,
    next_frame: u64,
    activity: Option<Activity>,
    awake: Option<Box<dyn Send>>,
    last_action: Option<Instant>,
    /// Secrets typed, by peko: `(name, value)`, taken out of titles.
    typed: HashMap<String, Vec<(String, String)>>,
    watching: Option<Result<(), String>>,
}

/// The computer, shared by every peko of the runtime.
pub struct ComputerDesk {
    platform: Arc<dyn Platform>,
    config_dir: Option<PathBuf>,
    inbox: Option<Arc<peko_session::InboxRegistry>>,
    manager: Weak<PrincipalManager>,
    audit: Option<Arc<peko_observability::Observability>>,
    runtime: Option<tokio::runtime::Handle>,
    me: OnceLock<Weak<ComputerDesk>>,
    state: Mutex<DeskState>,
}

static DESK: OnceLock<RwLock<Option<Arc<ComputerDesk>>>> = OnceLock::new();

fn desk_slot() -> &'static RwLock<Option<Arc<ComputerDesk>>> {
    DESK.get_or_init(|| RwLock::new(None))
}

/// The installed desk, if any (none in CLI one-shots and most tests).
#[must_use]
pub fn computer_desk() -> Option<Arc<ComputerDesk>> {
    desk_slot()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn set_desk(desk: &Arc<ComputerDesk>) {
    let _ = desk.me.set(Arc::downgrade(desk));
    *desk_slot().write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(desk));
}

/// Install the desk with this system's platform; the daemon calls this
/// once its inboxes and principals exist. It starts the tidy loop that
/// lets the display sleep again.
pub fn install_computer_desk(
    config_dir: PathBuf,
    inbox: Arc<peko_session::InboxRegistry>,
    manager: Weak<PrincipalManager>,
    audit: Arc<peko_observability::Observability>,
) -> Arc<ComputerDesk> {
    let desk = Arc::new(ComputerDesk {
        platform: native_platform(),
        config_dir: Some(config_dir),
        inbox: Some(inbox),
        manager,
        audit: Some(audit),
        runtime: tokio::runtime::Handle::try_current().ok(),
        me: OnceLock::new(),
        state: Mutex::default(),
    });
    set_desk(&desk);
    let weak = Arc::downgrade(&desk);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let Some(desk) = weak.upgrade() else { break };
            desk.tidy();
        }
    });
    desk
}

/// Whether the runtime config (`runtime.toml`) at `path` leaves `Computer` on (`[computer]
/// enabled`, on when absent or unreadable).
#[must_use]
pub fn enabled_in(path: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return true;
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return true;
    };
    match table.get("computer").and_then(|p| p.get("enabled")) {
        Some(toml::Value::Boolean(on)) => *on,
        Some(toml::Value::String(s)) => !s.trim().eq_ignore_ascii_case("false"),
        _ => true,
    }
}

/// Whether pekos are offered `Computer` right now: not turned off in the
/// runtime config (D5). Where the system cannot run it (Linux, a Windows
/// service) it stays listed and says so when called. Without a desk
/// (tests, one-shots) it is listed.
#[must_use]
pub fn listed() -> bool {
    computer_desk().is_none_or(|d| d.enabled())
}

/// The session of the run the owner's hand paused, if any.
#[must_use]
pub fn paused_session() -> Option<String> {
    computer_desk()
        .and_then(|d| d.paused())
        .and_then(|p| p.session_id)
}

/// The owner wrote in `session_id`: a run paused there continues with the
/// turn that message starts (D2).
pub fn owner_spoke(session_id: &str) {
    if let Some(desk) = computer_desk() {
        desk.owner_spoke(session_id);
    }
}

impl ComputerDesk {
    /// A desk over `platform` with nothing behind it (tests).
    #[must_use]
    pub fn with_platform(platform: Arc<dyn Platform>) -> Arc<Self> {
        let desk = Arc::new(Self {
            platform,
            config_dir: None,
            inbox: None,
            manager: Weak::new(),
            audit: None,
            runtime: tokio::runtime::Handle::try_current().ok(),
            me: OnceLock::new(),
            state: Mutex::default(),
        });
        let _ = desk.me.set(Arc::downgrade(&desk));
        desk
    }

    fn state(&self) -> std::sync::MutexGuard<'_, DeskState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[must_use]
    pub fn platform(&self) -> &Arc<dyn Platform> {
        &self.platform
    }

    /// `[computer] enabled` in the runtime config, read each time: off when
    /// `runtime.toml` or the `config.toml` Peko Desktop writes its
    /// settings to says `false`.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.config_dir.as_deref().is_none_or(|dir| {
            enabled_in(&dir.join(crate::common::paths::RUNTIME_CONFIG_FILE))
                && enabled_in(&dir.join("config.toml"))
        })
    }

    /// Why the tool does nothing here, if it does nothing.
    #[must_use]
    pub fn unavailable(&self) -> Option<String> {
        if !self.enabled() {
            return Some(
                "Computer is turned off on this computer (`[computer] enabled = false`).".into(),
            );
        }
        self.platform.unavailable()
    }

    /// A loaded principal by id.
    pub async fn principal(&self, principal_id: &str) -> Option<Arc<crate::principal::Principal>> {
        let manager = self.manager.upgrade()?;
        crate::daemon::cron_engine::resolve_principal(
            &manager,
            &peko_subject::PrincipalId(principal_id.to_string()),
        )
        .await
    }

    // ─── The hand ──────────────────────────────────────────────────

    /// Take the screen for `caller` (or look at it, when not `input`).
    fn claim(&self, caller: &Caller, input: bool) -> Result<(), Refusal> {
        self.start_watch();
        let mut st = self.state();
        if st.stopped.contains_key(&caller.run_id) {
            return Err(Refusal::Stopped);
        }
        if input && st.paused.is_some() {
            return Err(Refusal::Paused);
        }
        let now = Instant::now();
        let mine = match st.hand.as_ref() {
            Some(h) if h.caller.run_id == caller.run_id => true,
            Some(h)
                if h.last_input.is_some_and(|t| t.elapsed() < HAND_IDLE)
                    || (!input && h.last_used.elapsed() < IN_USE) =>
            {
                if input {
                    return Err(Refusal::Busy(
                        if h.caller.principal_id == caller.principal_id {
                            "another of your runs".to_string()
                        } else {
                            h.caller.peko.clone()
                        },
                    ));
                }
                false
            }
            _ => {
                st.hand = Some(Hand {
                    caller: caller.clone(),
                    last_used: now,
                    last_input: None,
                });
                true
            }
        };
        // The hand counts as busy from the first event actually sent
        // (`send`), not from a call that is refused before it sends any.
        if mine {
            if let Some(h) = st.hand.as_mut() {
                h.last_used = now;
            }
        }
        st.last_action = Some(now);
        if st.awake.is_none() {
            st.awake = self.platform.hold_display_awake();
        }
        Ok(())
    }

    /// Whether input may still be sent for `run_id` (checked between the
    /// events of one action).
    fn may_send(&self, run_id: &str) -> Result<(), Refusal> {
        let st = self.state();
        if st.stopped.contains_key(run_id) {
            return Err(Refusal::Stopped);
        }
        if st.paused.is_some() {
            return Err(Refusal::Paused);
        }
        Ok(())
    }

    fn send(&self, run_id: &str, event: &InputEvent) -> Result<Outcome<()>> {
        if let Err(r) = self.may_send(run_id) {
            return Ok(Outcome::Refused(r));
        }
        self.platform.post(event)?;
        if let Some(h) = self.state().hand.as_mut() {
            h.last_input = Some(Instant::now());
        }
        Ok(Outcome::Done(()))
    }

    fn note(&self, caller: &Caller, summary: String) {
        self.state().activity = Some(Activity {
            caller: caller.clone(),
            summary,
            at: Utc::now(),
            when: Instant::now(),
        });
    }

    fn start_watch(&self) {
        {
            let mut st = self.state();
            if st.watching.is_some() {
                return;
            }
            st.watching = Some(Ok(()));
        }
        let weak = self.me.get().cloned().unwrap_or_default();
        let result = self.platform.watch_input(Arc::new(move || {
            if let Some(desk) = weak.upgrade() {
                desk.owner_input();
            }
        }));
        if let Err(e) = result {
            tracing::warn!("computer: cannot watch for the owner's input: {e:#}");
            self.state().watching = Some(Err(format!("{e:#}")));
        }
    }

    /// Hardware input arrived: pause the run using the screen (D2).
    pub fn owner_input(&self) {
        let paused = {
            let mut st = self.state();
            if st.paused.is_some() {
                return;
            }
            let Some(hand) = st.hand.as_ref() else {
                return;
            };
            if hand.last_used.elapsed() >= IN_USE {
                return;
            }
            let pause = Pause {
                principal_id: hand.caller.principal_id.clone(),
                peko: hand.caller.peko.clone(),
                session_id: hand.caller.session_id.clone(),
                run_id: hand.caller.run_id.clone(),
                since: Utc::now(),
            };
            st.paused = Some(pause.clone());
            pause
        };
        tracing::info!(peko = %paused.peko, "computer: the owner's input paused the peko");
        self.audit(
            "computer.owner_input",
            &paused.principal_id,
            json!({"change": "paused", "session_id": paused.session_id}),
        );
    }

    /// The run paused by the owner's input, if any.
    #[must_use]
    pub fn paused(&self) -> Option<Pause> {
        self.state().paused.clone()
    }

    /// **Let it continue**: lift the pause and wake the paused session.
    pub async fn resume(&self) -> Option<Pause> {
        let pause = self.state().paused.take()?;
        if let Some(h) = self.state().hand.as_mut() {
            h.last_used = Instant::now();
        }
        self.audit(
            "computer.owner_input",
            &pause.principal_id,
            json!({"change": "continued", "session_id": pause.session_id}),
        );
        if let Some(session) = pause.session_id.clone() {
            self.wake(&pause.principal_id, &session).await;
        }
        Some(pause)
    }

    /// The owner wrote in `session_id`: lift a pause there; the turn the
    /// message starts goes on (no wake).
    pub fn owner_spoke(&self, session_id: &str) {
        let lifted = {
            let mut st = self.state();
            if st
                .paused
                .as_ref()
                .is_some_and(|p| p.session_id.as_deref() == Some(session_id))
            {
                st.paused.take()
            } else {
                None
            }
        };
        if let Some(pause) = lifted {
            self.audit(
                "computer.owner_input",
                &pause.principal_id,
                json!({"change": "continued_by_message", "session_id": pause.session_id}),
            );
        }
    }

    /// **Stop**: the run using the screen (or paused) is refused from now
    /// on; the caller cancels the run itself.
    pub fn stop(&self) -> Option<Stopped> {
        let stopped = {
            let mut st = self.state();
            let paused = st.paused.take();
            let hand = st.hand.take();
            let who = match (paused, hand) {
                (Some(p), _) => Stopped {
                    principal_id: p.principal_id,
                    peko: p.peko,
                    session_id: p.session_id,
                    run_id: p.run_id,
                },
                (None, Some(h)) => Stopped {
                    principal_id: h.caller.principal_id,
                    peko: h.caller.peko,
                    session_id: h.caller.session_id,
                    run_id: h.caller.run_id,
                },
                (None, None) => return None,
            };
            st.stopped.insert(who.run_id.clone(), Instant::now());
            st.activity = None;
            who
        };
        self.audit(
            "computer.stop",
            &stopped.principal_id,
            json!({"session_id": stopped.session_id}),
        );
        Some(stopped)
    }

    fn audit(&self, event: &'static str, principal_id: &str, detail: Value) {
        let (Some(audit), Some(rt)) = (self.audit.clone(), self.runtime.clone()) else {
            return;
        };
        let principal_id = principal_id.to_string();
        rt.spawn(async move {
            let _ = audit
                .audit_security(event, Some(&principal_id), detail)
                .await;
        });
    }

    async fn wake(&self, principal_id: &str, session: &str) {
        let Some(inbox) = self.inbox.as_ref() else {
            return;
        };
        let task_id = format!("computer:{}", uuid::Uuid::new_v4().simple());
        let result = json!({
            "status": "continue",
            "note": "Your owner let you continue using the computer. Take a new screenshot \
                     before you act: things may have moved.",
        });
        let event = peko_session::CompletionEvent {
            task_id: task_id.clone(),
            tool_name: COMPUTER_TOOL_NAME.to_string(),
            result: result.clone(),
            status: peko_tools_core::AsyncTaskStatus::Completed {
                result: peko_tools_core::ToolResult::success(result),
            },
            completed_at: Utc::now(),
            parent_session_key: session.to_string(),
        };
        let inbox = inbox
            .get_or_create(&peko_session::inbox_key(principal_id, session))
            .await;
        inbox
            .push(peko_session::InboxItem::Completion(event).into())
            .await;
        crate::async_exec::executor::wake::notify_completion_wake(
            crate::async_exec::executor::wake::CompletionWakeNotice {
                session_key: session.to_string(),
                task_id,
                tool_name: COMPUTER_TOOL_NAME.to_string(),
                principal_id: Some(principal_id.to_string()),
                via_steering: false,
            },
        );
    }

    /// Let the display sleep again, drop old frames and stops.
    pub fn tidy(&self) {
        let mut st = self.state();
        if st.last_action.is_none_or(|t| t.elapsed() > AWAKE_LINGER) {
            st.awake = None;
        }
        st.frames
            .retain(|_, f| f.touched.is_some_and(|t| t.elapsed() < FRAMES_IDLE));
        st.stopped.retain(|_, at| at.elapsed() < STOPPED_FOR);
    }

    /// Whether the display is being kept awake (tests).
    #[must_use]
    pub fn keeping_awake(&self) -> bool {
        self.state().awake.is_some()
    }

    // ─── Secrets typed (D6) ────────────────────────────────────────

    fn remember_typed(&self, principal_id: &str, name: &str, value: &str) {
        // A value too short to look for in titles is still named in the
        // note on later pictures.
        let mut st = self.state();
        let typed = st.typed.entry(principal_id.to_string()).or_default();
        if !typed.iter().any(|(n, v)| n == name && v == value) {
            typed.push((name.to_string(), value.to_string()));
        }
    }

    /// `text` with every secret the peko typed replaced by its name.
    #[must_use]
    pub fn scrub(&self, principal_id: &str, text: &str) -> String {
        let st = self.state();
        let mut out = text.to_string();
        for (name, value) in st.typed.get(principal_id).into_iter().flatten() {
            if value.len() >= crate::principal::secret_requests::MIN_REDACTED_LEN
                && out.contains(value.as_str())
            {
                out = out.replace(value.as_str(), &format!("[secret:{name}]"));
            }
        }
        out
    }

    /// Names of the secrets the peko typed on screen.
    #[must_use]
    pub fn secrets_typed(&self, principal_id: &str) -> Vec<String> {
        self.state()
            .typed
            .get(principal_id)
            .map(|t| t.iter().map(|(n, _)| n.clone()).collect())
            .unwrap_or_default()
    }

    // ─── Looking ───────────────────────────────────────────────────

    /// The displays.
    pub fn screens(&self, caller: &Caller) -> Result<Outcome<Vec<Screen>>> {
        if let Err(r) = self.claim(caller, false) {
            return Ok(Outcome::Refused(r));
        }
        self.note(caller, "looking at the screens".into());
        Ok(Outcome::Done(self.platform.screens()?))
    }

    /// Windows on screen, front to back; `app` filters by name or process
    /// id. The indicator strip is left out.
    pub fn windows(&self, caller: &Caller, app: Option<&str>) -> Result<Outcome<Vec<WindowInfo>>> {
        if let Err(r) = self.claim(caller, false) {
            return Ok(Outcome::Refused(r));
        }
        self.note(caller, "listing windows".into());
        let wanted = app.map(str::trim).filter(|a| !a.is_empty());
        let pid = wanted.and_then(|a| a.parse::<u32>().ok());
        let list = self
            .listable()?
            .into_iter()
            .filter(|w| match (wanted, pid) {
                (None, _) => true,
                (Some(_), Some(pid)) => w.pid == pid,
                (Some(name), None) => w.app.to_lowercase().contains(&name.to_lowercase()),
            })
            .map(|mut w| {
                w.title = self.scrub(&caller.principal_id, &w.title);
                w
            })
            .collect();
        Ok(Outcome::Done(list))
    }

    fn listable(&self) -> Result<Vec<WindowInfo>> {
        Ok(self
            .platform
            .windows()?
            .into_iter()
            .filter(|w| w.title != INDICATOR_TITLE && w.bounds.width > 0.0 && w.bounds.height > 0.0)
            .collect())
    }

    fn window(&self, id: u64) -> Result<Option<WindowInfo>> {
        Ok(self.listable()?.into_iter().find(|w| w.id == id))
    }

    fn frame(&self, run_id: &str, id: u64) -> Result<Frame, Refusal> {
        let st = self.state();
        st.frames
            .get(run_id)
            .and_then(|f| f.frames.iter().find(|f| f.id == id).cloned())
            .ok_or_else(|| {
                Refusal::Stale(format!(
                    "there is no frame {id} in this run (frames are kept for your last \
                     {KEPT_FRAMES} screenshots); take a new screenshot"
                ))
            })
    }

    /// A picture of a screen or a window (D1).
    pub fn screenshot(&self, caller: &Caller, req: &ShotRequest) -> Result<Outcome<Shot>> {
        if let Err(r) = self.claim(caller, false) {
            return Ok(Outcome::Refused(r));
        }
        if let Some(why) = self.platform.obstruction() {
            return Ok(Refusal::Blocked(why).into());
        }
        if req.after_ms > 0 {
            std::thread::sleep(Duration::from_millis(req.after_ms.min(MAX_AFTER_MS)));
        }
        // What to show, and which part of it.
        let (target, area) = match req.region {
            Some((frame, left, top, width, height)) => {
                let frame = match self.frame(&caller.run_id, frame) {
                    Ok(f) => f,
                    Err(r) => return Ok(r.into()),
                };
                match frame.local_rect(left, top, width, height) {
                    Ok(area) => (frame.target, Some(area)),
                    Err(r) => return Ok(r.into()),
                }
            }
            None => match (req.window, req.screen) {
                (Some(w), _) => (Target::Window(w), None),
                (None, Some(s)) => (Target::Screen(s), None),
                (None, None) => {
                    let screens = self.platform.screens()?;
                    let main = screens
                        .iter()
                        .find(|s| s.main)
                        .or(screens.first())
                        .ok_or_else(|| anyhow!("no screen found"))?;
                    (Target::Screen(main.id), None)
                }
            },
        };
        let (bounds, window, screen) = match target {
            Target::Window(id) => {
                let Some(w) = self.window(id)? else {
                    return Ok(Refusal::Stale(format!(
                        "window {id} is gone; list the windows again"
                    ))
                    .into());
                };
                if w.minimized {
                    return Ok(Refusal::Stale(format!(
                        "window {id} is minimized; bring it forward with `focus` first"
                    ))
                    .into());
                }
                (w.bounds, Some(w), None)
            }
            Target::Screen(id) => {
                let Some(s) = self.platform.screens()?.into_iter().find(|s| s.id == id) else {
                    return Ok(Refusal::Stale(format!("there is no screen {id}")).into());
                };
                (s.bounds, None, Some(s))
            }
        };
        let area = area.unwrap_or(Rect::new(0.0, 0.0, bounds.width, bounds.height));
        let image = self.platform.capture(target)?;
        let ppu_x = f64::from(image.width) / bounds.width.max(1.0);
        let ppu_y = f64::from(image.height) / bounds.height.max(1.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let crop = image.crop(
            (area.x * ppu_x).floor().max(0.0) as u32,
            (area.y * ppu_y).floor().max(0.0) as u32,
            (area.width * ppu_x).ceil().max(1.0) as u32,
            (area.height * ppu_y).ceil().max(1.0) as u32,
        );
        let (pw, ph) = picture::fitted(crop.width, crop.height);
        let jpeg = picture::encode(&picture::resize(&crop, pw, ph))?;
        let frame = {
            let mut st = self.state();
            st.next_frame += 1;
            let frame = Frame {
                id: st.next_frame,
                target,
                area,
                picture: (pw, ph),
                target_size: (bounds.width, bounds.height),
            };
            let run = st.frames.entry(caller.run_id.clone()).or_default();
            run.frames.push_back(frame.clone());
            while run.frames.len() > KEPT_FRAMES {
                run.frames.pop_front();
            }
            run.touched = Some(Instant::now());
            frame
        };
        let of = window
            .as_ref()
            .map_or_else(|| "the screen".to_string(), |w| w.app.clone());
        self.note(caller, format!("looking at {of}"));
        Ok(Outcome::Done(Shot {
            frame,
            jpeg,
            window: window.map(|mut w| {
                w.title = self.scrub(&caller.principal_id, &w.title);
                w
            }),
            screen,
        }))
    }

    /// Where picture pixel `(x, y)` of `frame` is on the desktop now.
    fn locate(&self, run_id: &str, frame: u64, x: f64, y: f64) -> Result<Outcome<(f64, f64)>> {
        let frame = match self.frame(run_id, frame) {
            Ok(f) => f,
            Err(r) => return Ok(r.into()),
        };
        let (lx, ly) = match frame.local(x, y) {
            Ok(p) => p,
            Err(r) => return Ok(r.into()),
        };
        let origin = match frame.target {
            Target::Window(id) => match self.window(id)? {
                Some(w) if !w.minimized && w.bounds.same_size(frame.target_size) => w.bounds,
                Some(w) if !w.minimized => {
                    return Ok(Refusal::Stale(format!(
                        "window {id} changed size since frame {}; take a new screenshot",
                        frame.id
                    ))
                    .into())
                }
                _ => {
                    return Ok(Refusal::Stale(format!(
                        "window {id} of frame {} is gone or minimized; take a new screenshot",
                        frame.id
                    ))
                    .into())
                }
            },
            Target::Screen(id) => match self.platform.screens()?.into_iter().find(|s| s.id == id) {
                Some(s) if s.bounds.same_size(frame.target_size) => s.bounds,
                _ => {
                    return Ok(Refusal::Stale(format!(
                        "screen {id} changed since frame {}; take a new screenshot",
                        frame.id
                    ))
                    .into())
                }
            },
        };
        Ok(Outcome::Done((origin.x + lx, origin.y + ly)))
    }

    /// Whether the named window is the front window of the frontmost app.
    fn in_front(&self, window: &WindowInfo) -> Result<bool> {
        let front = self.platform.front()?;
        Ok(front.pid == window.pid && front.window == Some(window.id))
    }

    /// The focus guard (D3): the window is in front, or it is brought
    /// forward once, or nothing is sent.
    fn guard(&self, window_id: u64) -> Result<Outcome<WindowInfo>> {
        if let Some(why) = self.platform.obstruction() {
            return Ok(Refusal::Blocked(why).into());
        }
        let Some(window) = self.window(window_id)? else {
            return Ok(Refusal::Stale(format!(
                "window {window_id} is gone; list the windows again. Nothing was sent."
            ))
            .into());
        };
        if let Some(why) = self.platform.unreachable(&window) {
            return Ok(Refusal::Blocked(why).into());
        }
        if !window.minimized && self.in_front(&window)? {
            return Ok(Outcome::Done(window));
        }
        if let Err(e) = self.platform.raise(&window) {
            tracing::debug!("computer: raising window {window_id}: {e:#}");
        }
        let deadline = Instant::now() + FOCUS_WAIT;
        loop {
            std::thread::sleep(Duration::from_millis(50));
            if let Some(now) = self.window(window_id)? {
                if !now.minimized && self.in_front(&now)? {
                    // A window that just came forward takes the next click
                    // as a click, not as activation.
                    std::thread::sleep(Duration::from_millis(100));
                    return Ok(Outcome::Done(now));
                }
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        let front = self.platform.front()?;
        let in_front = match front.window.and_then(|id| self.window(id).ok().flatten()) {
            Some(w) if w.pid == window.pid => format!(
                "another window of {} (window {}, \"{}\") is in front",
                w.app, w.id, w.title
            ),
            _ => format!("{} is in front", front.app),
        };
        Ok(Refusal::Focus(format!(
            "{in_front}, not window {window_id} ({}); nothing was sent. It could not be brought \
             forward: take a screenshot to see what is in the way.",
            window.app
        ))
        .into())
    }

    /// The point is over the window (or a menu of its app), not another
    /// app's window.
    fn over(&self, window: &WindowInfo, x: f64, y: f64) -> Option<Refusal> {
        match self.platform.owner_at(x, y) {
            Some((pid, _)) if pid == window.pid => None,
            Some((_, app)) => Some(Refusal::Focus(format!(
                "{app} covers that point, not {}; nothing was sent. Take a new screenshot.",
                window.app
            ))),
            None if window.bounds.contains(x, y) => None,
            None => Some(Refusal::Focus(format!(
                "that point is outside window {} ({}); nothing was sent",
                window.id, window.app
            ))),
        }
    }

    /// Carry out one input action (D1–D4).
    pub fn act(&self, caller: &Caller, input: &Input) -> Result<Outcome<Acted>> {
        if let Err(r) = self.claim(caller, true) {
            return Ok(r.into());
        }
        let run = caller.run_id.as_str();
        // The pointer only hovers: no window, no guard.
        if let Input::Move { frame, x, y } = input {
            let (gx, gy) = match self.locate(run, *frame, *x, *y)? {
                Outcome::Done(p) => p,
                Outcome::Refused(r) => return Ok(r.into()),
            };
            self.note(caller, input.summary(""));
            if let Outcome::Refused(r) = self.send(
                run,
                &InputEvent::Move {
                    x: gx,
                    y: gy,
                    held: None,
                },
            )? {
                return Ok(r.into());
            }
            return Ok(Outcome::Done(Acted {
                window: None,
                done: format!("moved the pointer to ({x}, {y}) in frame {frame}"),
            }));
        }
        let window_id = input.window().unwrap_or_default();
        // Points first: a stale frame sends nothing, not even a focus.
        let points: Vec<(f64, f64)> = match input {
            Input::Click { frame, x, y, .. } | Input::Scroll { frame, x, y, .. } => {
                if let Some(r) = self.frame_for(run, *frame, window_id)? {
                    return Ok(r.into());
                }
                match self.locate(run, *frame, *x, *y)? {
                    Outcome::Done(p) => vec![p],
                    Outcome::Refused(r) => return Ok(r.into()),
                }
            }
            Input::Drag {
                frame, from, to, ..
            } => {
                if let Some(r) = self.frame_for(run, *frame, window_id)? {
                    return Ok(r.into());
                }
                let mut points = Vec::new();
                for (x, y) in [from, to] {
                    match self.locate(run, *frame, *x, *y)? {
                        Outcome::Done(p) => points.push(p),
                        Outcome::Refused(r) => return Ok(r.into()),
                    }
                }
                points
            }
            _ => Vec::new(),
        };
        if let Input::Type {
            secret: Some(_), ..
        } = input
        {
            if self.platform.secure_input() {
                return Ok(Refusal::Blocked(
                    "a password field of the system or a system prompt holds the keyboard; a \
                     secret is never typed into one. Nothing was sent."
                        .into(),
                )
                .into());
            }
        }
        let window = match self.guard(window_id)? {
            Outcome::Done(w) => w,
            Outcome::Refused(r) => return Ok(r.into()),
        };
        self.note(caller, input.summary(&window.app));
        for (x, y) in &points {
            if let Some(r) = self.over(&window, *x, *y) {
                return Ok(r.into());
            }
        }
        let done = match input {
            Input::Focus { .. } => format!("window {} of {} is in front", window.id, window.app),
            Input::Click {
                x,
                y,
                frame,
                button,
                count,
                ..
            } => {
                let (gx, gy) = points[0];
                let mut events = vec![InputEvent::Move {
                    x: gx,
                    y: gy,
                    held: None,
                }];
                for n in 1..=(*count).clamp(1, 3) {
                    for down in [true, false] {
                        events.push(InputEvent::Press {
                            x: gx,
                            y: gy,
                            button: *button,
                            down,
                            count: n,
                        });
                    }
                }
                if let Some(r) = self.send_all(run, &events)? {
                    return Ok(r.into());
                }
                format!("clicked at ({x}, {y}) in frame {frame}")
            }
            Input::Drag {
                from, to, frame, ..
            } => {
                let ((x0, y0), (x1, y1)) = (points[0], points[1]);
                let mut events = vec![
                    InputEvent::Move {
                        x: x0,
                        y: y0,
                        held: None,
                    },
                    InputEvent::Press {
                        x: x0,
                        y: y0,
                        button: Button::Left,
                        down: true,
                        count: 1,
                    },
                ];
                for step in 1..=DRAG_STEPS {
                    let t = f64::from(step) / f64::from(DRAG_STEPS);
                    events.push(InputEvent::Move {
                        x: x0 + (x1 - x0) * t,
                        y: y0 + (y1 - y0) * t,
                        held: Some(Button::Left),
                    });
                }
                events.push(InputEvent::Press {
                    x: x1,
                    y: y1,
                    button: Button::Left,
                    down: false,
                    count: 1,
                });
                if let Some(r) = self.send_all(run, &events)? {
                    // Never leave the button held down.
                    let _ = self.platform.post(&InputEvent::Press {
                        x: x1,
                        y: y1,
                        button: Button::Left,
                        down: false,
                        count: 1,
                    });
                    return Ok(r.into());
                }
                format!(
                    "dragged from ({}, {}) to ({}, {}) in frame {frame}",
                    from.0, from.1, to.0, to.1
                )
            }
            Input::Scroll {
                dx,
                dy,
                x,
                y,
                frame,
                ..
            } => {
                let (gx, gy) = points[0];
                let events = [
                    InputEvent::Move {
                        x: gx,
                        y: gy,
                        held: None,
                    },
                    InputEvent::Scroll {
                        x: gx,
                        y: gy,
                        dx: *dx,
                        dy: *dy,
                    },
                ];
                if let Some(r) = self.send_all(run, &events)? {
                    return Ok(r.into());
                }
                format!("scrolled ({dx}, {dy}) lines at ({x}, {y}) in frame {frame}")
            }
            Input::Key { chord, .. } => {
                if let Some(r) = self.send_all(run, &[InputEvent::Chord(*chord)])? {
                    return Ok(r.into());
                }
                format!("pressed {}", chord_name(chord))
            }
            Input::Type { text, secret, .. } => {
                if let Some(name) = secret {
                    self.remember_typed(&caller.principal_id, name, text);
                }
                let pieces = pieces(text);
                for (i, piece) in pieces.iter().enumerate() {
                    // The focus and the owner are checked between pieces.
                    if i > 0 {
                        match self.guard(window_id)? {
                            Outcome::Done(_) => {}
                            Outcome::Refused(r) => {
                                return Ok(partial(r, i, pieces.len()).into());
                            }
                        }
                    }
                    if let Outcome::Refused(r) = self.send(run, piece)? {
                        return Ok(partial(r, i, pieces.len()).into());
                    }
                    std::thread::sleep(Duration::from_millis(8));
                }
                match secret {
                    Some(name) => format!("typed the secret {name}"),
                    None => format!("typed {} characters", text.chars().count()),
                }
            }
            Input::Move { .. } => unreachable!("handled above"),
        };
        let window = Some(WindowInfo {
            title: self.scrub(&caller.principal_id, &window.title),
            ..window
        });
        Ok(Outcome::Done(Acted { window, done }))
    }

    /// A frame of a window may only steer input to that window.
    fn frame_for(&self, run: &str, frame: u64, window: u64) -> Result<Option<Refusal>> {
        let frame = match self.frame(run, frame) {
            Ok(f) => f,
            Err(r) => return Ok(Some(r)),
        };
        Ok(match frame.target {
            Target::Window(w) if w != window => Some(Refusal::Stale(format!(
                "frame {} shows window {w}, not window {window}; nothing was sent",
                frame.id
            ))),
            _ => None,
        })
    }

    fn send_all(&self, run: &str, events: &[InputEvent]) -> Result<Option<Refusal>> {
        for event in events {
            if let Outcome::Refused(r) = self.send(run, event)? {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    // ─── What Peko Desktop and the CLI ask ─────────────────────────

    /// `computer_state`: who uses the screen, doing what, and whether the
    /// owner's input paused it.
    #[must_use]
    pub fn state_view(&self) -> Value {
        let unavailable = self.unavailable();
        let st = self.state();
        let using = st
            .activity
            .as_ref()
            .filter(|a| a.when.elapsed() < IN_USE || st.paused.is_some())
            .map(|a| {
                json!({
                    "principal_id": a.caller.principal_id,
                    "peko": a.caller.peko,
                    "session_id": a.caller.session_id,
                    "action": a.summary,
                    "at": a.at.to_rfc3339(),
                })
            });
        json!({
            "system": self.platform.system(),
            "available": unavailable.is_none(),
            "unavailable": unavailable,
            "using": using,
            "paused": st.paused.as_ref().map(|p| json!({
                "principal_id": p.principal_id,
                "peko": p.peko,
                "session_id": p.session_id,
                "since": p.since.to_rfc3339(),
            })),
            "watching_owner_input": st.watching.as_ref().map(|w| w.is_ok()),
            "watch_error": st.watching.as_ref().and_then(|w| w.as_ref().err().cloned()),
        })
    }

    /// `peko computer status`: whether it works here, and the grants.
    #[must_use]
    pub fn status(&self) -> Value {
        let mut view = self.state_view();
        view["enabled"] = json!(self.enabled());
        view["permissions"] = json!(self.platform.permissions());
        view
    }
}

/// A refusal partway through typing says how far it got.
fn partial(r: Refusal, sent: usize, total: usize) -> Refusal {
    if sent == 0 {
        return r;
    }
    let note = format!(" ({sent} of {total} pieces of the text had been typed before this.)");
    match r {
        Refusal::Focus(m) => Refusal::Focus(m + &note),
        Refusal::Blocked(m) => Refusal::Blocked(m + &note),
        Refusal::Stale(m) => Refusal::Stale(m + &note),
        other => other,
    }
}

/// Text as events: pieces of at most [`TEXT_PIECE`] UTF-16 units (a
/// surrogate pair never split), with newlines and tabs as Enter and Tab.
#[must_use]
pub fn pieces(text: &str) -> Vec<InputEvent> {
    let mut out = Vec::new();
    let mut piece = String::new();
    let mut units = 0;
    let flush = |piece: &mut String, units: &mut usize, out: &mut Vec<InputEvent>| {
        if !piece.is_empty() {
            out.push(InputEvent::Text(std::mem::take(piece)));
            *units = 0;
        }
    };
    for c in text.chars() {
        let key = match c {
            '\n' => Some(Key::Enter),
            '\t' => Some(Key::Tab),
            '\r' => continue,
            _ => None,
        };
        if let Some(key) = key {
            flush(&mut piece, &mut units, &mut out);
            out.push(InputEvent::Chord(Chord {
                mods: Mods::default(),
                key,
            }));
            continue;
        }
        if units + c.len_utf16() > TEXT_PIECE {
            flush(&mut piece, &mut units, &mut out);
        }
        piece.push(c);
        units += c.len_utf16();
    }
    flush(&mut piece, &mut units, &mut out);
    out
}
