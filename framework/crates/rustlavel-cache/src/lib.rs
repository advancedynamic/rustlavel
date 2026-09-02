//! rustlavel-cache: caching and rate limiting.
//!
//! One [`Cache`] trait, three drivers, and a rate limiter built on top of it:
//!
//! | driver   | shared between processes | survives a restart | needs |
//! |----------|--------------------------|--------------------|-------|
//! | `memory` | no                       | no                 | nothing |
//! | `file`   | between processes on one host | yes           | a directory |
//! | `redis`  | yes                      | yes                | a Redis server |
//!
//! ```ignore
//! use rustlavel_cache::{Cache, CacheExt, CacheStore, Throttle};
//!
//! let cache = CacheStore::from_config(&config)?;
//!
//! let users = cache
//!     .remember("users:active", Duration::from_secs(300), || async {
//!         Ok(load_active_users().await?)
//!     })
//!     .await?;
//!
//! router.middleware(Throttle::per_minute(&cache, 60));
//! ```
//!
//! The Redis client is written from scratch — RESP encoder and decoder,
//! connection, handshake and pool, all on Tokio's TCP — for the same reason the
//! HTTP server and the PostgreSQL driver are: a framework that owns its wire
//! protocols owns its error messages, its performance and its security
//! posture. See [`redis::resp`] for the protocol itself.
//!
//! Every lookup dispatches `cache.hit` or `cache.miss` on
//! [`rustlavel_core::events`], so Telescope can show a hit rate without this
//! crate knowing Telescope exists. Nothing is built when no subscriber is
//! listening.

pub mod config;
pub mod file;
pub mod idempotency;
pub mod memory;
pub mod rate_limit;
pub mod redis;
pub mod store;
pub mod throttle;

pub use config::{CacheConfig, CacheStore, Driver};
pub use file::FileStore;
pub use idempotency::Idempotency;
pub use memory::MemoryStore;
pub use rate_limit::{RateLimit, RateLimiter};
pub use redis::{RedisConfig, RedisStore};
pub use store::{BoxFuture, Cache, CacheExt};
pub use throttle::Throttle;

pub use rustlavel_core::{Error, Json, Result};

/// What an application importing this crate usually wants.
pub mod prelude {
    pub use crate::{Cache, CacheExt, CacheStore, Idempotency, RateLimiter, Throttle};
    pub use rustlavel_core::{Json, Result};
}

