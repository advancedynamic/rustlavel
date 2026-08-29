//! The [`Cache`] contract every driver implements.
//!
//! The trait is deliberately dyn-compatible: the factory in [`crate::config`]
//! decides at boot which driver an application uses, so the rest of the
//! framework has to be able to hold an `Arc<dyn Cache>` without knowing which
//! one it got. That is why every method returns a [`BoxFuture`] instead of
//! being an `async fn` — `async fn` in a trait is not dyn-compatible.
//!
//! The generic conveniences that *cannot* be dyn-compatible (`remember`, which
//! takes a closure returning a future) live in [`CacheExt`], blanket-implemented
//! for every `Cache` including `dyn Cache`, so callers never notice the split.

use rustlavel_core::events::{self, Event};
use rustlavel_core::{Json, Result};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// A boxed future borrowed from the cache and its key arguments.
///
/// Mirrors `rustlavel_http::handler::BoxFuture`, but borrowing rather than
/// `'static`, so a driver can hold a lock guard or a pooled connection across
/// the await without cloning the key first.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A cache backend.
///
/// Values are [`Json`] rather than a generic `T` for two reasons: it keeps the
/// trait dyn-compatible, and every rustlavel package already speaks `Json`, so
/// anything that can be serialised can be cached without a second trait.
pub trait Cache: Send + Sync + 'static {
    /// The driver's name, used in `cache.hit` / `cache.miss` events and in
    /// error messages. Telescope shows it as the store column.
    fn driver(&self) -> &'static str;

    /// Fetch a value, or `None` when it is missing or expired.
    ///
    /// Expiry is checked on read in every driver, so an entry that nobody asks
    /// for again is never reported as present, even before a sweep removes it.
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Json>>>;

    /// Store a value that expires after `ttl`.
    ///
    /// A zero or negative `ttl` is treated as "already expired": the key is
    /// forgotten rather than stored, which matches Laravel and avoids leaving
    /// an entry behind that no read will ever return.
    fn put<'a>(&'a self, key: &'a str, value: Json, ttl: Duration) -> BoxFuture<'a, Result<()>>;

    /// Store a value with no expiry. It still goes away on [`Cache::flush`].
    fn forever<'a>(&'a self, key: &'a str, value: Json) -> BoxFuture<'a, Result<()>>;

    /// Remove a key. Returns whether something was actually there.
    fn forget<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>>;

    /// Remove everything this store owns.
    fn flush(&self) -> BoxFuture<'_, Result<()>>;

    /// Whether a live (unexpired) value exists.
    fn has<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { Ok(self.get(key).await?.is_some()) })
    }

    /// Add `by` to a counter, creating it at zero first. Returns the new value.
    ///
    /// Counters are stored as plain JSON numbers so `get` on a counter returns
    /// something sensible in every driver.
    fn increment<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>>;

    /// Subtract `by` from a counter. Returns the new value.
    fn decrement<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move { self.increment(key, -by).await })
    }

    /// Increment a counter, giving it `ttl` only when this call created it.
    ///
    /// Rate limiting needs exactly this and nothing weaker: the first request
    /// of a window starts the clock, and the ninety-ninth must not restart it.
    /// Built as a driver method rather than a `get`-then-`put` in the limiter
    /// because a read-modify-write loses counts under concurrency, which is the
    /// one thing a rate limiter may not do.
    fn increment_within<'a>(
        &'a self,
        key: &'a str,
        by: i64,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<i64>>;

    /// How long until a key expires: `None` when it is missing or immortal.
    ///
    /// The rate limiter reports this as `Retry-After`, so a driver that cannot
    /// answer would force the caller to guess.
    fn ttl<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Duration>>>;

    /// Read a value and remove it in one call — Laravel's `Cache::pull`.
    fn pull<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Json>>> {
        Box::pin(async move {
            let value = self.get(key).await?;
            if value.is_some() {
                self.forget(key).await?;
            }
            Ok(value)
        })
    }
}

