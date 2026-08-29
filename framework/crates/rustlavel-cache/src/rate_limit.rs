//! Rate limiting on top of the [`Cache`] trait.
//!
//! # Fixed window, and what that costs
//!
//! A counter is kept per *window*: the key embeds `now / window`, so at every
//! boundary the counter is a new key that starts at zero and expires on its own.
//! One increment per request, one key per window, nothing to clean up.
//!
//! The price is burstiness at the seam. With a limit of 60 per minute, a client
//! can send 60 requests in the last instant of one window and 60 in the first
//! instant of the next — 120 in a moment, twice the nominal rate. A sliding
//! window (a sorted set of timestamps, or a weighted blend of the current and
//! previous window) removes that, at the cost of storing a timestamp per
//! request or of a second read on every request.
//!
//! Fixed window is the right default here because the job of a `throttle`
//! middleware is to stop abuse and runaway clients, and a 2× burst for one
//! instant does not defeat that — while the sorted-set approach makes every
//! request more expensive for every honest user. Anything that genuinely needs
//! a smooth rate (billing, an upstream API quota) should be built on
//! [`RateLimiter::attempt`]'s reported window rather than pretending the
//! boundary does not exist.
//!
//! # Which driver
//!
//! The limiter is only as shared as its cache. The memory driver counts per
//! process, so four workers behind a load balancer allow four times the limit;
//! use the Redis driver whenever more than one process serves traffic.

use crate::store::Cache;
use rustlavel_core::Result;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Keys are namespaced so a limiter can share a cache with ordinary entries
/// without a `user:1` counter ever colliding with a `user:1` cache value.
const NAMESPACE: &str = "rustlavel:throttle:";

/// The outcome of one attempt, and everything the `X-RateLimit-*` headers need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimit {
    /// The configured ceiling for the window.
    pub limit: u64,
    /// How many attempts this key has made in the current window.
    pub used: u64,
    /// How many remain. Zero once the limit is reached.
    pub remaining: u64,
    /// How long until the window resets — what `Retry-After` reports.
    pub reset_after: Duration,
    /// Whether this attempt was refused.
    pub exceeded: bool,
}

impl RateLimit {
    /// `Retry-After` is expressed in whole seconds and must never be `0`, or a
    /// client reads it as "retry immediately" and hammers straight back.
    pub fn retry_after_seconds(&self) -> u64 {
        self.reset_after.as_secs().max(1)
    }

    /// The Unix timestamp at which the window resets, for `X-RateLimit-Reset`.
    pub fn reset_at(&self) -> u64 {
        now_millis().div_euclid(1000) + self.reset_after.as_secs()
    }
}

/// Counts attempts per key and window against any [`Cache`].
#[derive(Clone)]
pub struct RateLimiter {
    store: Arc<dyn Cache>,
}

impl RateLimiter {
    pub fn new(store: Arc<dyn Cache>) -> Self {
        RateLimiter { store }
    }

    /// Build a limiter over a driver held by value.
    pub fn with_driver(store: impl Cache) -> Self {
        RateLimiter { store: Arc::new(store) }
    }

    /// Record one attempt against `key` and report where it stands.
    ///
    /// The counter is incremented even when the limit is already exceeded. That
    /// is deliberate: a client that keeps hammering after a 429 keeps its
    /// window alive, which is exactly the behaviour you want from a throttle.
    pub async fn attempt(&self, key: &str, limit: u64, window: Duration) -> Result<RateLimit> {
        let window_millis = window.as_millis().max(1) as u64;
        let now = now_millis();
        let slot = now / window_millis;

        let counter = format!("{NAMESPACE}{key}:{slot}");

        // A little slack past the boundary so a request that arrives just as
        // the window turns cannot find its counter already swept away.
        let ttl = Duration::from_millis(window_millis + 1_000);
        let used = self.store.increment_within(&counter, 1, ttl).await?.max(0) as u64;

        // Computed from the slot rather than read back from the store, so every
        // driver reports the same reset instant whether or not it can answer
        // a TTL query cheaply.
        let window_ends = (slot + 1) * window_millis;
        let reset_after = Duration::from_millis(window_ends.saturating_sub(now));

        Ok(RateLimit {
            limit,
            used,
            remaining: limit.saturating_sub(used),
            reset_after,
            exceeded: used > limit,
        })
    }

    /// Whether the key has already exhausted its window, without spending an
    /// attempt on the question.
    pub async fn too_many(&self, key: &str, limit: u64, window: Duration) -> Result<bool> {
        Ok(self.used(key, window).await? >= limit)
    }

    /// How many attempts the key has made in the current window.
    pub async fn used(&self, key: &str, window: Duration) -> Result<u64> {
        let window_millis = window.as_millis().max(1) as u64;
        let slot = now_millis() / window_millis;
        let counter = format!("{NAMESPACE}{key}:{slot}");

        Ok(self
            .store
            .get(&counter)
            .await?
            .and_then(|value| value.as_i64())
            .unwrap_or(0)
            .max(0) as u64)
    }

