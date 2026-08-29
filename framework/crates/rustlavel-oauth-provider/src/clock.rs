//! The one place this crate asks what time it is.
//!
//! Codes expire in a minute and tokens in an hour, so every test of an expiry
//! would otherwise have to sleep for one. A [`Clock`] is carried by the server
//! instead, and a test moves it forward by an hour without waiting.
//!
//! The skew lives on the server rather than in a global, so two tests running
//! concurrently — each with its own server — cannot move each other's clock.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The system clock, plus an offset a test can push forward.
#[derive(Clone, Default)]
pub struct Clock {
    skew: Arc<AtomicU64>,
}

impl Clock {
    pub fn system() -> Clock {
        Clock::default()
    }

    /// The current time in unix seconds.
    pub fn now(&self) -> u64 {
        rustlavel_auth::unix_now().saturating_add(self.skew.load(Ordering::Relaxed))
    }

    /// Jump forward, so a test can watch something expire.
    ///
    /// Only ever forward: a clock that can be wound back would let a test —
    /// and, in production, a caller — resurrect an expired token.
    pub fn advance(&self, seconds: u64) {
        self.skew.fetch_add(seconds, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clock").field("skew", &self.skew.load(Ordering::Relaxed)).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_plausible_wall_clock() {
        let now = Clock::system().now();
        assert!(now > 1_577_836_800, "the system clock reported {now}");
    }

    #[test]
    fn advancing_moves_time_forward_and_nothing_else() {
        let clock = Clock::system();
        let before = clock.now();

        clock.advance(3600);

        assert!(clock.now() >= before + 3600);
        // A second clock is unaffected: the skew is per server, not global.
        assert!(Clock::system().now() < before + 3600);
    }

    #[test]
    fn a_clone_shares_the_same_skew() {
        // Handlers hold clones of the server; they must all agree on the time.
        let clock = Clock::system();
        let clone = clock.clone();
        clock.advance(120);

        assert!(clone.now() >= clock.now().saturating_sub(1));
        assert!(clone.now() >= rustlavel_auth::unix_now() + 120);
    }
}
