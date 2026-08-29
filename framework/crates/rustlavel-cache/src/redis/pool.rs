//! A small connection pool.
//!
//! A cache read is supposed to be cheaper than the work it avoids, and a TCP
//! handshake plus `AUTH` plus `SELECT` per read is not. The pool keeps a few
//! connections alive and hands them out, discarding any whose framing may have
//! drifted — see [`super::connection::Connection::is_broken`].

use super::config::RedisConfig;
use super::connection::Connection;
use rustlavel_core::{Error, Result};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

struct Inner {
    config: RedisConfig,
    idle: Mutex<VecDeque<Connection>>,
    /// Bounds how many connections exist at once, including those in use, so a
    /// traffic spike cannot open a thousand sockets against the cache.
    permits: Arc<Semaphore>,
}

#[derive(Clone)]
pub struct Pool {
    inner: Arc<Inner>,
}

impl Pool {
    /// Create a pool. Nothing is connected until the first command, so an
    /// application still boots when Redis is briefly unavailable — a cache
    /// being down should degrade a service, not stop it starting.
    pub fn new(config: RedisConfig) -> Self {
        let permits = Arc::new(Semaphore::new(config.max_connections.max(1)));
        Pool {
            inner: Arc::new(Inner { config, idle: Mutex::new(VecDeque::new()), permits }),
        }
    }

    pub fn config(&self) -> &RedisConfig {
        &self.inner.config
    }

    /// Open one connection immediately, so a misconfiguration is reported at
    /// boot rather than on the first cache miss in production.
    pub async fn verify(&self) -> Result<()> {
        let mut connection = self.acquire().await?;
        connection.command(&[b"PING"]).await?.into_result()?;
        Ok(())
    }

    pub async fn acquire(&self) -> Result<PooledConnection> {
        let permit = Arc::clone(&self.inner.permits)
            .acquire_owned()
            .await
            .map_err(|_| Error::msg("the Redis pool has been closed"))?;

        if let Some(connection) = self.inner.idle.lock().await.pop_front() {
            return Ok(PooledConnection {
                connection: Some(connection),
                pool: Arc::clone(&self.inner),
                _permit: permit,
            });
        }

        let connection = Connection::connect(&self.inner.config).await?;
        Ok(PooledConnection {
            connection: Some(connection),
            pool: Arc::clone(&self.inner),
            _permit: permit,
        })
    }

    /// Run one command on a borrowed connection.
    pub async fn command(&self, args: &[&[u8]]) -> Result<super::resp::Value> {
        let mut connection = self.acquire().await?;
        connection.command(args).await
    }

    /// How many connections are idle. For tests and diagnostics.
    pub async fn idle_count(&self) -> usize {
        self.inner.idle.lock().await.len()
    }

    pub async fn close(&self) {
        let mut idle = self.inner.idle.lock().await;
        while let Some(connection) = idle.pop_front() {
            connection.close().await;
        }
    }
}

/// A connection borrowed from the pool, returned when dropped.
pub struct PooledConnection {
    connection: Option<Connection>,
    pool: Arc<Inner>,
    /// Held for the lifetime of the borrow; releasing it lets another caller in.
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl std::ops::Deref for PooledConnection {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.connection.as_ref().expect("connection is present until drop")
    }
}

impl std::ops::DerefMut for PooledConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        self.connection.as_mut().expect("connection is present until drop")
    }
}

impl Drop for PooledConnection {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else { return };

        if connection.is_broken() {
            // Returning this would hand the next borrower somebody else's
            // reply. Closing it costs one socket; reusing it costs correctness.
            tokio::spawn(async move { connection.close().await });
            return;
        }

        let pool = Arc::clone(&self.pool);
        tokio::spawn(async move {
            pool.idle.lock().await.push_back(connection);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_pool_opens_nothing_until_it_is_used() {
        let pool = Pool::new(RedisConfig { port: 1, ..RedisConfig::default() });
        assert_eq!(pool.idle_count().await, 0);
    }

    #[tokio::test]
    async fn acquiring_reports_a_connection_failure_rather_than_hanging() {
        let pool = Pool::new(RedisConfig {
            port: 1,
            connect_timeout: Duration::from_secs(2),
            ..RedisConfig::default()
        });
        assert!(pool.acquire().await.is_err());
        assert!(pool.verify().await.is_err());
    }
}