    /// Forget a key's current window — after a successful login, say, so a user
    /// who finally got their password right is not still locked out.
    pub async fn clear(&self, key: &str, window: Duration) -> Result<()> {
        let window_millis = window.as_millis().max(1) as u64;
        let slot = now_millis() / window_millis;
        self.store.forget(&format!("{NAMESPACE}{key}:{slot}")).await?;
        Ok(())
    }
}

fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;

    fn limiter() -> RateLimiter {
        RateLimiter::with_driver(MemoryStore::new())
    }

    #[tokio::test]
    async fn the_first_attempts_are_allowed_and_the_next_one_is_not() {
        let limiter = limiter();
        let window = Duration::from_secs(60);

        for expected_remaining in (0..3).rev() {
            let outcome = limiter.attempt("ada", 3, window).await.unwrap();
            assert!(!outcome.exceeded);
            assert_eq!(outcome.remaining, expected_remaining);
        }

        let refused = limiter.attempt("ada", 3, window).await.unwrap();
        assert!(refused.exceeded);
        assert_eq!(refused.remaining, 0);
        assert_eq!(refused.used, 4);
    }

    #[tokio::test]
    async fn two_keys_are_counted_separately() {
        let limiter = limiter();
        let window = Duration::from_secs(60);

        limiter.attempt("ada", 1, window).await.unwrap();
        limiter.attempt("ada", 1, window).await.unwrap();

        assert!(limiter.attempt("ada", 1, window).await.unwrap().exceeded);
        assert!(!limiter.attempt("grace", 1, window).await.unwrap().exceeded);
    }

    #[tokio::test]
    async fn a_window_that_passes_lets_the_client_back_in() {
        let limiter = limiter();
        let window = Duration::from_millis(80);

        limiter.attempt("ada", 1, window).await.unwrap();
        assert!(limiter.attempt("ada", 1, window).await.unwrap().exceeded);

        // Two windows, to be sure the boundary was crossed however the first
        // attempt happened to land inside its slot.
        tokio::time::sleep(Duration::from_millis(180)).await;
        assert!(!limiter.attempt("ada", 1, window).await.unwrap().exceeded);
    }

    #[tokio::test]
    async fn retry_after_is_never_zero_seconds() {
        let limiter = limiter();
        // A sub-second window would otherwise round down to "retry now".
        let outcome = limiter.attempt("ada", 1, Duration::from_millis(200)).await.unwrap();

        assert!(outcome.reset_after < Duration::from_millis(201));
        assert_eq!(outcome.retry_after_seconds(), 1);
        assert!(outcome.reset_at() >= now_millis() / 1000);
    }

    #[tokio::test]
    async fn too_many_reports_the_state_without_spending_an_attempt() {
        let limiter = limiter();
        let window = Duration::from_secs(60);

        limiter.attempt("ada", 2, window).await.unwrap();
        assert!(!limiter.too_many("ada", 2, window).await.unwrap());
        assert_eq!(limiter.used("ada", window).await.unwrap(), 1);

        limiter.attempt("ada", 2, window).await.unwrap();
        assert!(limiter.too_many("ada", 2, window).await.unwrap());
        // Asking twice must not have counted as two more attempts.
        assert_eq!(limiter.used("ada", window).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn clearing_a_key_gives_the_whole_window_back() {
        let limiter = limiter();
        let window = Duration::from_secs(60);

        limiter.attempt("ada", 1, window).await.unwrap();
        assert!(limiter.attempt("ada", 1, window).await.unwrap().exceeded);

        limiter.clear("ada", window).await.unwrap();
        assert!(!limiter.attempt("ada", 1, window).await.unwrap().exceeded);
    }

    #[tokio::test]
    async fn a_limiter_key_cannot_collide_with_an_ordinary_cache_entry() {
        let store = MemoryStore::new();
        store.forever("ada", rustlavel_core::Json::from("a cached value")).await.unwrap();

        let limiter = RateLimiter::new(Arc::new(store.clone()));
        limiter.attempt("ada", 5, Duration::from_secs(60)).await.unwrap();

        assert_eq!(
            store.get("ada").await.unwrap(),
            Some(rustlavel_core::Json::from("a cached value")),
            "the limiter must not have trampled the cached value"
        );
    }

    #[tokio::test]
    async fn concurrent_attempts_never_let_more_than_the_limit_through() {
        let limiter = limiter();
        let window = Duration::from_secs(60);

        let mut tasks = Vec::new();
        for _ in 0..40 {
            let limiter = limiter.clone();
            tasks.push(tokio::spawn(async move {
                limiter.attempt("shared", 10, window).await.unwrap().exceeded
            }));
        }

        let mut allowed = 0;
        for task in tasks {
            if !task.await.unwrap() {
                allowed += 1;
            }
        }

        assert_eq!(allowed, 10, "exactly the limit may pass, whatever the interleaving");
    }
}
