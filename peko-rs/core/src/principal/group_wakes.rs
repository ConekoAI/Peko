//! How often other pekos may wake a peko in a group, and what waits for
//! it past that (ADR-128 D2).
//!
//! One file per (peko, group), in the peko's own state:
//! `<local>/group_wakes/<channel>.json`. It holds the two limits (absent:
//! the defaults), when other pekos' posts woke or steered the peko in the
//! last day, and the posts that are held because a limit was reached.
//! The runtime that hosts the peko keeps it; nothing here crosses to
//! another runtime (D8).
//!
//! A person's post is never counted here and never held.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use peko_channel::ChannelId;

use crate::principal::Principal;

/// Wakes and steers other pekos' posts may cause in one peko, per group,
/// per rolling hour, unless the owner set another number.
pub const DEFAULT_PER_HOUR: u32 = 60;
/// The same per rolling 24 hours.
pub const DEFAULT_PER_DAY: u32 = 300;

/// Most held posts kept quoted in the file. Older ones are counted
/// (`dropped`) and stay in the group's log.
const MAX_HELD: usize = 200;

/// Most characters of held posts quoted in one compiled message.
const COMPILED_MAX_CHARS: usize = 24_000;

/// Prefix of the owner's request id for a group whose posts are held.
pub const REQUEST_ID_PREFIX: &str = "wakes-";

/// One lock for every book: each change is a small read, edit and write.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Wakes every task that waits for a limit to free up, after any change
/// to any book (a reset, new limits, a new held post).
static CHANGED: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);

/// Resolves when some book changes after this call (the wait starts when
/// the future is made, not when it is first polled).
pub fn changed() -> tokio::sync::futures::Notified<'static> {
    CHANGED.notified()
}

/// A limit the owner set: a number, or none at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Count(u32),
    None,
}

impl Limit {
    /// `"none"`, or a whole number.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("none") {
            return Some(Self::None);
        }
        text.parse().ok().map(Self::Count)
    }

    fn allows(self, spent: usize) -> bool {
        match self {
            Self::None => true,
            Self::Count(n) => spent < n as usize,
        }
    }
}

impl std::fmt::Display for Limit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Count(n) => write!(f, "{n}"),
            Self::None => f.write_str("none"),
        }
    }
}

impl Serialize for Limit {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Count(n) => s.serialize_u32(*n),
            Self::None => s.serialize_str("none"),
        }
    }
}

impl<'de> Deserialize<'de> for Limit {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Count(u32),
            Word(String),
        }
        match Repr::deserialize(d)? {
            Repr::Count(n) => Ok(Self::Count(n)),
            Repr::Word(w) if w.eq_ignore_ascii_case("none") => Ok(Self::None),
            Repr::Word(w) => Err(serde::de::Error::custom(format!(
                "a wake limit is a number or \"none\", not {w:?}"
            ))),
        }
    }
}

/// A post held because a limit was reached, as the peko will read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldPost {
    /// The post's line in the group's log.
    pub event_id: String,
    /// `{author} (id …): {text}`, as a turn reads a post.
    pub line: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct File {
    channel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    per_hour: Option<Limit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    per_day: Option<Limit>,
    /// When other pekos' posts woke or steered the peko, oldest first.
    #[serde(default)]
    spent: Vec<DateTime<Utc>>,
    #[serde(default)]
    held: Vec<HeldPost>,
    /// Held posts no longer quoted here ([`MAX_HELD`]).
    #[serde(default)]
    dropped: usize,
    /// When the first of the posts now held was held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    held_since: Option<DateTime<Utc>>,
    /// The owner said "not now" to the request for these held posts.
    #[serde(default)]
    dismissed: bool,
}

impl File {
    fn hour(&self) -> Limit {
        self.per_hour.unwrap_or(Limit::Count(DEFAULT_PER_HOUR))
    }

    fn day(&self) -> Limit {
        self.per_day.unwrap_or(Limit::Count(DEFAULT_PER_DAY))
    }

    fn prune(&mut self, now: DateTime<Utc>) {
        let day_ago = now - Duration::days(1);
        self.spent.retain(|at| *at > day_ago);
    }

    fn spent_in_hour(&self, now: DateTime<Utc>) -> usize {
        let hour_ago = now - Duration::hours(1);
        self.spent.iter().filter(|at| **at > hour_ago).count()
    }

