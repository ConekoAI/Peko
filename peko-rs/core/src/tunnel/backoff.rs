//! Exponential Backoff for Tunnel Reconnection
//!
//! Every runtime keeps one tunnel to PekoHub, and a Hub restart drops all
//! of them at the same moment. Without jitter they would all retry on the
//! same schedule (1, 2, 4 … s) and reach the Hub together, again and
//! again, while it can complete only a few handshakes a second. So:
//!
//! * the first reconnect after a drop waits a random time in
//!   `[0, first_spread]` ([`ExponentialBackoff::first_reconnect_delay`]);
//! * each retry after a failed attempt waits a random time in the upper
//!   half of its exponential step, `[d/2, d]` ("equal jitter"), where
//!   `d` is 1, 2, 4 … s capped at `max_secs`
//!   ([`ExponentialBackoff::next`]).

use std::time::Duration;

use rand::Rng;

/// Default upper bound of the random wait before the first reconnect
/// after a drop.
const DEFAULT_FIRST_SPREAD: Duration = Duration::from_secs(10);

/// Exponential backoff with jitter for reconnection delays.
#[derive(Debug, Clone)]
pub struct ExponentialBackoff {
    current_secs: u64,
    initial_secs: u64,
    max_secs: u64,
    base: u64,
    first_spread: Duration,
}

impl ExponentialBackoff {
    /// Create a new backoff with the given parameters.
    ///
    /// # Arguments
    /// * `initial_secs` - Starting step in seconds
    /// * `max_secs` - Cap on a step in seconds (no delay ever exceeds it)
    /// * `base` - Multiplication factor (at least 2)
    #[must_use]
    pub fn new(initial_secs: u64, max_secs: u64, base: u64) -> Self {
        Self {
            current_secs: initial_secs.min(max_secs),
            initial_secs: initial_secs.min(max_secs),
            max_secs,
            base: base.max(2),
            first_spread: DEFAULT_FIRST_SPREAD,
        }
    }

    /// Set the upper bound of [`Self::first_reconnect_delay`].
    #[must_use]
    pub fn with_first_spread(mut self, first_spread: Duration) -> Self {
        self.first_spread = first_spread;
        self
    }

    /// Get the next delay and advance the backoff.
    ///
    /// The delay is random in `[d/2, d]`, where `d` is the current
    /// exponential step (never above the cap).
    #[must_use]
    pub fn next(&mut self) -> Duration {
        self.next_with(&mut rand::thread_rng())
    }

    fn next_with<R: Rng + ?Sized>(&mut self, rng: &mut R) -> Duration {
        let step_ms = self.current_secs.saturating_mul(1000);
        self.current_secs = self
            .current_secs
            .saturating_mul(self.base)
            .min(self.max_secs);
        let half = step_ms / 2;
        Duration::from_millis(half + rng.gen_range(0..=step_ms - half))
    }

    /// Random wait in `[0, first_spread]` before reconnecting after an
    /// established tunnel dropped, so runtimes dropped together by a Hub
    /// restart do not all knock at once.
    #[must_use]
    pub fn first_reconnect_delay(&self) -> Duration {
        self.first_reconnect_delay_with(&mut rand::thread_rng())
    }

    fn first_reconnect_delay_with<R: Rng + ?Sized>(&self, rng: &mut R) -> Duration {
        let spread_ms = u64::try_from(self.first_spread.as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(rng.gen_range(0..=spread_ms))
    }

    /// Reset the backoff to the initial step.
    pub fn reset(&mut self) {
        self.current_secs = self.initial_secs;
    }

    /// Create the default backoff: steps of 1s, 2s, 4s, 8s, ... capped at
    /// 60s, each jittered to `[d/2, d]`; first reconnect after a drop
    /// within 10s.
    #[must_use]
    pub fn default_tunnel() -> Self {
        Self::new(1, 60, 2)
    }
}

impl Default for ExponentialBackoff {
    fn default() -> Self {
        Self::default_tunnel()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    const SAMPLES: usize = 500;

    /// Assert that `delay` lies in `[step/2, step]`.
    fn assert_in_step(delay: Duration, step_secs: u64) {
        let step = Duration::from_secs(step_secs);
        assert!(
            delay >= step / 2 && delay <= step,
            "delay {delay:?} outside [{:?}, {step:?}]",
            step / 2
        );
    }

    #[test]
    fn test_exponential_backoff() {
        let steps = [1, 2, 4, 8, 16, 32, 60, 60];
        for _ in 0..SAMPLES {
            let mut backoff = ExponentialBackoff::new(1, 60, 2);
            for step in steps {
                assert_in_step(backoff.next(), step);
            }
        }
    }

    #[test]
    fn test_backoff_never_exceeds_cap() {
        let mut backoff = ExponentialBackoff::new(1, 60, 2);
        for _ in 0..SAMPLES {
            assert!(backoff.next() <= Duration::from_mins(1));
        }
    }

    #[test]
    fn test_backoff_reset() {
        let mut backoff = ExponentialBackoff::new(1, 60, 2);
        assert_in_step(backoff.next(), 1);
        assert_in_step(backoff.next(), 2);
        backoff.reset();
        assert_in_step(backoff.next(), 1);
    }

    #[test]
    fn test_backoff_with_base_3() {
        let steps = [1, 3, 9, 27, 30];
        for _ in 0..SAMPLES {
            let mut backoff = ExponentialBackoff::new(1, 30, 3);
            for step in steps {
                assert_in_step(backoff.next(), step);
            }
        }
    }

    /// Runtimes on the same schedule must not retry at the same instant.
    #[test]
    fn test_backoff_delays_are_spread() {
        let mut rng = StdRng::seed_from_u64(7);
        let delays: std::collections::BTreeSet<Duration> = (0..SAMPLES)
            .map(|_| ExponentialBackoff::new(1, 60, 2).next_with(&mut rng))
            .collect();
        assert!(delays.len() > SAMPLES / 2, "only {} distinct", delays.len());
        let spread = delays
            .last()
            .unwrap()
            .saturating_sub(*delays.first().unwrap());
        assert!(spread >= Duration::from_millis(400), "spread {spread:?}");
    }

    #[test]
    fn test_first_reconnect_delay_within_spread() {
        let backoff = ExponentialBackoff::default_tunnel();
        for _ in 0..SAMPLES {
            assert!(backoff.first_reconnect_delay() <= DEFAULT_FIRST_SPREAD);
        }
        let short = ExponentialBackoff::default_tunnel().with_first_spread(Duration::from_secs(2));
        for _ in 0..SAMPLES {
            assert!(short.first_reconnect_delay() <= Duration::from_secs(2));
        }
    }

    #[test]
    fn test_first_reconnect_delay_is_spread() {
        let backoff = ExponentialBackoff::default_tunnel();
        let mut rng = StdRng::seed_from_u64(11);
        let delays: Vec<Duration> = (0..SAMPLES)
            .map(|_| backoff.first_reconnect_delay_with(&mut rng))
            .collect();
        let min = *delays.iter().min().unwrap();
        let max = *delays.iter().max().unwrap();
        // Uniform over 10s: 500 samples cover most of the window.
        assert!(min < Duration::from_secs(1), "min {min:?}");
        assert!(max > Duration::from_secs(9), "max {max:?}");
    }

    #[test]
    fn test_zero_first_spread_reconnects_at_once() {
        let backoff = ExponentialBackoff::default_tunnel().with_first_spread(Duration::ZERO);
        assert_eq!(backoff.first_reconnect_delay(), Duration::ZERO);
    }
}
