//! Integration tests against a real Redis server.
//!
//! They run only when `REDIS_URL` is set, so `cargo test` stays green on a
//! machine with no Redis. Start one with:
//!
//! ```text
//! docker run -d --name rustlavel-redis -p 56379:6379 redis:7
//! export REDIS_URL=redis://127.0.0.1:56379
//! ```
//!
//! Every test uses its own key prefix *and* its own Redis database number, so
//! they can run concurrently: `flush` is `FLUSHDB`, and two tests flushing the
//! same database would delete each other's keys.

use rustlavel_cache::redis::RedisConfig;
use rustlavel_cache::{Cache, CacheExt, Json, RedisStore};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// Hands out a distinct database number per test within this binary. Redis
/// ships with sixteen; the suite stays well inside that.
static NEXT_DATABASE: AtomicU32 = AtomicU32::new(1);

/// Connect to the configured server, or skip the test saying so out loud.
macro_rules! redis {
    () => {
        match std::env::var("REDIS_URL") {
            Ok(url) if !url.is_empty() => {
                let mut config = match RedisConfig::from_url(&url) {
                    Ok(config) => config,
                    Err(e) => panic!("REDIS_URL is set but could not be parsed: {e}"),
                };
                config.database = NEXT_DATABASE.fetch_add(1, Ordering::SeqCst) % 16;

                let store = RedisStore::new(config, "");
                if let Err(e) = store.verify().await {
                    panic!("REDIS_URL is set but connecting failed: {e}");
                }
                store.flush().await.expect("a fresh database");
                store
            }
            _ => {
                eprintln!("skipped: set REDIS_URL to run the Redis integration tests");
                return;
            }
        }
    };
}

#[tokio::test]
async fn pings_a_live_server() {
    let cache = redis!();
    assert_eq!(cache.ping().await.unwrap(), "PONG");
}

#[tokio::test]
async fn connects_straight_from_a_url() {
    let url = match std::env::var("REDIS_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => {
            eprintln!("skipped: set REDIS_URL to run the Redis integration tests");
            return;
        }
    };

    // The one-liner an application actually writes, exercised end to end.
    let cache = RedisStore::connect(&url).unwrap();
    cache.verify().await.unwrap();
    assert_eq!(cache.ping().await.unwrap(), "PONG");
}

/// The same behavioural contract the memory and file drivers satisfy in the
/// crate's own test module, run here against a real server. It lives in two
/// places because an integration test links the crate without `cfg(test)`;
/// keeping them identical is the point.
#[tokio::test]
async fn the_redis_driver_satisfies_the_cache_contract() {
    let cache = redis!();

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

    // TTL actually expires — the server does this one, not us.
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
    cache.remember_forever("remembered-forever", || async { Ok(Json::from(7)) }).await.unwrap();
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
async fn expire_and_pexpire_reach_the_server() {
    let cache = redis!();

    cache.forever("mortal", Json::from(1)).await.unwrap();
    assert_eq!(cache.ttl("mortal").await.unwrap(), None);

    assert!(cache.expire("mortal", 30).await.unwrap());
    let remaining = cache.ttl("mortal").await.unwrap().expect("EXPIRE set a deadline");
    assert!(remaining > Duration::from_secs(25) && remaining <= Duration::from_secs(30));

    assert!(cache.pexpire("mortal", Duration::from_millis(120)).await.unwrap());
    tokio::time::sleep(Duration::from_millis(220)).await;
    assert_eq!(cache.get("mortal").await.unwrap(), None);

    // Both report `false` for a key that is not there.
    assert!(!cache.expire("never-existed", 30).await.unwrap());
    assert!(!cache.pexpire("never-existed", Duration::from_secs(1)).await.unwrap());
}

#[tokio::test]
async fn a_key_containing_protocol_syntax_cannot_become_a_second_command() {
    let cache = redis!();

    // If arguments were concatenated rather than length-prefixed, this key
    // would smuggle a FLUSHALL past the client.
    let hostile = "evil\r\nFLUSHALL\r\nkey with spaces \"and quotes\"";
    cache.forever("bystander", Json::from("still here")).await.unwrap();
    cache.forever(hostile, Json::from("stored")).await.unwrap();

    assert_eq!(cache.get(hostile).await.unwrap(), Some(Json::from("stored")));
    assert_eq!(
        cache.get("bystander").await.unwrap(),
        Some(Json::from("still here")),
        "an injected FLUSHALL would have wiped this"
    );
}

#[tokio::test]
async fn a_prefix_namespaces_every_key() {
    let cache = redis!();
    let config = cache.pool().config().clone();

    let alpha = RedisStore::new(config.clone(), "alpha:");
    let beta = RedisStore::new(config, "beta:");

    alpha.forever("shared", Json::from(1)).await.unwrap();
    beta.forever("shared", Json::from(2)).await.unwrap();

    assert_eq!(alpha.get("shared").await.unwrap(), Some(Json::from(1)));
    assert_eq!(beta.get("shared").await.unwrap(), Some(Json::from(2)));

    // The prefix reaches the raw key too, which is what makes it a namespace.
    assert!(cache.has("alpha:shared").await.unwrap());
    assert!(!cache.has("shared").await.unwrap());
}

#[tokio::test]
async fn many_concurrent_tasks_share_the_pool_without_losing_a_count() {
    let cache = redis!();
    let cache = std::sync::Arc::new(cache);

    let mut tasks = Vec::new();
    for _ in 0..16 {
        let cache = std::sync::Arc::clone(&cache);
        tasks.push(tokio::spawn(async move {
            for _ in 0..25 {
                cache.increment("hits", 1).await.unwrap();
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }

    assert_eq!(cache.get("hits").await.unwrap(), Some(Json::from(400)));
    // More tasks than connections, so connections must have been recycled.
    assert!(cache.pool().idle_count().await > 0, "the pool should be holding idle connections");
}

#[tokio::test]
async fn a_rate_limiter_over_redis_lets_exactly_the_limit_through() {
    use rustlavel_cache::RateLimiter;

    let cache = redis!();
    let limiter = RateLimiter::with_driver(cache);
    let window = Duration::from_secs(60);

    let mut allowed = 0;
    for _ in 0..10 {
        if !limiter.attempt("ada", 4, window).await.unwrap().exceeded {
            allowed += 1;
        }
    }

    assert_eq!(allowed, 4);
    assert!(limiter.too_many("ada", 4, window).await.unwrap());

    limiter.clear("ada", window).await.unwrap();
    assert!(!limiter.attempt("ada", 4, window).await.unwrap().exceeded);
}

#[tokio::test]
async fn a_wrong_password_is_reported_rather_than_hanging() {
    let url = match std::env::var("REDIS_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => {
            eprintln!("skipped: set REDIS_URL to run the Redis integration tests");
            return;
        }
    };

    let mut config = RedisConfig::from_url(&url).unwrap();
    config.password = "definitely-not-the-password".into();

    let store = RedisStore::new(config, "");
    let error = match store.verify().await {
        Ok(()) => panic!("a server with no password should still refuse AUTH"),
        Err(e) => e.to_string(),
    };

    // Redis with no password configured answers `ERR Client sent AUTH, but no
    // password is set`; one with a password answers `WRONGPASS`. Either way the
    // client must surface it, and must not print the attempted password.
    assert!(
        error.contains("rejected authentication") || error.contains("AUTH"),
        "unexpected error: {error}"
    );
    assert!(!error.contains("definitely-not-the-password"));
}