    fn room(&self, now: DateTime<Utc>) -> Room {
        let in_hour = self.spent_in_hour(now);
        let in_day = self.spent.len();
        let hour = frees(&self.spent, self.hour(), in_hour, Duration::hours(1));
        let day = frees(&self.spent, self.day(), in_day, Duration::days(1));
        match (hour, day) {
            (Room::Never, _) | (_, Room::Never) => Room::Never,
            (Room::Now, other) | (other, Room::Now) => other,
            (Room::At(a), Room::At(b)) => Room::At(a.max(b)),
        }
    }
}

/// When a window that holds the newest `in_window` of `spent` has room
/// under `limit` again.
fn frees(spent: &[DateTime<Utc>], limit: Limit, in_window: usize, window: Duration) -> Room {
    let Limit::Count(n) = limit else {
        return Room::Now;
    };
    let n = n as usize;
    if in_window < n {
        return Room::Now;
    }
    if n == 0 {
        return Room::Never;
    }
    // Room opens when the window holds `n - 1`: once the entry that is
    // `n`-th from the newest has left it.
    spent
        .get(spent.len() - n)
        .map_or(Room::Now, |at| Room::At(*at + window))
}

/// Whether another peko's post may wake the peko now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Room {
    Now,
    /// Not before this moment.
    At(DateTime<Utc>),
    /// A limit of zero: other pekos never wake it here.
    Never,
}

/// What the owner is shown about one peko in one group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeStatus {
    pub channel: String,
    pub per_hour: Limit,
    pub per_day: Limit,
    pub spent_hour: usize,
    pub spent_day: usize,
    /// Posts that wait for a limit to free up.
    pub held: usize,
    pub held_since: Option<DateTime<Utc>>,
    pub room: Room,
    /// Whether the owner is asked about the held posts: a limit holds
    /// them now, and the owner has not said "not now".
    pub asks_owner: bool,
}

impl WakeStatus {
    /// The id of the owner's request for this group's held posts.
    #[must_use]
    pub fn request_id(&self) -> String {
        request_id(&self.channel)
    }

    /// Which limit holds the posts, in words.
    #[must_use]
    pub fn limit_reached(&self) -> String {
        let hour = !self.per_hour.allows(self.spent_hour);
        let day = !self.per_day.allows(self.spent_day);
        match (hour, day) {
            (_, true) => format!("{} wakes a day", self.per_day),
            (true, false) => format!("{} wakes an hour", self.per_hour),
            (false, false) => "its limit of wakes".to_string(),
        }
    }
}

/// The owner's request id for `channel`'s held posts.
#[must_use]
pub fn request_id(channel: &str) -> String {
    format!(
        "{REQUEST_ID_PREFIX}{}",
        peko_channel::fs::channel_dir_name(&ChannelId(channel.to_string()))
    )
}

/// Where a peko's books are kept.
#[must_use]
pub fn dir_for(principal: &Principal) -> PathBuf {
    // `<local>/sessions` is the sessions dir; the books sit beside it.
    let sessions = principal.memory.sessions_dir();
    sessions.parent().map_or_else(
        || sessions.join("group_wakes"),
        |local| local.join("group_wakes"),
    )
}

/// One peko's wakes in one group.
#[derive(Debug, Clone)]
pub struct WakeBook {
    path: PathBuf,
    channel: String,
}

impl WakeBook {
    #[must_use]
    pub fn open(dir: &Path, channel: &ChannelId) -> Self {
        Self {
            path: dir.join(format!(
                "{}.json",
                peko_channel::fs::channel_dir_name(channel)
            )),
            channel: channel.as_str().to_string(),
        }
    }

    fn load(&self) -> File {
        let mut file: File = std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        file.channel.clone_from(&self.channel);
        file
    }

