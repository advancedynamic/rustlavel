//! The connection pool.
//!
//! Opening a PostgreSQL connection costs a TCP handshake plus authentication,
//! which is far too much to pay per request. The pool keeps a small set alive
//! and hands them out, discarding any that broke or that a handler left inside
//! a transaction.

use crate::config::DatabaseConfig;
use crate::postgres::Connection;
use rustlavel_core::{Error, Result};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

struct Inner {
    config: DatabaseConfig,
    idle: Mutex<VecDeque<Connection>>,
    /// Bounds how many connections exist at once, including those in use.
    permits: Arc<Semaphore>,
}

#[derive(Clone)]
pub struct Pool {
    inner: Arc<Inner>,
}

impl Pool {
    /// Create a pool. No connection is opened until one is needed, so an
    /// application still boots when the database is briefly unavailable.
    pub fn new(config: DatabaseConfig) -> Self {
        let permits = Arc::new(Semaphore::new(config.max_connections));
        Pool {
            inner: Arc::new(Inner {
                config,
                idle: Mutex::new(VecDeque::new()),
                permits,
            }),
        }
    }

    /// Open one connection immediately, so a misconfiguration is reported at
    /// boot rather than on the first request.
    pub async fn verify(&self) -> Result<()> {
        let mut connection = self.acquire().await?;
        connection.simple_query("select 1").await?;
        Ok(())
    }

    pub fn config(&self) -> &DatabaseConfig {
        &self.inner.config
    }

    /// Take a connection, opening one if none is idle.
    pub async fn acquire(&self) -> Result<PooledConnection> {
        let permit = Arc::clone(&self.inner.permits)
            .acquire_owned()
            .await
            .map_err(|_| Error::msg("the database pool has been closed"))?;

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

    /// How many connections are currently idle. Used by tests and diagnostics.
    pub async fn idle_count(&self) -> usize {
        self.inner.idle.lock().await.len()
    }

    /// Close every idle connection.
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

        // A broken connection, or one still inside a transaction, must not go
        // back into rotation: the next borrower would inherit the mess.
        if connection.is_broken() || connection.in_transaction() {
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

    #[tokio::test]
    async fn a_pool_opens_nothing_until_it_is_used() {
        let pool = Pool::new(DatabaseConfig { port: 1, ..DatabaseConfig::default() });
        assert_eq!(pool.idle_count().await, 0);
    }

    #[tokio::test]
    async fn acquiring_reports_a_connection_failure() {
        let pool = Pool::new(DatabaseConfig { port: 1, ..DatabaseConfig::default() });
        assert!(pool.acquire().await.is_err());
    }
}
