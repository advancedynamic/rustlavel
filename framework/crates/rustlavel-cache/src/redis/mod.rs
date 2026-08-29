//! A Redis client written from scratch on Tokio TCP.
//!
//! Three layers, each independently testable:
//!
//! * [`resp`] — the wire format. Pure functions over byte slices, so the
//!   protocol tests need no server at all.
//! * [`connection`] — one socket, the handshake, and command/reply framing.
//! * [`pool`] — a bounded set of connections, discarding any that broke.
//!
//! On top sits [`RedisStore`], the [`Cache`] implementation. It uses only
//! `GET`, `SET`, `DEL`, `EXISTS`, `INCRBY`, `DECRBY`, `FLUSHDB`, `EXPIRE`,
//! `PEXPIRE`, `PTTL`, `PING` and `AUTH`/`SELECT` — a small enough surface that
//! it also works against the Redis-compatible servers (Valkey, KeyDB,
//! Dragonfly) people actually deploy.

pub mod config;
pub mod connection;
pub mod pool;
pub mod resp;

pub use config::RedisConfig;
pub use connection::Connection;
pub use pool::{Pool, PooledConnection};
pub use resp::Value;

use crate::store::{BoxFuture, Cache, decode, prefixed, record};
use rustlavel_core::{Error, Json, Result};
use std::time::Duration;

/// A cache backed by Redis.
///
/// Cloning shares one pool, so this can be registered as application state.
#[derive(Clone)]
pub struct RedisStore {
    pool: Pool,
    prefix: String,
}

impl RedisStore {
    /// Build a store from a URL: `redis://[:password@]host:port[/db]`.
    pub fn connect(url: &str) -> Result<Self> {
        Ok(RedisStore::new(RedisConfig::from_url(url)?, ""))
    }

    pub fn new(config: RedisConfig, prefix: impl Into<String>) -> Self {
        RedisStore { pool: Pool::new(config), prefix: prefix.into() }
    }

    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Open a connection now, so a bad URL or a wrong password surfaces at boot.
    pub async fn verify(&self) -> Result<()> {
        self.pool.verify().await
    }

    /// `PING`. Returns `PONG`, and is the cheapest liveness check there is.
    pub async fn ping(&self) -> Result<String> {
        let reply = self.pool.command(&[b"PING"]).await?.into_result()?;
        reply
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::msg("Redis answered PING with something other than a status"))
    }

    /// `EXPIRE key seconds` — whole-second precision, which is what the Redis
    /// command itself offers. Returns whether the key existed.
    pub async fn expire(&self, key: &str, seconds: u64) -> Result<bool> {
        let full = prefixed(&self.prefix, key);
        let seconds = seconds.to_string();
        let reply = self
            .pool
            .command(&[b"EXPIRE", full.as_bytes(), seconds.as_bytes()])
            .await?
            .into_result()?;
        Ok(reply.as_i64() == Some(1))
    }

    /// `PEXPIRE key milliseconds`, for the sub-second windows a rate limiter
    /// wants and `EXPIRE` cannot express.
    pub async fn pexpire(&self, key: &str, ttl: Duration) -> Result<bool> {
        let full = prefixed(&self.prefix, key);
        let millis = (ttl.as_millis() as u64).max(1).to_string();
        let reply = self
            .pool
            .command(&[b"PEXPIRE", full.as_bytes(), millis.as_bytes()])
            .await?
            .into_result()?;
        Ok(reply.as_i64() == Some(1))
    }

    /// Run an arbitrary command. The escape hatch for anything this crate does
    /// not wrap; arguments are still length-prefixed, so it cannot be injected.
    pub async fn command(&self, args: &[&[u8]]) -> Result<Value> {
        self.pool.command(args).await?.into_result()
    }

    async fn integer(&self, args: &[&[u8]]) -> Result<i64> {
        let reply = self.pool.command(args).await?.into_result()?;
        reply.as_i64().ok_or_else(|| {
            Error::msg(format!("expected an integer reply from Redis, got {reply:?}"))
        })
    }
}

