//! Questions a delegated run asks the run that delegated it (ADR-114 D4).
//!
//! A worker's final message is its result. When it cannot go on without
//! an answer, it calls `Ask` and ends its turn; the run ends normally and
//! the questions travel back with its result as `needs_input`. Nothing
//! waits: the delegating run answers by continuing the worker's session.
//!
//! The subagent executor marks each delegated run's session while the
//! run is live ([`expect_report`]) and collects what it asked when the
//! run ends ([`finish`]). `Ask` stages into a marked session only, so a
//! run with no one to report to (the trunk, a cron job) is refused.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use peko_protocol::channel::Ask;

/// Live delegated sessions, with the questions each has staged.
static DELEGATED: LazyLock<Mutex<HashMap<String, Option<Ask>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Mark `session` as a delegated run that reports back.
pub fn expect_report(session: &str) {
    DELEGATED
        .lock()
        .expect("delegated asks mutex")
        .insert(session.to_string(), None);
}

/// Stage `ask` for the delegated run in `session`, replacing any staged
/// before (`None` clears). Returns `false` when `session` is not a live
/// delegated run.
pub fn stage(session: &str, ask: Option<Ask>) -> bool {
    match DELEGATED
        .lock()
        .expect("delegated asks mutex")
        .get_mut(session)
    {
        Some(slot) => {
            *slot = ask;
            true
        }
        None => false,
    }
}

/// End the delegated run in `session`: unmark it and return what it asked.
pub fn finish(session: &str) -> Option<Ask> {
    DELEGATED
        .lock()
        .expect("delegated asks mutex")
        .remove(session)
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use peko_protocol::channel::Question;

    fn ask() -> Ask {
        Ask {
            for_actor: None,
            questions: vec![Question {
                text: "Which file?".into(),
                options: Vec::new(),
                multi: false,
            }],
        }
    }

    #[test]
    fn only_a_marked_session_stages_and_finish_takes_it_once() {
        let session = uuid::Uuid::new_v4().to_string();
        assert!(!stage(&session, Some(ask())), "not delegated");
        expect_report(&session);
        assert!(stage(&session, Some(ask())));
        assert_eq!(finish(&session), Some(ask()));
        assert_eq!(finish(&session), None);
        assert!(!stage(&session, Some(ask())), "unmarked after finish");

        expect_report(&session);
        assert!(stage(&session, Some(ask())));
        assert!(stage(&session, None), "cleared");
        assert_eq!(finish(&session), None);
    }
}