#[cfg(test)]
mod tests {
    //! The behavioural suite every driver must satisfy.
    //!
    //! It is written once against `dyn Cache` and run against each driver, so a
    //! driver cannot quietly disagree with the others about what `forget`
    //! returns or what an expired key looks like. `tests/redis.rs` holds the
    //! Redis driver to the same contract against a live server; it carries its
    //! own copy because an integration test links the crate without `cfg(test)`.

    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    /// Assert the full contract against one driver.
    async fn assert_cache_contract(cache: &dyn Cache) {
        cache.flush().await.unwrap();

        // A miss is None, not an error.
        assert_eq!(cache.get("absent").await.unwrap(), None);
        assert!(!cache.has("absent").await.unwrap());
        assert!(!cache.forget("absent").await.unwrap());

        // put / get round-trips every JSON shape.
        for value in [
            Json::Null,
            Json::from(true),
            Json::from(-17),
            Json::from(1.5),
            Json::from("a string with \" and \\ and \n in it"),
            Json::from(vec![1, 2, 3]),
            Json::object([("nested", Json::object([("deep", Json::from(true))]))]),
        ] {
            cache.put("shape", value.clone(), Duration::from_secs(60)).await.unwrap();
            assert_eq!(cache.get("shape").await.unwrap(), Some(value.clone()), "round trip failed");
        }

        // forever survives without a TTL.
        cache.forever("immortal", Json::from("forever")).await.unwrap();
        assert_eq!(cache.ttl("immortal").await.unwrap(), None);
        assert!(cache.has("immortal").await.unwrap());

        // forget reports whether it removed something.
        assert!(cache.forget("immortal").await.unwrap());
        assert!(!cache.forget("immortal").await.unwrap());

        // TTL actually expires.
        cache.put("brief", Json::from("gone soon"), Duration::from_millis(120)).await.unwrap();
        assert!(cache.has("brief").await.unwrap());
        assert!(cache.ttl("brief").await.unwrap().is_some());
        tokio::time::sleep(Duration::from_millis(220)).await;
        assert_eq!(cache.get("brief").await.unwrap(), None, "the TTL did not expire the key");
        assert!(!cache.has("brief").await.unwrap());

        // increment / decrement.
        assert_eq!(cache.increment("counter", 1).await.unwrap(), 1);
        assert_eq!(cache.increment("counter", 4).await.unwrap(), 5);
        assert_eq!(cache.decrement("counter", 2).await.unwrap(), 3);
        assert_eq!(cache.get("counter").await.unwrap(), Some(Json::from(3)));
        assert_eq!(cache.decrement("fresh-counter", 3).await.unwrap(), -3);

        // increment_within starts the window only once.
        assert_eq!(cache.increment_within("window", 1, Duration::from_secs(60)).await.unwrap(), 1);
        assert_eq!(cache.increment_within("window", 1, Duration::from_secs(60)).await.unwrap(), 2);
        let remaining = cache.ttl("window").await.unwrap().expect("a window has a deadline");
        assert!(remaining <= Duration::from_secs(60));

        // remember computes on a miss and only on a miss.
        let computed = cache
            .remember("remembered", Duration::from_secs(60), || async { Ok(Json::from("first")) })
            .await
            .unwrap();
        assert_eq!(computed, Json::from("first"));

        let cached = cache
            .remember("remembered", Duration::from_secs(60), || async {
                panic!("remember must not recompute a hit")
            })
            .await
            .unwrap();
        assert_eq!(cached, Json::from("first"));

        // remember_forever likewise.
        cache
            .remember_forever("remembered-forever", || async { Ok(Json::from(7)) })
            .await
            .unwrap();
        assert_eq!(cache.ttl("remembered-forever").await.unwrap(), None);

        // pull returns the value and leaves nothing behind.
        assert_eq!(cache.pull("remembered").await.unwrap(), Some(Json::from("first")));
        assert_eq!(cache.pull("remembered").await.unwrap(), None);

        // A zero TTL means "already expired".
        cache.forever("doomed", Json::from(1)).await.unwrap();
        cache.put("doomed", Json::from(2), Duration::ZERO).await.unwrap();
        assert!(!cache.has("doomed").await.unwrap());

        // flush empties everything.
        cache.forever("a", Json::from(1)).await.unwrap();
        cache.forever("b", Json::from(2)).await.unwrap();
        cache.flush().await.unwrap();
        assert_eq!(cache.get("a").await.unwrap(), None);
        assert_eq!(cache.get("b").await.unwrap(), None);
        assert_eq!(cache.get("counter").await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_memory_driver_satisfies_the_cache_contract() {
        assert_cache_contract(&MemoryStore::new()).await;
    }

    #[tokio::test]
    async fn the_file_driver_satisfies_the_cache_contract() {
        // Its own directory: the contract calls `flush`, which would wipe a
        // concurrently running test sharing the same one.
        let directory = std::env::temp_dir()
            .join(format!("rustlavel-cache-contract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);

        assert_cache_contract(&FileStore::new(&directory).unwrap()).await;

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn a_boxed_driver_satisfies_the_contract_too() {
        // Proves the trait really is dyn-compatible end to end, which is what
        // the whole `BoxFuture` return style buys.
        let cache: Arc<dyn Cache> = Arc::new(MemoryStore::new());
        assert_cache_contract(cache.as_ref()).await;
        assert_eq!(cache.driver(), "memory");
    }

    #[tokio::test]
    async fn a_lookup_dispatches_a_hit_or_a_miss_event() {
        use rustlavel_core::events::{self, Event};
        use std::sync::Mutex;

        // The event registry is process-global and tests run concurrently, so
        // this one listens for keys only it uses rather than assuming it is
        // the only thing touching a cache right now.
        let marker = "event-probe:";
        events::clear_subscribers();

        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        events::subscribe(move |event: &Event| {
            let key = event.field("key").and_then(Json::as_str).unwrap_or_default();
            if event.kind.starts_with("cache.") && key.starts_with(marker) {
                sink.lock().unwrap().push((event.kind.to_string(), key.to_string()));
            }
        });

        let cache = MemoryStore::new();
        cache.get("event-probe:missing").await.unwrap();
        cache.forever("event-probe:present", Json::from(1)).await.unwrap();
        cache.get("event-probe:present").await.unwrap();

        // A driver must report its own name, so Telescope can tell stores apart.
        let names: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&names);
        events::subscribe(move |event: &Event| {
            if event.kind == "cache.miss" {
                *slot.lock().unwrap() =
                    event.field("store").and_then(Json::as_str).map(str::to_string);
            }
        });
        cache.get("event-probe:another-miss").await.unwrap();
        let store_name = names.lock().unwrap().clone();

        let recorded = seen.lock().unwrap().clone();
        events::clear_subscribers();

        assert_eq!(store_name.as_deref(), Some("memory"));
        assert_eq!(
            recorded,
            vec![
                ("cache.miss".to_string(), "event-probe:missing".to_string()),
                ("cache.hit".to_string(), "event-probe:present".to_string()),
                ("cache.miss".to_string(), "event-probe:another-miss".to_string()),
            ]
        );
    }
}
