//! Invite links a session made, so its replies carry them exactly.
//!
//! A group invite link is a long signed token (`…/g/<id>?invite=<jws>`).
//! A model that retypes one from its `Channel link` result into a reply
//! can garble it: seen live with Claude Haiku on 2026-11-23, twice in a
//! row, with the same bytes wrong, so the person got a link that does not
//! work. The runtime remembers each link a session makes
//! ([`remember`]), and before that session's text goes out ([`repair`])
//! replaces any invite link in it that is not one of them with the one it
//! stands for.
//!
//! Only links the same session made are put in, and only where its text
//! already has an invite link. A session cannot get a working link it was
//! not given: one that writes a made-up link for a group gets nothing
//! unless it made a link to that group itself.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// How long a made link is remembered for its session's replies.
const REMEMBER_FOR: Duration = Duration::from_hours(1);

/// Most links remembered per session (the newest are kept).
const PER_SESSION: usize = 20;

/// One link a session made.
#[derive(Debug, Clone)]
struct Minted {
    link: String,
    /// The group id in the link's path (`…/g/<id>`).
    group: Option<String>,
    at: Instant,
}

static MINTED: LazyLock<Mutex<HashMap<String, Vec<Minted>>>> = LazyLock::new(Mutex::default);

static INVITE_LINK: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"https?://[^\s<>()\[\]`]*?/g/([^\s/?#<>()\[\]`]+)\?invite=[A-Za-z0-9._~%-]+")
        .expect("invite link pattern")
});

/// The key a session is remembered under: its canonical id, so the id a
/// tool sees and the one its turn runs under agree.
fn key(session: &str) -> String {
    peko_session::id::SessionId::parse(session)
        .map_or_else(|| session.to_string(), |id| id.to_string())
}