impl Cache for RedisStore {
    fn driver(&self) -> &'static str {
        "redis"
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Json>>> {
        Box::pin(async move {
            let full = prefixed(&self.prefix, key);
            let reply = self.pool.command(&[b"GET", full.as_bytes()]).await?.into_result()?;

            // Redis expires keys for us, so a nil reply is the whole miss path.
            let found = reply.as_str().and_then(decode);
            record(found.is_some(), "redis", key);
            Ok(found)
        })
    }

    fn put<'a>(&'a self, key: &'a str, value: Json, ttl: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let full = prefixed(&self.prefix, key);
            if ttl.is_zero() {
                self.pool.command(&[b"DEL", full.as_bytes()]).await?.into_result()?;
                return Ok(());
            }

            // PX rather than EX: a caller asking for 500ms should get 500ms,
            // not a second rounded either way.
            let millis = (ttl.as_millis() as u64).max(1).to_string();
            let payload = value.to_string();
            self.pool
                .command(&[b"SET", full.as_bytes(), payload.as_bytes(), b"PX", millis.as_bytes()])
                .await?
                .into_result()?;
            Ok(())
        })
    }

    fn forever<'a>(&'a self, key: &'a str, value: Json) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let full = prefixed(&self.prefix, key);
            let payload = value.to_string();
            self.pool
                .command(&[b"SET", full.as_bytes(), payload.as_bytes()])
                .await?
                .into_result()?;
            Ok(())
        })
    }

    fn forget<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let full = prefixed(&self.prefix, key);
            Ok(self.integer(&[b"DEL", full.as_bytes()]).await? > 0)
        })
    }

    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // `FLUSHDB` empties the whole database, prefix or no prefix — there
            // is no server-side "delete by prefix" that is safe on a large
            // keyspace (`KEYS` blocks the server). Give a cache that shares a
            // Redis with anything else its own database number.
            self.pool.command(&[b"FLUSHDB"]).await?.into_result()?;
            Ok(())
        })
    }

    fn has<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let full = prefixed(&self.prefix, key);
            Ok(self.integer(&[b"EXISTS", full.as_bytes()]).await? > 0)
        })
    }

    fn increment<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            let full = prefixed(&self.prefix, key);
            let by = by.to_string();
            self.integer(&[b"INCRBY", full.as_bytes(), by.as_bytes()]).await
        })
    }

    fn decrement<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            let full = prefixed(&self.prefix, key);
            let by = by.to_string();
            self.integer(&[b"DECRBY", full.as_bytes(), by.as_bytes()]).await
        })
    }

    fn increment_within<'a>(
        &'a self,
        key: &'a str,
        by: i64,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            let full = prefixed(&self.prefix, key);
            let millis = (ttl.as_millis() as u64).max(1).to_string();

            // `SET key 0 PX ttl NX` creates the counter *with* its window and
            // does nothing at all if it already exists, so a later request in
            // the same window cannot extend it. Then `INCRBY` counts. Two
            // round trips, no lost TTL, and no read-modify-write race: the
            // `NX` and the `INCRBY` are each atomic on the server.
            self.pool
                .command(&[b"SET", full.as_bytes(), b"0", b"PX", millis.as_bytes(), b"NX"])
                .await?
                .into_result()?;

            let by = by.to_string();
            self.integer(&[b"INCRBY", full.as_bytes(), by.as_bytes()]).await
        })
    }

    fn ttl<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Duration>>> {
        Box::pin(async move {
            let full = prefixed(&self.prefix, key);
            // PTTL answers -2 for a missing key and -1 for one with no expiry;
            // both mean "no deadline to report".
            let millis = self.integer(&[b"PTTL", full.as_bytes()]).await?;
            Ok((millis >= 0).then(|| Duration::from_millis(millis as u64)))
        })
    }
}