    fn save(&self, file: &File) {
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(file).unwrap_or_default())?;
            std::fs::rename(tmp, &self.path)
        };
        if let Err(e) = write() {
            tracing::warn!(path = %self.path.display(), "group wakes: save failed: {e}");
        }
    }

    fn edit<T>(&self, now: DateTime<Utc>, change: impl FnOnce(&mut File) -> T) -> T {
        let out = {
            let _guard = LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut file = self.load();
            file.prune(now);
            let out = change(&mut file);
            self.save(&file);
            out
        };
        CHANGED.notify_waiters();
        out
    }

    fn read(&self, now: DateTime<Utc>) -> File {
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut file = self.load();
        file.prune(now);
        file
    }

    /// Spend one wake when the limits have room and nothing is already
    /// held (held posts go first, in order). `false`: hold the post.
    pub fn spend(&self, now: DateTime<Utc>) -> bool {
        self.edit(now, |file| {
            if !file.held.is_empty() || file.room(now) != Room::Now {
                return false;
            }
            file.spent.push(now);
            true
        })
    }

    /// Keep `posts` for when a limit frees up.
    pub fn hold(&self, now: DateTime<Utc>, posts: Vec<HeldPost>) {
        self.edit(now, |file| {
            if file.held.is_empty() && file.dropped == 0 {
                file.held_since = Some(now);
                file.dismissed = false;
            }
            for post in posts {
                if !file.held.iter().any(|h| h.event_id == post.event_id) {
                    file.held.push(post);
                }
            }
            let excess = file.held.len().saturating_sub(MAX_HELD);
            file.held.drain(..excess);
            file.dropped += excess;
        });
    }

    /// Take everything held, spending one wake, when the limits have
    /// room. `None`: nothing is held, or there is no room yet.
    pub fn take_held(&self, now: DateTime<Utc>) -> Option<Held> {
        self.edit(now, |file| {
            if file.held.is_empty() || file.room(now) != Room::Now {
                return None;
            }
            file.spent.push(now);
            file.held_since = None;
            file.dismissed = false;
            Some(Held {
                posts: std::mem::take(&mut file.held),
                dropped: std::mem::take(&mut file.dropped),
            })
        })
    }

    #[must_use]
    pub fn status(&self, now: DateTime<Utc>) -> WakeStatus {
        let file = self.read(now);
        WakeStatus {
            channel: self.channel.clone(),
            per_hour: file.hour(),
            per_day: file.day(),
            spent_hour: file.spent_in_hour(now),
            spent_day: file.spent.len(),
            held: file.held.len() + file.dropped,
            held_since: file.held_since,
            room: file.room(now),
            asks_owner: !file.held.is_empty() && !file.dismissed && file.room(now) != Room::Now,
        }
    }

    /// The owner's limits. `None` leaves one as it is.
    pub fn set_limits(&self, now: DateTime<Utc>, per_hour: Option<Limit>, per_day: Option<Limit>) {
        self.edit(now, |file| {
            if per_hour.is_some() {
                file.per_hour = per_hour;
            }
            if per_day.is_some() {
                file.per_day = per_day;
            }
        });
    }

    /// The owner lets the peko continue: what was spent is forgotten, so
    /// what is held is delivered at once.
    pub fn reset(&self, now: DateTime<Utc>) {
        self.edit(now, |file| {
            file.spent.clear();
            file.dismissed = false;
        });
    }

    /// The owner said "not now": the held posts keep waiting for the
    /// limit, and the request is not shown again for them.
    pub fn dismiss(&self, now: DateTime<Utc>) {
        self.edit(now, |file| file.dismissed = true);
    }
}

/// What a peko missed while a limit held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub posts: Vec<HeldPost>,
    /// Older held posts that are not quoted.
    pub dropped: usize,
}

/// Every group of `dir`'s peko that has a book, with its status.
#[must_use]
pub fn all(dir: &Path, now: DateTime<Utc>) -> Vec<(WakeBook, WakeStatus)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(file) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<File>(&bytes).ok())
        else {
            continue;
        };
        let book = WakeBook {
            path,
            channel: file.channel,
        };
        let status = book.status(now);
        out.push((book, status));
    }
    out.sort_by(|a, b| a.1.channel.cmp(&b.1.channel));
    out
}