/// The group id in `link`'s path.
fn group_of(link: &str) -> Option<String> {
    INVITE_LINK
        .captures(link)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Remember that `session` made `link`.
pub(crate) fn remember(session: &str, link: &str) {
    let now = Instant::now();
    let mut minted = MINTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    minted.retain(|_, links| {
        links.retain(|m| now.duration_since(m.at) < REMEMBER_FOR);
        !links.is_empty()
    });
    let links = minted.entry(key(session)).or_default();
    links.push(Minted {
        link: link.to_string(),
        group: group_of(link),
        at: now,
    });
    if links.len() > PER_SESSION {
        let extra = links.len() - PER_SESSION;
        links.drain(..extra);
    }
}

/// `text` with every invite link `session` did not make replaced by the
/// one it made for the same group, or, when it made links to one group
/// only, by the newest of them. Text with no invite link, or from a
/// session that made none, comes back unchanged.
pub(crate) fn repair(session: &str, text: &str) -> String {
    if !text.contains("invite=") {
        return text.to_string();
    }
    let links = {
        let minted = MINTED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match minted.get(&key(session)) {
            Some(links) => links
                .iter()
                .filter(|m| m.at.elapsed() < REMEMBER_FOR)
                .cloned()
                .collect::<Vec<_>>(),
            None => return text.to_string(),
        }
    };
    if links.is_empty() {
        return text.to_string();
    }
    let one_group = {
        let mut groups: Vec<_> = links.iter().map(|m| m.group.as_deref()).collect();
        groups.dedup();
        groups.len() == 1
    };
    INVITE_LINK
        .replace_all(text, |caps: &regex::Captures<'_>| {
            let written = &caps[0];
            if links.iter().any(|m| m.link == written) {
                return written.to_string();
            }
            let group = caps.get(1).map(|m| m.as_str());
            let same_group = links.iter().rev().find(|m| m.group.as_deref() == group);
            let stands_for = same_group.or_else(|| one_group.then(|| links.last()).flatten());
            match stands_for {
                Some(m) => {
                    tracing::info!(
                        session,
                        "a reply carried a garbled invite link; it goes out as the one this \
                         session made"
                    );
                    m.link.clone()
                }
                None => written.to_string(),
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "http://127.0.0.1:3942/g/118297777254957056?invite=eyJhbGci.eyJ2IjoicGVrby1ncm91cC1pbnZpdGUvMSIsImMiOiJncm91cDpzdGF0cy10ZWFtIn0.sig-_A";
    const GARBLED: &str = "http://127.0.0.1:3942/g/118297777254957056?invite=eyJhbGci.eyJ2IjoicGVrby1ncm91cC1pbnZpdGUvMSIsImMiOiJncm91cDpzdGF0cy10eFtbGFtIn0.sig-_A";

    fn session(tag: &str) -> String {
        format!("test-session-{tag}-{}", uuid::Uuid::new_v4())
    }

    #[test]
    fn a_garbled_link_goes_out_as_the_one_the_session_made() {
        let s = session("garbled");
        remember(&s, GOOD);
        let reply = format!("Here is the link for Noah:\n\n{GARBLED}\n\nIt works for 7 days.");
        assert_eq!(
            repair(&s, &reply),
            format!("Here is the link for Noah:\n\n{GOOD}\n\nIt works for 7 days.")
        );
    }

    #[test]
    fn a_correct_link_and_plain_text_are_left_alone() {
        let s = session("correct");
        remember(&s, GOOD);
        let reply = format!("Link: <{GOOD}>");
        assert_eq!(repair(&s, &reply), reply);
        assert_eq!(repair(&s, "no links here"), "no links here");
    }

    #[test]
    fn another_sessions_links_are_never_put_in() {
        let maker = session("maker");
        remember(&maker, GOOD);
        let other = session("other");
        assert_eq!(repair(&other, GARBLED), GARBLED);
    }

    #[test]
    fn a_link_to_another_group_is_left_alone_when_the_session_made_several() {
        let s = session("groups");
        remember(&s, GOOD);
        remember(&s, "https://pekohub.ai/g/42?invite=aaa.bbb.ccc");
        let unknown = "https://pekohub.ai/g/99?invite=xxx.yyy.zzz";
        assert_eq!(repair(&s, unknown), unknown);
        assert_eq!(repair(&s, GARBLED), GOOD);
    }

    #[test]
    fn with_links_to_one_group_a_link_garbled_in_its_path_is_repaired_too() {
        let s = session("path");
        remember(&s, GOOD);
        let path_garbled = GARBLED.replace("118297777254957056", "11829777725495705");
        assert_eq!(repair(&s, &path_garbled), GOOD);
    }

    /// The pair seen live (collab benchmark, Claude Haiku, 2026-11-23):
    /// the link `Channel link` made, and the reply that carried it with
    /// `stats-team` garbled inside the token.
    #[test]
    fn the_link_garbled_live_goes_out_as_made() {
        const MADE: &str = "http://127.0.0.1:3962/g/118298712886738944?invite=eyJhbGciOiJFZERTQSIsInR5cCI6IkpXUyJ9.eyJ2IjoicGVrby1ncm91cC1pbnZpdGUvMSIsImciOiIxMTgyOTg3MTI4ODY3Mzg5NDQiLCJjIjoiZ3JvdXA6c3RhdHMtdGVhbSIsImgiOiJkaWQ6a2V5Ono2TWtlcDdDY3gyU2FMVGp2TVBaZDY1NWU0Q1RyMWtpS0FOYzRTR2V6QTJGOXhQVSIsImV4cCI6MTc5NjAzNTAxMSwibiI6IjhmM2QxOWFmLWE4YmYtNGRkMS1iMWY3LTllZWQ5OWQ4NDExMiJ9.c_SGK-vwZbzW_M7O_rV3nUM08GK8BOXfcSal0fnsjfbgh5gGdguGSzxA8KDz4AiSD1ZcoQ_a4LR0tu6cvAwJAg";
        const WRITTEN: &str = "http://127.0.0.1:3962/g/118298712886738944?invite=eyJhbGciOiJFZERTQSIsInR5cCI6IkpXUyJ9.eyJ2IjoicGVrby1ncm91cC1pbnZpdGUvMSIsImciOiIxMTgyOTg3MTI4ODY3Mzg5NDQiLCJjIjoiZ3JvdXA6c3RhdHMtdGeFtbS1pbnZpdGUtdGVhbSIsImgiOiJkaWQ6a2V5Ono2TWtlcDdDY3gyU2FMVGp2TVBaZDY1NWU0Q1RyMWtpS0FOYzRTR2V6QTJGOXhQVSIsImV4cCI6MTc5NjAzNTAxMSwibiI6IjhmM2QxOWFmLWE4YmYtNGRkMS1iMWY3LTllZWQ5OWQ4NDExMiJ9.c_SGK-vwZbzW_M7O_rV3nUM08GK8BOXfcSal0fnsjfbgh5gGdguGSzxA8KDz4AiSD1ZcoQ_a4LR0tu6cvAwJAg";
        let s = session("live");
        remember(&s, MADE);
        let reply = format!(
            "I created **stats-team** and made it invite-only. Here is Noah's invite link \
             (valid 7 days, until 2026-11-30):\n\n{WRITTEN}\n\nI did not add Otto."
        );
        let sent = repair(&s, &reply);
        assert!(sent.contains(MADE), "{sent}");
        assert!(!sent.contains(WRITTEN));
    }
}
