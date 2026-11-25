//! Order across a path switch (ADR-125 D6).
//!
//! A frame sent on a link and the next sent through PekoHub can arrive
//! the other way round. The sealed body of each channel event and invite
//! carries a counter per pair of computers, `seq`, and the receiver holds
//! a frame that arrives ahead of its turn for up to [`HOLD_FOR`].
//!
//! `seq` is `{ "e": <epoch>, "n": <counter> }`: the epoch is new each
//! time the sender starts, so a restart is a new count rather than a
//! counter that went back. A resend of a frame (same `request_id`, to the
//! same computer) keeps the number it had, so it fills the gap it left.
//! A frame without `seq` (an older sender) is never held.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How long a frame that arrived ahead of its turn is held.
pub const HOLD_FOR: Duration = Duration::from_secs(2);
/// The sealed-body field.
pub const SEQ_FIELD: &str = "seq";
/// How many (computer, request) numbers are remembered for resends.
const REMEMBER: usize = 8192;

/// The frame types that carry `seq`.
#[must_use]
pub fn is_ordered_type(ty: &str) -> bool {
    matches!(ty, "tunnel_channel_event" | "tunnel_channel_invite")
}

/// A frame's place in its sender's count to one computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seq {
    #[serde(rename = "e")]
    pub epoch: String,
    #[serde(rename = "n")]
    pub n: u64,
}

/// Numbers frames to each computer (the sender's side).
pub struct Sequencer {
    epoch: String,
    inner: Mutex<SequencerInner>,
}

#[derive(Default)]
struct SequencerInner {
    next: HashMap<String, u64>,
    given: HashMap<(String, String), u64>,
    order: VecDeque<(String, String)>,
}

impl Default for Sequencer {
    fn default() -> Self {
        Self::new()
    }
}

impl Sequencer {
    /// A count that starts now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            epoch: format!("{:016x}", rand::random::<u64>()),
            inner: Mutex::new(SequencerInner::default()),
        }
    }

    /// The number of the frame `request_id` to `recipient`: the one it had
    /// when it was sent before, else the next.
    pub fn stamp(&self, recipient: &str, request_id: &str) -> Seq {
        let mut inner = self.inner.lock().unwrap();
        let key = (recipient.to_string(), request_id.to_string());
        if let Some(n) = inner.given.get(&key) {
            return Seq {
                epoch: self.epoch.clone(),
                n: *n,
            };
        }
        let next = inner.next.entry(recipient.to_string()).or_insert(1);
        let n = *next;
        *next += 1;
        inner.given.insert(key.clone(), n);
        inner.order.push_back(key);
        while inner.order.len() > REMEMBER {
            if let Some(old) = inner.order.pop_front() {
                inner.given.remove(&old);
            }
        }
        Seq {
            epoch: self.epoch.clone(),
            n,
        }
    }
}

struct Held<T> {
    item: T,
    at: Instant,
}

struct FromPeer<T> {
    epoch: String,
    next: u64,
    held: BTreeMap<u64, Held<T>>,
}

/// Puts frames from each computer back in their sender's order (the
/// receiver's side).
pub struct Reorder<T> {
    hold: Duration,
    peers: HashMap<String, FromPeer<T>>,
}

impl<T> Default for Reorder<T> {
    fn default() -> Self {
        Self::new(HOLD_FOR)
    }
}

impl<T> Reorder<T> {
    /// Holding early frames for `hold`.
    #[must_use]
    pub fn new(hold: Duration) -> Self {
        Self {
            hold,
            peers: HashMap::new(),
        }
    }

    /// A frame from `source` numbered `seq` arrived at `now`: the frames
    /// to hand on now, in order.
    pub fn push(&mut self, source: &str, seq: Seq, item: T, now: Instant) -> Vec<T> {
        let mut out = Vec::new();
        let peer = match self.peers.get_mut(source) {
            Some(peer) if peer.epoch == seq.epoch => peer,
            Some(peer) => {
                // The sender started again: what was held of its last
                // count goes now, and the new count starts here.
                out.extend(std::mem::take(&mut peer.held).into_values().map(|h| h.item));
                peer.epoch = seq.epoch.clone();
                peer.next = seq.n;
                peer
            }
            None => self.peers.entry(source.to_string()).or_insert(FromPeer {
                epoch: seq.epoch.clone(),
                next: seq.n,
                held: BTreeMap::new(),
            }),
        };
        if seq.n < peer.next {
            // Late, or a resend of one handed on already.
            out.push(item);
            return out;
        }
        if seq.n > peer.next {
            peer.held.insert(seq.n, Held { item, at: now });
            return out;
        }
        out.push(item);
        peer.next += 1;
        Self::drain(peer, &mut out);
        out
    }

    fn drain(peer: &mut FromPeer<T>, out: &mut Vec<T>) {
        while let Some(held) = peer.held.remove(&peer.next) {
            out.push(held.item);
            peer.next += 1;
        }
    }

