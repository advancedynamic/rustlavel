//! The in-memory driver: the default, and the one tests use.
//!
//! Entries live in a fixed set of shards, each behind its own `RwLock`. One
//! global lock would serialise every cache read in the process — which for a
//! web server means serialising every request — while sharding means two keys
//! that hash differently never wait for each other.
//!
//! Expired entries are removed on read (so a stale value is never returned even
//! between sweeps) and by a periodic background sweep (so a key that is written
//! once and never read again does not leak memory forever).

use crate::store::{BoxFuture, Cache, counter_value, prefixed, record};
use rustlavel_core::{Json, Result};
use std::collections::HashMap;
use std::collections::hash_map::Entry as MapEntry;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, RwLock, Weak};
use std::time::{Duration, Instant};

/// How many shards the key space is split across. A power of two so the index
/// is a mask; sixteen is comfortably more than the core count of a typical
/// deployment without wasting memory on empty maps.
const SHARDS: usize = 16;

/// How often the background task removes entries nobody read.
const DEFAULT_SWEEP: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
struct Entry {
    value: Json,
    /// `None` means the entry was stored with `forever`.
    expires_at: Option<Instant>,
}

impl Entry {
    fn is_expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }
}

struct Inner {
    shards: Vec<RwLock<HashMap<String, Entry>>>,
    prefix: String,
}

impl Inner {
    fn shard(&self, key: &str) -> &RwLock<HashMap<String, Entry>> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        &self.shards[(hasher.finish() as usize) % SHARDS]
    }

    /// Drop every expired entry. Called by the background sweep.
    fn sweep(&self) {
        let now = Instant::now();
        for shard in &self.shards {
            let mut map = shard.write().expect("cache shard poisoned");
            map.retain(|_, entry| !entry.is_expired(now));
        }
    }

    fn live_count(&self) -> usize {
        let now = Instant::now();
        self.shards
            .iter()
            .map(|shard| {
                shard
                    .read()
                    .expect("cache shard poisoned")
                    .values()
                    .filter(|entry| !entry.is_expired(now))
                    .count()
            })
            .sum()
    }
}

/// A process-local cache. Cloning shares one store, so it can be registered as
/// application state and handed to every handler.
#[derive(Clone)]
pub struct MemoryStore {
    inner: Arc<Inner>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        MemoryStore::new()
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        MemoryStore::with_options("", DEFAULT_SWEEP)
    }

    pub fn with_prefix(prefix: impl Into<String>) -> Self {
        MemoryStore::with_options(prefix, DEFAULT_SWEEP)
    }

    /// Build a store with an explicit sweep interval.
    ///
    /// The sweep is spawned only when a Tokio runtime is already running, so
    /// constructing a cache during boot — before the runtime exists — works and
    /// simply relies on lazy eviction until the process starts serving.
    pub fn with_options(prefix: impl Into<String>, sweep_every: Duration) -> Self {
        let inner = Arc::new(Inner {
            shards: (0..SHARDS).map(|_| RwLock::new(HashMap::new())).collect(),
            prefix: prefix.into(),
        });

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            // Weak, so the sweep never keeps a dropped cache alive; the task
            // ends by itself the first time it wakes up after the last clone
            // goes away.
            let weak: Weak<Inner> = Arc::downgrade(&inner);
            handle.spawn(async move {
                loop {
                    tokio::time::sleep(sweep_every).await;
                    match weak.upgrade() {
                        Some(inner) => inner.sweep(),
                        None => break,
                    }
                }
            });
        }

        MemoryStore { inner }
    }

    /// Remove expired entries now. The background sweep calls this; tests call
    /// it directly rather than waiting a minute.
    pub fn sweep(&self) {
        self.inner.sweep();
    }

    /// How many unexpired entries are held. For tests and diagnostics.
    pub fn len(&self) -> usize {
        self.inner.live_count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The one place a key is read, so lazy eviction cannot be forgotten.
    fn read(&self, key: &str) -> Option<Json> {
        let shard = self.inner.shard(key);
        let now = Instant::now();

        // Fast path under a read lock: the common case is a live entry.
        {
            let map = shard.read().expect("cache shard poisoned");
            match map.get(key) {
                None => return None,
                Some(entry) if !entry.is_expired(now) => return Some(entry.value.clone()),
                Some(_) => {}
            }
        }

        // Expired: take the write lock to evict, re-checking in case another
        // task replaced the entry with a fresh one in between.
        let mut map = shard.write().expect("cache shard poisoned");
        match map.get(key) {
            Some(entry) if entry.is_expired(now) => {
                map.remove(key);
                None
            }
            Some(entry) => Some(entry.value.clone()),
            None => None,
        }
    }

    fn write(&self, key: String, value: Json, expires_at: Option<Instant>) {
        self.inner
            .shard(&key)
            .write()
            .expect("cache shard poisoned")
            .insert(key, Entry { value, expires_at });
    }

    fn remove(&self, key: &str) -> bool {
        let now = Instant::now();
        self.inner
            .shard(key)
            .write()
            .expect("cache shard poisoned")
            .remove(key)
            // Removing an entry that had already expired is not a removal: the
            // caller should see the same `false` a later read would imply.
            .is_some_and(|entry| !entry.is_expired(now))
    }

    /// Increment under one write lock, which is what makes this driver safe for
    /// the rate limiter: no window exists in which two tasks read the same value.
    fn bump(&self, key: String, by: i64, ttl: Option<Duration>) -> i64 {
        let now = Instant::now();
        let mut map = self.inner.shard(&key).write().expect("cache shard poisoned");

        match map.entry(key) {
            MapEntry::Occupied(mut slot) if !slot.get().is_expired(now) => {
                let next = counter_value(Some(&slot.get().value)) + by;
                slot.get_mut().value = Json::from(next);
                next
            }
            MapEntry::Occupied(mut slot) => {
                // Expired: this call is creating the entry, so it owns the TTL.
                slot.insert(Entry { value: Json::from(by), expires_at: ttl.map(|d| now + d) });
                by
            }
            MapEntry::Vacant(slot) => {
                slot.insert(Entry { value: Json::from(by), expires_at: ttl.map(|d| now + d) });
                by
            }
        }
    }
}

