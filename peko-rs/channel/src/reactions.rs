//! Emoji reactions on a conversation's posts (ADR-126), folded from the
//! conversation's own log.
//!
//! A post may carry a [`ReactOp`]. The reactions on a message are the
//! fold of every such op in the log, as the board is (ADR-101), so they
//! are signed, ordered, mirrored, served as history and followed to a
//! person's other computers exactly as posts are.
//!
//! - **Who reacted** is the post's verified author, compared by immutable
//!   actor key (ADR-081 D1), never anything in the op.
//! - **One actor, one emoji, one message** is on or off: ops apply in
//!   `(at, msg_id)` order, so the latest wins and every copy that holds
//!   the same posts folds the same reactions.
//! - **At most [`MAX_EMOJI_PER_MESSAGE`] different emoji** stand on one
//!   message; an op that would add another is ignored.
//! - An op naming a reaction post is ignored. An op naming a post this
//!   copy does not hold is kept: it shows once the post arrives.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use peko_protocol::channel::{ChannelEvent, ReactOp};
use peko_subject::{ActorKey, ActorKeys, ActorRef};

/// Most different emoji that stand on one message (D1).
pub const MAX_EMOJI_PER_MESSAGE: usize = 20;
/// Longest emoji, in bytes (D1).
pub const MAX_EMOJI_BYTES: usize = 64;

/// One emoji on a message, with who put it there (oldest first).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Reaction {
    pub emoji: String,
    /// The reactors' addresses, as written on their posts.
    pub by: Vec<String>,
}

/// A reaction that stands, with when it was put on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandingReaction {
    /// The post reacted to.
    pub to: String,
    pub emoji: String,
    /// The reactor's address, as written on its post.
    pub by: String,
    /// When the op that put it on was posted (RFC 3339).
    pub at: String,
}

#[derive(Debug, Clone)]
struct Entry {
    emoji: String,
    /// `(key, address, at)`, oldest first.
    by: Vec<(ActorKey, String, String)>,
}

/// The reactions of one conversation, by the `msg_id` they are on.
#[derive(Debug, Clone, Default)]
pub struct Reactions {
    on: HashMap<String, Vec<Entry>>,
}

