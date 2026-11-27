//! Messages from visitors, counted per peko (ADR-130).
//!
//! A visitor is anyone but the peko's owner (ADR-113 D1): another person,
//! or another peko. Each message a visitor posts in one of the peko's
//! direct conversations counts once, in the UTC day and ISO week of the
//! post's own time, so a post read again later (a mirror catching up)
//! lands in its own day, not today. Group posts are the group's, and are
//! not counted.
//!
//! The tally is one small file in the peko's own state,
//! `<local>/visitors.json`, holding this day's and this week's counts.
//! The instance heartbeat reports it to PekoHub, where the owner's
//! dashboard shows it; nothing else about the messages leaves the
//! computer.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};

use crate::principal::Principal;

/// One lock for every tally: each change is a small read, edit and write.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// This day's and this week's visitor messages to one peko.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisitorTally {
    /// The UTC day `daily` counts, `YYYY-MM-DD`.
    pub day: String,
    pub daily: u64,
    /// The ISO week `weekly` counts, `YYYY-Www`.
    pub week: String,
    pub weekly: u64,
}

/// `2026-11-27`.
#[must_use]
pub fn day_of(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d").to_string()
}

/// `2026-W48` (ISO 8601: weeks start on Monday, week 1 holds the year's
/// first Thursday).
#[must_use]
pub fn week_of(at: DateTime<Utc>) -> String {
    let week = at.iso_week();
    format!("{}-W{:02}", week.year(), week.week())
}

impl VisitorTally {
    /// The tally as of `now`: counts from an earlier day or week read as
    /// zero.
    #[must_use]
    pub fn as_of(&self, now: DateTime<Utc>) -> Self {
        let (day, week) = (day_of(now), week_of(now));
        Self {
            daily: if self.day == day { self.daily } else { 0 },
            weekly: if self.week == week { self.weekly } else { 0 },
            day,
            week,
        }
    }

    /// Count one message posted at `at`, as of `now`. A message from an
    /// earlier week counts nowhere; one from earlier this week counts
    /// only in the week. Returns whether it counted.
    pub fn record(&mut self, at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        *self = self.as_of(now);
        if week_of(at) != self.week {
            return false;
        }
        self.weekly += 1;
        if day_of(at) == self.day {
            self.daily += 1;
        }
        true
    }
}

/// Where a peko's tally lives: beside its sessions, in its local state.
#[must_use]
pub fn path_for(principal: &Principal) -> PathBuf {
    let sessions = principal.memory.sessions_dir();
    sessions.parent().map_or_else(
        || sessions.join("visitors.json"),
        |local| local.join("visitors.json"),
    )
}

fn load(path: &Path) -> VisitorTally {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The peko's tally as of `now` (zero when nothing was counted yet).
#[must_use]
pub fn read(path: &Path, now: DateTime<Utc>) -> VisitorTally {
    let _guard = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    load(path).as_of(now)
}

/// Count one visitor message posted at `at`.
pub fn record(path: &Path, at: DateTime<Utc>, now: DateTime<Utc>) -> std::io::Result<()> {
    let _guard = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut tally = load(path);
    if !tally.record(at, now) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&tally).unwrap_or_default())?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn iso_weeks_start_on_monday() {
        assert_eq!(week_of(t("2026-11-29T23:00:00Z")), "2026-W48"); // Sunday
        assert_eq!(week_of(t("2026-11-30T00:00:00Z")), "2026-W49"); // Monday
                                                                    // 1 January 2027 is a Friday: still the last week of 2026.
        assert_eq!(week_of(t("2027-01-01T12:00:00Z")), "2026-W53");
    }

    #[test]
    fn counts_today_and_this_week_and_rolls_over() {
        let now = t("2026-11-27T12:00:00Z"); // Friday, W48
        let mut tally = VisitorTally::default();
        assert!(tally.record(t("2026-11-27T11:00:00Z"), now));
        assert!(tally.record(t("2026-11-27T11:30:00Z"), now));
        // Earlier this week: the week only.
        assert!(tally.record(t("2026-11-24T09:00:00Z"), now));
        // Last week: nowhere.
        assert!(!tally.record(t("2026-11-20T09:00:00Z"), now));
        assert_eq!((tally.daily, tally.weekly), (2, 3));

        let saturday = t("2026-11-28T08:00:00Z");
        assert_eq!(tally.as_of(saturday).daily, 0);
        assert_eq!(tally.as_of(saturday).weekly, 3);
        let monday = t("2026-11-30T08:00:00Z");
        assert_eq!(tally.as_of(monday).weekly, 0);
    }

    #[test]
    fn the_file_keeps_the_tally() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local/visitors.json");
        let now = t("2026-11-27T12:00:00Z");
        assert_eq!(read(&path, now).daily, 0);
        record(&path, t("2026-11-27T10:00:00Z"), now).unwrap();
        record(&path, t("2026-11-26T10:00:00Z"), now).unwrap();
        let tally = read(&path, now);
        assert_eq!(
            (tally.day.as_str(), tally.daily, tally.weekly),
            ("2026-11-27", 1, 2)
        );
        // A corrupt file reads as nothing counted, and is replaced on the next count.
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(read(&path, now).weekly, 0);
        record(&path, now, now).unwrap();
        assert_eq!(read(&path, now).weekly, 1);
    }
}