/// The one message a peko is woken with for several posts other pekos
/// wrote to it (ADR-128 D2, D8): held past a limit, or found together
/// when this runtime caught up. Oldest first; when they are too many to
/// quote, the newest are quoted and the rest are counted.
#[must_use]
pub fn compiled_message(lines: &[String], dropped: usize, held: bool) -> String {
    let mut quoted: Vec<&str> = Vec::new();
    let mut size = 0usize;
    for line in lines.iter().rev() {
        size += line.chars().count();
        if size > COMPILED_MAX_CHARS && !quoted.is_empty() {
            break;
        }
        quoted.push(line);
    }
    quoted.reverse();
    let unquoted = dropped + lines.len() - quoted.len();
    let why = if held {
        "While you were over your limit of wakes here, other pekos wrote these to you."
    } else {
        "Other pekos wrote these to you here before you could be woken."
    };
    let mut out = format!(
        "({why} Answer what still needs an answer. Nothing you write is posted to the \
         group unless you post it with ChannelSend; reply to the post you answer (`parent`: \
         its id), so the answer reaches whoever asked.)\n"
    );
    if unquoted > 0 {
        out.push_str(&format!(
            "({unquoted} earlier ones are not quoted: read the group with ChannelRead for \
             those.)\n"
        ));
    }
    for line in quoted {
        out.push_str(&peko_tools_core::neutralize_runtime_tags(line));
        out.push('\n');
    }
    out
}

/// What a turn another peko's post started is told (ADR-128 D4, D7).
pub const PEKO_STARTED_NOTE: &str = "(Another peko wrote this to you. Nothing you write is \
     posted to the group unless you post it with ChannelSend. To answer, reply to this post \
     (`parent`: its id), so the answer reaches whoever asked: post what the group needs to \
     read, once, or nothing.)";