impl Reactions {
    /// Fold `events` (a conversation's log, in any order).
    pub fn fold<'e>(
        events: impl IntoIterator<Item = &'e ChannelEvent>,
        keys: &dyn ActorKeys,
    ) -> Self {
        let mut reaction_posts: HashSet<&str> = HashSet::new();
        let mut ops: Vec<(Stamp, &str, &ReactOp)> = Vec::new();
        for ev in events {
            if let ChannelEvent::Posted {
                author,
                at,
                msg_id,
                react: Some(op),
                ..
            } = ev
            {
                if let Some(id) = msg_id {
                    reaction_posts.insert(id.as_str());
                }
                ops.push((
                    Stamp::new(at, msg_id.as_deref().unwrap_or("")),
                    author.as_str(),
                    op,
                ));
            }
        }
        ops.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out = Self::default();
        for (stamp, author, op) in ops {
            if reaction_posts.contains(op.to.as_str()) || !valid_emoji(&op.emoji) {
                continue;
            }
            let Some(key) = key_of(keys, author) else {
                continue;
            };
            out.apply(key, author, &stamp.at, op);
        }
        out
    }

    fn apply(&mut self, key: ActorKey, author: &str, at: &str, op: &ReactOp) {
        let entries = self.on.entry(op.to.clone()).or_default();
        let pos = entries.iter().position(|e| e.emoji == op.emoji);
        if op.remove {
            if let Some(i) = pos {
                entries[i].by.retain(|(k, _, _)| *k != key);
                if entries[i].by.is_empty() {
                    entries.remove(i);
                }
            }
            return;
        }
        match pos {
            Some(i) => {
                if !entries[i].by.iter().any(|(k, _, _)| *k == key) {
                    entries[i]
                        .by
                        .push((key, author.to_string(), at.to_string()));
                }
            }
            None if entries.len() >= MAX_EMOJI_PER_MESSAGE => {}
            None => entries.push(Entry {
                emoji: op.emoji.clone(),
                by: vec![(key, author.to_string(), at.to_string())],
            }),
        }
    }

    /// The reactions on post `msg_id`, in the order their emoji first
    /// stood there. Empty when none.
    pub fn on(&self, msg_id: &str) -> Vec<Reaction> {
        self.on
            .get(msg_id)
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| Reaction {
                        emoji: e.emoji.clone(),
                        by: e.by.iter().map(|(_, a, _)| a.clone()).collect(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// How many different emoji stand on `msg_id`.
    pub fn emoji_count(&self, msg_id: &str) -> usize {
        self.on.get(msg_id).map_or(0, Vec::len)
    }

    /// Whether `by` (an address) has `emoji` on `msg_id`.
    pub fn has(&self, keys: &dyn ActorKeys, msg_id: &str, emoji: &str, by: &str) -> bool {
        let Some(key) = key_of(keys, by) else {
            return false;
        };
        self.on.get(msg_id).is_some_and(|entries| {
            entries
                .iter()
                .any(|e| e.emoji == emoji && e.by.iter().any(|(k, _, _)| *k == key))
        })
    }

    /// Every reaction that stands, newest first.
    pub fn standing(&self) -> Vec<StandingReaction> {
        let mut all: Vec<StandingReaction> = self
            .on
            .iter()
            .flat_map(|(to, entries)| {
                entries.iter().flat_map(move |e| {
                    e.by.iter().map(move |(_, by, at)| StandingReaction {
                        to: to.clone(),
                        emoji: e.emoji.clone(),
                        by: by.clone(),
                        at: at.clone(),
                    })
                })
            })
            .collect();
        all.sort_by(|a, b| Stamp::new(&b.at, &b.to).cmp(&Stamp::new(&a.at, &a.to)));
        all
    }

    /// Whether no reaction stands.
    pub fn is_empty(&self) -> bool {
        self.on.values().all(Vec::is_empty)
    }
}

fn key_of(keys: &dyn ActorKeys, address: &str) -> Option<ActorKey> {
    address
        .parse::<ActorRef>()
        .ok()
        .filter(|a| !matches!(a, ActorRef::Public))
        .map(|a| keys.key_of(&a))
        .or_else(|| (!address.is_empty()).then(|| ActorKey(address.to_string())))
}

/// `(at, msg_id)` ordering, by instant when `at` parses.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Stamp {
    when: Option<DateTime<Utc>>,
    at: String,
    id: String,
}

impl Stamp {
    fn new(at: &str, id: &str) -> Self {
        Self {
            when: DateTime::parse_from_rfc3339(at)
                .ok()
                .map(|d| d.with_timezone(&Utc)),
            at: at.to_string(),
            id: id.to_string(),
        }
    }
}

/// Whether `s` is one Unicode emoji (D1): a single pictograph, keycap or
/// flag, with its variation selectors, skin tones and tags, or several
/// joined by zero-width joiners — at most [`MAX_EMOJI_BYTES`] bytes.
pub fn valid_emoji(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_EMOJI_BYTES {
        return false;
    }
    s.split('\u{200D}').all(valid_emoji_part)
}

fn valid_emoji_part(part: &str) -> bool {
    let mut chars = part.chars();
    let Some(base) = chars.next() else {
        return false;
    };
    let rest: Vec<char> = chars.collect();
    // A flag: two regional indicators.
    if is_regional(base) {
        return rest.len() == 1 && is_regional(rest[0]);
    }
    // A keycap: 0-9, # or *, VS16 optional, then U+20E3.
    if base.is_ascii_digit() || base == '#' || base == '*' {
        return matches!(rest.as_slice(), ['\u{20E3}'] | ['\u{FE0F}', '\u{20E3}']);
    }
    if !is_pictograph(base) {
        return false;
    }
    rest.iter().all(|&c| is_modifier(c))
}

fn is_regional(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

fn is_modifier(c: char) -> bool {
    c == '\u{FE0F}'
        || c == '\u{FE0E}'
        || c == '\u{20E3}'
        || ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
        || ('\u{E0020}'..='\u{E007F}').contains(&c)
}

fn is_pictograph(c: char) -> bool {
    matches!(c,
        '\u{00A9}' | '\u{00AE}' | '\u{203C}' | '\u{2049}' | '\u{2122}' | '\u{2139}'
        | '\u{2194}'..='\u{21AA}'
        | '\u{231A}'..='\u{23FF}'
        | '\u{24C2}'
        | '\u{25AA}'..='\u{25FE}'
        | '\u{2600}'..='\u{27BF}'
        | '\u{2934}' | '\u{2935}'
        | '\u{2B05}'..='\u{2B55}'
        | '\u{3030}' | '\u{303D}' | '\u{3297}' | '\u{3299}'
        | '\u{1F000}'..='\u{1F1E5}'
        | '\u{1F200}'..='\u{1FAFF}'
    )
}

/// The readable line a reaction post carries (D1), for clients and
/// runtimes that predate reactions. It quotes the start of the message
/// without its `@`s, so an older runtime cannot read it as a mention.
pub fn describe(op: &ReactOp, who: &str, target_author: &str, target_text: &str) -> String {
    let excerpt = excerpt(&target_text.replace('@', ""), 60);
    if op.remove {
        format!(
            "{who} removed {} from {target_author}: “{excerpt}”",
            op.emoji
        )
    } else {
        format!("{who} reacted {} to {target_author}: “{excerpt}”", op.emoji)
    }
}

/// The first `max` characters of `text` on one line, with `…` when cut.
pub fn excerpt(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use peko_protocol::channel::ChannelId;
    use peko_subject::CanonicalKeys;

    fn post(id: &str, author: &str, at: &str) -> ChannelEvent {
        ChannelEvent::Posted {
            channel: ChannelId::parse("chan_abcd1234").unwrap(),
            author: author.into(),
            parent: None,
            text: "hello".into(),
            at: at.into(),
            via: None,
            msg_id: Some(id.into()),
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

    fn react(
        id: &str,
        author: &str,
        at: &str,
        to: &str,
        emoji: &str,
        remove: bool,
    ) -> ChannelEvent {
        let mut ev = post(id, author, at);
        if let ChannelEvent::Posted { react, .. } = &mut ev {
            *react = Some(ReactOp {
                to: to.into(),
                emoji: emoji.into(),
                remove,
            });
        }
        ev
    }

    const MAYA: &str = "user:local:maya";
    const REX: &str = "principal:local:prin_rex";

    fn at(s: u32) -> String {
        format!("2026-11-25T12:00:{s:02}Z")
    }

    #[test]
    fn add_and_remove() {
        let log = [
            post("m1", REX, &at(0)),
            react("r1", MAYA, &at(1), "m1", "👍", false),
            react("r2", REX, &at(2), "m1", "👍", false),
            react("r3", MAYA, &at(3), "m1", "🎉", false),
        ];
        let r = Reactions::fold(&log, &CanonicalKeys);
        assert_eq!(
            r.on("m1"),
            vec![
                Reaction {
                    emoji: "👍".into(),
                    by: vec![MAYA.into(), REX.into()]
                },
                Reaction {
                    emoji: "🎉".into(),
                    by: vec![MAYA.into()]
                },
            ]
        );
        let mut log = log.to_vec();
        log.push(react("r4", MAYA, &at(4), "m1", "👍", true));
        log.push(react("r5", MAYA, &at(5), "m1", "🎉", true));
        let r = Reactions::fold(&log, &CanonicalKeys);
        assert_eq!(
            r.on("m1"),
            vec![Reaction {
                emoji: "👍".into(),
                by: vec![REX.into()]
            }]
        );
    }

    #[test]
    fn crossed_ops_fold_the_same_in_either_order() {
        let add = react("r1", MAYA, &at(1), "m1", "👍", false);
        let remove = react("r2", MAYA, &at(2), "m1", "👍", true);
        let one = Reactions::fold([&add, &remove], &CanonicalKeys);
        let other = Reactions::fold([&remove, &add], &CanonicalKeys);
        assert!(one.on("m1").is_empty());
        assert!(other.on("m1").is_empty());
        // Same instant: the larger msg_id is the later op.
        let a = react("r_a", MAYA, &at(1), "m1", "👍", true);
        let b = react("r_b", MAYA, &at(1), "m1", "👍", false);
        assert_eq!(Reactions::fold([&a, &b], &CanonicalKeys).on("m1").len(), 1);
        assert_eq!(Reactions::fold([&b, &a], &CanonicalKeys).on("m1").len(), 1);
    }

    #[test]
    fn one_actor_one_emoji_counts_once() {
        let log = [
            react("r1", MAYA, &at(1), "m1", "👍", false),
            react("r2", MAYA, &at(2), "m1", "👍", false),
        ];
        let r = Reactions::fold(&log, &CanonicalKeys);
        assert_eq!(r.on("m1")[0].by, vec![MAYA.to_string()]);
    }

    #[test]
    fn at_most_twenty_emoji_stand_on_a_message() {
        let emoji: Vec<String> = (0..21u32)
            .map(|i| char::from_u32(0x1F600 + i).unwrap().to_string())
            .collect();
        let log: Vec<ChannelEvent> = emoji
            .iter()
            .enumerate()
            .map(|(i, e)| react(&format!("r{i:02}"), MAYA, &at(i as u32), "m1", e, false))
            .collect();
        let r = Reactions::fold(&log, &CanonicalKeys);
        assert_eq!(r.emoji_count("m1"), MAX_EMOJI_PER_MESSAGE);
        assert!(!r.on("m1").iter().any(|x| x.emoji == emoji[20]));
    }

    #[test]
    fn an_op_before_its_target_applies_when_it_arrives() {
        let log = [react("r1", MAYA, &at(1), "m_later", "👀", false)];
        let r = Reactions::fold(&log, &CanonicalKeys);
        assert_eq!(r.on("m_later").len(), 1);
    }

    #[test]
    fn a_reaction_to_a_reaction_is_ignored() {
        let log = [
            post("m1", REX, &at(0)),
            react("r1", MAYA, &at(1), "m1", "👍", false),
            react("r2", REX, &at(2), "r1", "😂", false),
        ];
        let r = Reactions::fold(&log, &CanonicalKeys);
        assert!(r.on("r1").is_empty());
        assert_eq!(r.on("m1").len(), 1);
    }

    #[test]
    fn emoji_are_checked() {
        for ok in ["👍", "❤️", "👍🏽", "👨‍👩‍👧", "🇯🇵", "1️⃣", "#️⃣", "🏳️‍🌈", "✅", "🫡"]
        {
            assert!(valid_emoji(ok), "{ok:?} should be an emoji");
        }
        for bad in ["", "a", "ok", "👍👍", "👍 ", ":+1:", "🇯", "1", "<b>"] {
            assert!(!valid_emoji(bad), "{bad:?} should not be an emoji");
        }
        let long = "👍".to_string() + &"\u{FE0F}".repeat(30);
        assert!(!valid_emoji(&long));
    }

    #[test]
    fn invalid_emoji_from_a_remote_copy_is_ignored() {
        let log = [react("r1", MAYA, &at(1), "m1", "hello", false)];
        assert!(Reactions::fold(&log, &CanonicalKeys).is_empty());
    }

    #[test]
    fn the_readable_line_drops_mentions() {
        let op = ReactOp {
            to: "m1".into(),
            emoji: "👍".into(),
            remove: false,
        };
        let line = describe(&op, "maya", "rex", "@kit the tests pass now");
        assert_eq!(line, "maya reacted 👍 to rex: “kit the tests pass now”");
        assert!(!line.contains('@'));
    }

    #[test]
    fn standing_is_newest_first() {
        let log = [
            react("r1", MAYA, &at(1), "m1", "👍", false),
            react("r2", MAYA, &at(3), "m2", "🎉", false),
        ];
        let s = Reactions::fold(&log, &CanonicalKeys).standing();
        assert_eq!(s[0].to, "m2");
        assert_eq!(s[1].to, "m1");
    }

    #[test]
    fn an_older_runtime_parses_a_reaction_post() {
        // A `Posted` line as an older runtime's types would read it: the
        // unknown `react` field is ignored and the line still parses.
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct OldPosted {
            kind: String,
            author: String,
            parent: Option<String>,
            text: String,
        }
        let line = serde_json::to_string(&react("r1", MAYA, &at(1), "m1", "👍", false)).unwrap();
        assert!(line.contains("\"react\""));
        let old: OldPosted = serde_json::from_str(&line).unwrap();
        assert_eq!(old.kind, "posted");
        // And this runtime's own types round-trip it.
        let back: ChannelEvent = serde_json::from_str(&line).unwrap();
        assert!(!back.is_message());
        assert!(post("m", MAYA, &at(0)).is_message());
    }
}