    /// The frames whose hold ran out by `now` (with any held before them),
    /// in order; the count moves past them.
    pub fn due(&mut self, now: Instant) -> Vec<T> {
        let mut out = Vec::new();
        for peer in self.peers.values_mut() {
            let last_expired = peer
                .held
                .iter()
                .filter(|(_, h)| now.duration_since(h.at) >= self.hold)
                .map(|(n, _)| *n)
                .max();
            let Some(last) = last_expired else { continue };
            let rest = peer.held.split_off(&(last + 1));
            let released = std::mem::replace(&mut peer.held, rest);
            out.extend(released.into_values().map(|h| h.item));
            peer.next = last + 1;
            Self::drain(peer, &mut out);
        }
        out
    }

    /// When the next hold runs out.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.peers
            .values()
            .flat_map(|p| p.held.values())
            .map(|h| h.at + self.hold)
            .min()
    }

    /// How many frames are held.
    #[must_use]
    pub fn held(&self) -> usize {
        self.peers.values().map(|p| p.held.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(e: &str, n: u64) -> Seq {
        Seq { epoch: e.into(), n }
    }

    #[test]
    fn the_sender_counts_per_computer_and_a_resend_keeps_its_number() {
        let s = Sequencer::new();
        assert_eq!(s.stamp("B", "r1").n, 1);
        assert_eq!(s.stamp("B", "r2").n, 2);
        assert_eq!(s.stamp("C", "r1").n, 1);
        assert_eq!(s.stamp("B", "r1").n, 1, "a resend keeps its number");
        assert_eq!(s.stamp("B", "r3").n, 3);
        assert_ne!(Sequencer::new().epoch, s.epoch);
    }

    #[test]
    fn in_order_frames_pass_straight_through() {
        let mut r = Reorder::new(HOLD_FOR);
        let now = Instant::now();
        assert_eq!(r.push("A", seq("e", 1), 1, now), vec![1]);
        assert_eq!(r.push("A", seq("e", 2), 2, now), vec![2]);
        assert_eq!(r.held(), 0);
    }

    #[test]
    fn an_early_frame_waits_for_the_one_before_it() {
        let mut r = Reorder::new(HOLD_FOR);
        let now = Instant::now();
        assert_eq!(r.push("A", seq("e", 1), 1, now), vec![1]);
        assert!(r.push("A", seq("e", 3), 3, now).is_empty());
        assert!(r.push("A", seq("e", 4), 4, now).is_empty());
        assert_eq!(r.held(), 2);
        assert_eq!(r.push("A", seq("e", 2), 2, now), vec![2, 3, 4]);
        assert_eq!(r.held(), 0);
    }

    #[test]
    fn a_gap_that_does_not_fill_is_given_up_after_the_hold() {
        let mut r = Reorder::new(HOLD_FOR);
        let t0 = Instant::now();
        r.push("A", seq("e", 1), 1, t0);
        assert!(r.push("A", seq("e", 3), 3, t0).is_empty());
        assert!(r.due(t0 + Duration::from_millis(1999)).is_empty());
        assert_eq!(r.next_deadline(), Some(t0 + HOLD_FOR));
        assert_eq!(r.due(t0 + HOLD_FOR), vec![3]);
        // The count moved past the gap; the missing one, when it comes,
        // is late and is handed on at once.
        assert_eq!(r.push("A", seq("e", 4), 4, t0 + HOLD_FOR), vec![4]);
        assert_eq!(r.push("A", seq("e", 2), 2, t0 + HOLD_FOR), vec![2]);
    }

    #[test]
    fn computers_are_ordered_apart() {
        let mut r = Reorder::new(HOLD_FOR);
        let now = Instant::now();
        r.push("A", seq("e", 1), "a1", now);
        assert!(r.push("A", seq("e", 3), "a3", now).is_empty());
        assert_eq!(r.push("B", seq("f", 7), "b7", now), vec!["b7"]);
        assert_eq!(r.push("B", seq("f", 8), "b8", now), vec!["b8"]);
    }

    #[test]
    fn a_restarted_sender_starts_a_new_count() {
        let mut r = Reorder::new(HOLD_FOR);
        let now = Instant::now();
        r.push("A", seq("old", 10), 10, now);
        assert!(r.push("A", seq("old", 12), 12, now).is_empty());
        // New epoch: what was held goes first, then the new count.
        assert_eq!(r.push("A", seq("new", 1), 1, now), vec![12, 1]);
        assert_eq!(r.push("A", seq("new", 2), 2, now), vec![2]);
    }

    #[test]
    fn only_channel_events_and_invites_are_ordered() {
        assert!(is_ordered_type("tunnel_channel_event"));
        assert!(is_ordered_type("tunnel_channel_invite"));
        assert!(!is_ordered_type("tunnel_attachment_chunk"));
        assert!(!is_ordered_type("tunnel_channel_history"));
    }
}