/// What a peko is asked, once, when a turn another peko started ended
/// with a reply it did not post (ADR-128 D4). Models forget that such a
/// turn posts nothing by itself (2026-11-26 live run: a report to the
/// person who asked was written and never posted).
pub const UNPOSTED_REMINDER: &str = "(Your last reply was not posted: another peko started \
     that turn, so nothing reaches the group unless you post it with ChannelSend. If the group, \
     or the person who asked you for this, needs what you wrote, post it now, once. If nobody \
     needs it, reply NO_REPLY.)";

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> (WakeBook, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let book = WakeBook::open(dir.path(), &ChannelId("group:team".to_string()));
        (book, dir)
    }

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-26T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + Duration::minutes(minutes)
    }

    fn held(id: &str) -> HeldPost {
        HeldPost {
            event_id: id.to_string(),
            line: format!("principal:local:juno (id msg_{id}): @otto post {id}"),
            at: at(0),
        }
    }

    #[test]
    fn the_hourly_limit_holds_and_frees_as_the_hour_rolls() {
        let (book, _dir) = book();
        book.set_limits(at(0), Some(Limit::Count(2)), None);
        assert!(book.spend(at(0)));
        assert!(book.spend(at(10)));
        assert!(!book.spend(at(20)), "the third wake in an hour is refused");
        assert_eq!(book.status(at(20)).room, Room::At(at(60)));
        assert!(book.spend(at(61)), "the first wake has left the hour");
        assert!(!book.spend(at(62)));
        assert_eq!(book.status(at(62)).room, Room::At(at(70)));
    }

    #[test]
    fn the_daily_limit_outlasts_the_hour() {
        let (book, _dir) = book();
        book.set_limits(at(0), Some(Limit::None), Some(Limit::Count(3)));
        for minute in [0, 1, 2] {
            assert!(book.spend(at(minute)));
        }
        assert!(!book.spend(at(120)), "an hour later the day still holds");
        assert_eq!(book.status(at(120)).room, Room::At(at(24 * 60)));
        assert!(book.spend(at(24 * 60 + 1)));
    }

    #[test]
    fn defaults_apply_until_the_owner_sets_limits() {
        let (book, _dir) = book();
        let status = book.status(at(0));
        assert_eq!(status.per_hour, Limit::Count(DEFAULT_PER_HOUR));
        assert_eq!(status.per_day, Limit::Count(DEFAULT_PER_DAY));
        book.set_limits(at(0), Some(Limit::None), Some(Limit::None));
        for _ in 0..500 {
            assert!(book.spend(at(0)));
        }
    }

    #[test]
    fn a_limit_of_zero_never_has_room() {
        let (book, _dir) = book();
        book.set_limits(at(0), Some(Limit::Count(0)), None);
        assert!(!book.spend(at(0)));
        assert_eq!(book.status(at(0)).room, Room::Never);
    }

    #[test]
    fn held_posts_are_taken_together_for_one_wake_when_room_opens() {
        let (book, _dir) = book();
        book.set_limits(at(0), Some(Limit::Count(1)), None);
        assert!(book.spend(at(0)));
        assert!(!book.spend(at(1)));
        book.hold(at(1), vec![held("7"), held("8")]);
        book.hold(at(2), vec![held("8"), held("9")]);
        let status = book.status(at(2));
        assert_eq!(status.held, 3, "a post is held once");
        assert!(status.asks_owner);
        assert_eq!(status.held_since, Some(at(1)));
        assert_eq!(book.take_held(at(30)), None, "no room yet");
        let taken = book.take_held(at(61)).expect("room after the hour");
        assert_eq!(
            taken
                .posts
                .iter()
                .map(|p| p.event_id.as_str())
                .collect::<Vec<_>>(),
            ["7", "8", "9"]
        );
        let status = book.status(at(61));
        assert_eq!((status.held, status.spent_hour), (0, 1), "one wake spent");
        assert!(!status.asks_owner);
    }

    #[test]
    fn a_new_post_waits_behind_what_is_already_held() {
        let (book, _dir) = book();
        book.set_limits(at(0), Some(Limit::Count(1)), None);
        assert!(book.spend(at(0)));
        book.hold(at(1), vec![held("7")]);
        assert!(
            !book.spend(at(90)),
            "room is back, but the held post goes first"
        );
    }

    #[test]
    fn reset_lets_the_peko_continue_and_dismiss_only_hides_the_request() {
        let (book, _dir) = book();
        book.set_limits(at(0), Some(Limit::Count(1)), None);
        assert!(book.spend(at(0)));
        book.hold(at(1), vec![held("7")]);
        book.dismiss(at(2));
        let status = book.status(at(2));
        assert!(!status.asks_owner);
        assert_eq!(status.held, 1, "the post still waits for the limit");
        book.reset(at(3));
        assert!(book.take_held(at(3)).is_some());
    }

    #[test]
    fn the_file_survives_a_reopen_and_lists_with_its_channel() {
        let (book, dir) = book();
        book.set_limits(at(0), Some(Limit::Count(5)), Some(Limit::None));
        assert!(book.spend(at(0)));
        book.hold(at(1), vec![held("7")]);
        let again = WakeBook::open(dir.path(), &ChannelId("group:team".to_string()));
        let status = again.status(at(1));
        assert_eq!(status.per_hour, Limit::Count(5));
        assert_eq!(status.per_day, Limit::None);
        assert_eq!((status.spent_hour, status.held), (1, 1));
        let listed = all(dir.path(), at(1));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1.channel, "group:team");
        assert_eq!(listed[0].1.request_id(), request_id("group:team"));
    }

    #[test]
    fn the_compiled_message_quotes_the_newest_and_counts_the_rest() {
        let lines: Vec<String> = (0..4).map(|i| format!("juno: post {i}")).collect();
        let message = compiled_message(&lines, 0, true);
        assert!(message.contains("over your limit of wakes"));
        assert!(message.contains("juno: post 0\njuno: post 1\njuno: post 2\njuno: post 3\n"));
        assert!(!message.contains("not quoted"));

        let long: Vec<String> = (0..3)
            .map(|i| format!("juno {i}: {}", "x".repeat(COMPILED_MAX_CHARS / 2)))
            .collect();
        let message = compiled_message(&long, 2, false);
        assert!(message.contains("before you could be woken"));
        assert!(message.contains("(4 earlier ones are not quoted"));
        assert!(!message.contains("juno 0:") && !message.contains("juno 1:"));
        assert!(message.contains("juno 2:"));
    }

    #[test]
    fn limits_parse_and_round_trip() {
        assert_eq!(Limit::parse("none"), Some(Limit::None));
        assert_eq!(Limit::parse(" 12 "), Some(Limit::Count(12)));
        assert_eq!(Limit::parse("lots"), None);
        let json = serde_json::to_string(&[Limit::Count(3), Limit::None]).unwrap();
        assert_eq!(json, r#"[3,"none"]"#);
        assert_eq!(
            serde_json::from_str::<Vec<Limit>>(&json).unwrap(),
            [Limit::Count(3), Limit::None]
        );
    }
}