impl Cache for MemoryStore {
    fn driver(&self) -> &'static str {
        "memory"
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Json>>> {
        Box::pin(async move {
            let full = prefixed(&self.inner.prefix, key);
            let found = self.read(&full);
            record(found.is_some(), "memory", key);
            Ok(found)
        })
    }

    fn put<'a>(&'a self, key: &'a str, value: Json, ttl: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let full = prefixed(&self.inner.prefix, key);
            if ttl.is_zero() {
                self.remove(&full);
                return Ok(());
            }
            self.write(full, value, Some(Instant::now() + ttl));
            Ok(())
        })
    }

    fn forever<'a>(&'a self, key: &'a str, value: Json) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.write(prefixed(&self.inner.prefix, key), value, None);
            Ok(())
        })
    }

    fn forget<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { Ok(self.remove(&prefixed(&self.inner.prefix, key))) })
    }

    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            for shard in &self.inner.shards {
                shard.write().expect("cache shard poisoned").clear();
            }
            Ok(())
        })
    }

    fn increment<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move { Ok(self.bump(prefixed(&self.inner.prefix, key), by, None)) })
    }

    fn increment_within<'a>(
        &'a self,
        key: &'a str,
        by: i64,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move { Ok(self.bump(prefixed(&self.inner.prefix, key), by, Some(ttl))) })
    }

    fn ttl<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Duration>>> {
        Box::pin(async move {
            let full = prefixed(&self.inner.prefix, key);
            let now = Instant::now();
            let map = self.inner.shard(&full).read().expect("cache shard poisoned");
            Ok(map
                .get(&full)
                .filter(|entry| !entry.is_expired(now))
                .and_then(|entry| entry.expires_at)
                .map(|at| at.saturating_duration_since(now)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::CacheExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn an_expired_entry_is_evicted_the_moment_it_is_read() {
        let cache = MemoryStore::new();
        cache.put("temporary", Json::from("here"), Duration::from_millis(30)).await.unwrap();
        assert_eq!(cache.len(), 1);

        tokio::time::sleep(Duration::from_millis(60)).await;

        assert_eq!(cache.get("temporary").await.unwrap(), None);
        assert_eq!(cache.len(), 0, "the read should have dropped the entry, not just hidden it");
    }

    #[tokio::test]
    async fn a_sweep_removes_entries_nobody_ever_reads_again() {
        let cache = MemoryStore::new();
        for i in 0..50 {
            cache
                .put(&format!("key-{i}"), Json::from(i), Duration::from_millis(20))
                .await
                .unwrap();
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
        cache.sweep();

        // Nothing read them, so only the sweep could have freed the memory.
        let raw: usize = cache.inner.shards.iter().map(|s| s.read().unwrap().len()).sum();
        assert_eq!(raw, 0);
    }

    #[tokio::test]
    async fn the_background_sweep_stops_when_the_last_clone_is_dropped() {
        let cache = MemoryStore::with_options("", Duration::from_millis(5));
        let weak = Arc::downgrade(&cache.inner);
        drop(cache);

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(weak.strong_count(), 0, "the sweep task must not keep the cache alive");
    }

    #[tokio::test]
    async fn a_prefix_keeps_two_stores_from_colliding() {
        let alpha = MemoryStore::with_prefix("alpha:");
        let beta = MemoryStore::with_prefix("beta:");

        alpha.forever("shared", Json::from(1)).await.unwrap();
        beta.forever("shared", Json::from(2)).await.unwrap();

        assert_eq!(alpha.get("shared").await.unwrap(), Some(Json::from(1)));
        assert_eq!(beta.get("shared").await.unwrap(), Some(Json::from(2)));
    }

    #[tokio::test]
    async fn concurrent_increments_never_lose_a_count() {
        let cache = MemoryStore::new();
        let cache = Arc::new(cache);

        let mut tasks = Vec::new();
        for _ in 0..32 {
            let cache = Arc::clone(&cache);
            tasks.push(tokio::spawn(async move {
                for _ in 0..100 {
                    cache.increment("hits", 1).await.unwrap();
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(cache.get("hits").await.unwrap(), Some(Json::from(3200)));
    }

    #[tokio::test]
    async fn hammering_mixed_operations_from_many_tasks_stays_consistent() {
        let cache = Arc::new(MemoryStore::new());
        let computed = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for task_id in 0..24 {
            let cache = Arc::clone(&cache);
            let computed = Arc::clone(&computed);
            tasks.push(tokio::spawn(async move {
                for round in 0..80 {
                    let key = format!("k-{}", (task_id * round) % 40);
                    match round % 4 {
                        0 => {
                            cache.put(&key, Json::from(round), Duration::from_millis(5)).await.unwrap();
                        }
                        1 => {
                            let _ = cache.get(&key).await.unwrap();
                        }
                        2 => {
                            let _ = cache.forget(&key).await.unwrap();
                        }
                        _ => {
                            let counter = Arc::clone(&computed);
                            let value = cache
                                .remember(&key, Duration::from_millis(5), move || async move {
                                    counter.fetch_add(1, Ordering::SeqCst);
                                    Ok(Json::from("computed"))
                                })
                                .await
                                .unwrap();
                            assert!(!value.is_null());
                        }
                    }
                }
            }));
        }

        for task in tasks {
            task.await.unwrap();
        }

        // Everything expires within milliseconds, so after a sweep the store
        // must come back to empty rather than growing without bound.
        tokio::time::sleep(Duration::from_millis(30)).await;
        cache.sweep();
        assert_eq!(cache.len(), 0);
    }

    #[tokio::test]
    async fn increment_within_starts_the_clock_only_on_the_first_call() {
        let cache = MemoryStore::new();

        assert_eq!(cache.increment_within("window", 1, Duration::from_millis(120)).await.unwrap(), 1);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(cache.increment_within("window", 1, Duration::from_millis(120)).await.unwrap(), 2);

        // If the second call had reset the TTL the counter would still be here.
        tokio::time::sleep(Duration::from_millis(90)).await;
        assert_eq!(cache.get("window").await.unwrap(), None);
    }

    #[tokio::test]
    async fn ttl_reports_the_remaining_life_of_a_key() {
        let cache = MemoryStore::new();
        cache.forever("immortal", Json::from(1)).await.unwrap();
        cache.put("mortal", Json::from(1), Duration::from_secs(30)).await.unwrap();

        assert_eq!(cache.ttl("immortal").await.unwrap(), None);
        assert_eq!(cache.ttl("nothing").await.unwrap(), None);
        let remaining = cache.ttl("mortal").await.unwrap().expect("a mortal key has a ttl");
        assert!(remaining <= Duration::from_secs(30) && remaining > Duration::from_secs(25));
    }

    #[tokio::test]
    async fn putting_with_a_zero_ttl_forgets_instead_of_storing() {
        let cache = MemoryStore::new();
        cache.forever("doomed", Json::from(1)).await.unwrap();
        cache.put("doomed", Json::from(2), Duration::ZERO).await.unwrap();

        assert!(!cache.has("doomed").await.unwrap());
    }
}