/// The conveniences that take a closure, and so cannot live on a dyn-compatible
/// trait. Blanket-implemented, including for `dyn Cache`.
pub trait CacheExt: Cache {
    /// Return the cached value, or compute it, store it for `ttl`, and return it.
    ///
    /// The closure runs only on a miss. Note that two concurrent misses both
    /// compute: this is a cache, not a lock, and stampede protection would mean
    /// holding a lock across arbitrary user code.
    fn remember<'a, F, Fut>(
        &'a self,
        key: &'a str,
        ttl: Duration,
        compute: F,
    ) -> impl Future<Output = Result<Json>> + Send + 'a
    where
        F: FnOnce() -> Fut + Send + 'a,
        Fut: Future<Output = Result<Json>> + Send + 'a,
    {
        async move {
            if let Some(hit) = self.get(key).await? {
                return Ok(hit);
            }
            let value = compute().await?;
            self.put(key, value.clone(), ttl).await?;
            Ok(value)
        }
    }

    /// [`CacheExt::remember`] with no expiry.
    fn remember_forever<'a, F, Fut>(
        &'a self,
        key: &'a str,
        compute: F,
    ) -> impl Future<Output = Result<Json>> + Send + 'a
    where
        F: FnOnce() -> Fut + Send + 'a,
        Fut: Future<Output = Result<Json>> + Send + 'a,
    {
        async move {
            if let Some(hit) = self.get(key).await? {
                return Ok(hit);
            }
            let value = compute().await?;
            self.forever(key, value.clone()).await?;
            Ok(value)
        }
    }
}

impl<T: Cache + ?Sized> CacheExt for T {}

/// Report a lookup on the instrumentation bus.
///
/// Guarded by `has_subscribers` so an application with no Telescope never pays
/// for the string allocations behind an event nobody reads.
pub(crate) fn record(hit: bool, driver: &'static str, key: &str) {
    if !events::has_subscribers() {
        return;
    }
    let kind = if hit { "cache.hit" } else { "cache.miss" };
    Event::new(kind).with("key", key).with("store", driver).dispatch();
}

/// Turn a stored payload back into a value, treating corruption as a miss.
///
/// A cache is by definition disposable, so an unreadable entry must never take
/// the application down: the caller simply recomputes.
pub(crate) fn decode(payload: &str) -> Option<Json> {
    Json::parse(payload).ok()
}

/// Read a counter out of whatever the key currently holds.
///
/// A non-numeric value counts as zero rather than an error: `increment` on a
/// key someone else used for a string should start counting, not explode.
pub(crate) fn counter_value(value: Option<&Json>) -> i64 {
    match value {
        Some(Json::Number(n)) => *n as i64,
        Some(Json::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// Prefixes are applied by the driver rather than by a wrapper so that a
/// driver's native operations (Redis `INCRBY`, a file name) see the final key.
pub(crate) fn prefixed(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        return key.to_string();
    }
    format!("{prefix}{key}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_reads_numbers_strings_and_nothing_at_all() {
        assert_eq!(counter_value(Some(&Json::from(7))), 7);
        assert_eq!(counter_value(Some(&Json::from("12"))), 12);
        assert_eq!(counter_value(Some(&Json::Null)), 0);
        assert_eq!(counter_value(None), 0);
        assert_eq!(counter_value(Some(&Json::from("not a number"))), 0);
    }

    #[test]
    fn a_corrupt_payload_decodes_as_a_miss_rather_than_an_error() {
        assert_eq!(decode("42"), Some(Json::from(42)));
        assert_eq!(decode("{oops"), None);
    }

    #[test]
    fn an_empty_prefix_leaves_the_key_untouched() {
        assert_eq!(prefixed("", "users:1"), "users:1");
        assert_eq!(prefixed("app:", "users:1"), "app:users:1");
    }
}
