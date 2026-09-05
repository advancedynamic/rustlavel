//! The connection pool.
//!
//! Opening a connection costs a TCP handshake plus authentication, whatever the
//! database, which is far too much to pay per request. The pool keeps a small
//! set alive and hands them out, discarding any that broke or that a handler
//! left inside a transaction.
//!
//! It knows nothing about any particular database: it holds a [`Driver`], and
//! the driver knows the protocol.

use crate::dialect::Dialect;
use crate::driver::{Driver, DriverConnection};
use rustlavel_core::{Error, Result};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

struct Inner {
    driver: Arc<dyn Driver>,
    /// Each idle connection beside the credential generation it was opened
    /// under, so one belonging to a rotated credential can be told apart from
    /// one that is still current.
    idle: Mutex<VecDeque<(u64, Box<dyn DriverConnection>)>>,
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
    pub fn new(driver: Arc<dyn Driver>) -> Self {
        let permits = Arc::new(Semaphore::new(driver.max_connections().max(1)));
        Pool { inner: Arc::new(Inner { driver, idle: Mutex::new(VecDeque::new()), permits }) }
    }

    /// Open one connection immediately, so a misconfiguration is reported at
    /// boot rather than on the first request.
    pub async fn verify(&self) -> Result<()> {
        let mut connection = self.acquire().await?;
        connection.simple_query("select 1").await?;
        Ok(())
    }

    pub fn driver(&self) -> &Arc<dyn Driver> {
        &self.inner.driver
    }

    pub fn dialect(&self) -> Arc<dyn Dialect> {
        self.inner.driver.dialect()
    }

    /// Take a connection, opening one if none is idle.
    pub async fn acquire(&self) -> Result<PooledConnection> {
        let permit = Arc::clone(&self.inner.permits)
            .acquire_owned()
            .await
            .map_err(|_| Error::msg("the database pool has been closed"))?;

        let generation = self.inner.driver.generation();

        // Discard anything opened under a credential that has since been
        // replaced. Such a connection usually still *works* — a database
        // authenticates once, at connect time — which is exactly why it has to
        // be dropped deliberately: left alone it would keep an access granted
        // by a revoked account alive for as long as the process runs.
        loop {
            let Some((opened_under, connection)) = self.inner.idle.lock().await.pop_front() else {
                break;
            };

            if opened_under == generation {
                return Ok(PooledConnection {
                    connection: Some(connection),
                    generation,
                    pool: Arc::clone(&self.inner),
                    _permit: permit,
                });
            }

            connection.close().await;
        }

        let connection = self.inner.driver.connect().await?;
        Ok(PooledConnection {
            connection: Some(connection),
            generation,
            pool: Arc::clone(&self.inner),
            _permit: permit,
        })
    }

    /// How many connections are currently idle. Used by tests and diagnostics.
    pub async fn idle_count(&self) -> usize {
        self.inner.idle.lock().await.len()
    }

    /// How many sockets this pool is holding open: idle plus borrowed.
    ///
    /// The number that matters to the *server*, which is a different number
    /// from the one the semaphore governs. A permit is held only while a
    /// connection is borrowed, so the semaphore caps concurrent use; a
    /// connection handed back sits in `idle` holding no permit and a very much
    /// open socket. An application with one pool never has to care. One with a
    /// pool per tenant does: fifty idle pools are five hundred sockets against
    /// a server whose default limit is a hundred, while every semaphore reads
    /// zero.
    pub async fn open_count(&self) -> usize {
        let borrowed = self
            .inner
            .driver
            .max_connections()
            .max(1)
            .saturating_sub(self.inner.permits.available_permits());
        self.inner.idle.lock().await.len() + borrowed
    }

    /// Close up to `limit` idle connections, returning how many went.
    ///
    /// Only idle ones: a borrowed connection is in the middle of somebody's
    /// query, and taking it away would turn a capacity problem into a failed
    /// request. Freeing what is idle is enough — that is where a pool nobody
    /// is using keeps its sockets.
    pub async fn close_idle(&self, limit: usize) -> usize {
        let mut closed = 0;
        while closed < limit {
            let Some((_, connection)) = self.inner.idle.lock().await.pop_front() else { break };
            connection.close().await;
            closed += 1;
        }
        closed
    }

    /// Close every idle connection.
    pub async fn close(&self) {
        let mut idle = self.inner.idle.lock().await;
        while let Some((_, connection)) = idle.pop_front() {
            connection.close().await;
        }
    }

    /// Close idle connections belonging to a superseded credential, now.
    ///
    /// [`Pool::acquire`] already refuses to hand one out, so this is not needed
    /// for correctness — it is for closing the window between a rotation and
    /// the next request on a quiet pool, where an idle session opened with a
    /// revoked account would otherwise sit there until somebody happened to
    /// need it. Connections currently lent out are left alone; they are retired
    /// when their borrower gives them back.
    ///
    /// Returns how many were closed.
    pub async fn retire_superseded(&self) -> usize {
        let generation = self.inner.driver.generation();
        let mut idle = self.inner.idle.lock().await;

        let mut keeping = VecDeque::with_capacity(idle.len());
        let mut closed = 0;

        while let Some((opened_under, connection)) = idle.pop_front() {
            if opened_under == generation {
                keeping.push_back((opened_under, connection));
            } else {
                connection.close().await;
                closed += 1;
            }
        }

        *idle = keeping;
        closed
    }
}

/// A connection borrowed from the pool, returned when dropped.
pub struct PooledConnection {
    connection: Option<Box<dyn DriverConnection>>,
    /// The credential generation this connection was opened under.
    generation: u64,
    pool: Arc<Inner>,
    /// Held for the lifetime of the borrow; releasing it lets another caller in.
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl std::ops::Deref for PooledConnection {
    type Target = dyn DriverConnection;

    fn deref(&self) -> &(dyn DriverConnection + 'static) {
        self.connection.as_deref().expect("connection is present until drop")
    }
}

impl std::ops::DerefMut for PooledConnection {
    fn deref_mut(&mut self) -> &mut (dyn DriverConnection + 'static) {
        self.connection.as_deref_mut().expect("connection is present until drop")
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

        // A connection opened under a credential that has since been replaced
        // is closed rather than returned: this is the "busy connections go when
        // their borrower is finished" half of the rotation.
        let pool = Arc::clone(&self.pool);
        let generation = self.generation;
        tokio::spawn(async move {
            if generation != pool.driver.generation() {
                connection.close().await;
                return;
            }
            pool.idle.lock().await.push_back((generation, connection));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Postgres;
    use crate::driver::BoxFuture;

    /// A driver that hands out connections and counts how many it opened, with
    /// a generation the test can move on by hand.
    struct Counting {
        opened: Arc<std::sync::atomic::AtomicUsize>,
        closed: Arc<std::sync::atomic::AtomicUsize>,
        generation: Arc<std::sync::atomic::AtomicU64>,
    }

    struct Nothing(Arc<std::sync::atomic::AtomicUsize>);

    impl DriverConnection for Nothing {
        fn query<'a>(
            &'a mut self,
            _sql: &'a str,
            _params: &'a [crate::value::Value],
        ) -> BoxFuture<'a, Result<crate::driver::QueryResult>> {
            Box::pin(async { Err(Error::msg("not a real connection")) })
        }

        fn simple_query<'a>(
            &'a mut self,
            _sql: &'a str,
        ) -> BoxFuture<'a, Result<crate::driver::QueryResult>> {
            Box::pin(async { Err(Error::msg("not a real connection")) })
        }

        fn is_broken(&self) -> bool {
            false
        }

        fn in_transaction(&self) -> bool {
            false
        }

        fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {})
        }
    }

    impl Driver for Counting {
        fn dialect(&self) -> Arc<dyn Dialect> {
            Arc::new(Postgres)
        }

        fn connect(&self) -> BoxFuture<'_, Result<Box<dyn DriverConnection>>> {
            self.opened.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let closed = Arc::clone(&self.closed);
            Box::pin(async move { Ok(Box::new(Nothing(closed)) as Box<dyn DriverConnection>) })
        }

        fn describe(&self) -> String {
            "test://counting".into()
        }

        fn generation(&self) -> u64 {
            self.generation.load(std::sync::atomic::Ordering::Acquire)
        }
    }

    fn counting() -> (Pool, Arc<std::sync::atomic::AtomicUsize>, Arc<std::sync::atomic::AtomicUsize>, Arc<std::sync::atomic::AtomicU64>) {
        let opened = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let closed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let driver = Counting {
            opened: Arc::clone(&opened),
            closed: Arc::clone(&closed),
            generation: Arc::clone(&generation),
        };
        (Pool::new(Arc::new(driver)), opened, closed, generation)
    }

    /// The drop handler returns the connection on a spawned task, so a test has
    /// to let the runtime run before the pool has it back.
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn a_connection_comes_back_and_is_reused() {
        let (pool, opened, _, _) = counting();

        drop(pool.acquire().await.unwrap());
        settle().await;
        drop(pool.acquire().await.unwrap());
        settle().await;

        assert_eq!(opened.load(std::sync::atomic::Ordering::SeqCst), 1, "the second borrow reused it");
    }

    #[tokio::test]
    async fn an_idle_connection_from_a_rotated_credential_is_never_handed_out() {
        // It would still work — a database authenticates once, at connect — and
        // that is the danger: reusing it keeps access alive under an account
        // the store has already revoked.
        let (pool, opened, closed, generation) = counting();

        drop(pool.acquire().await.unwrap());
        settle().await;
        assert_eq!(pool.idle_count().await, 1);

        generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        drop(pool.acquire().await.unwrap());
        settle().await;

        assert_eq!(opened.load(std::sync::atomic::Ordering::SeqCst), 2, "a fresh connection");
        assert_eq!(closed.load(std::sync::atomic::Ordering::SeqCst), 1, "the stale one was closed");
    }

    #[tokio::test]
    async fn a_borrowed_connection_is_retired_when_it_comes_back() {
        // The other half of the rotation: nothing in flight is interrupted, but
        // a connection handed back after the credential changed does not go
        // into the idle set.
        let (pool, _, closed, generation) = counting();

        let borrowed = pool.acquire().await.unwrap();
        generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        drop(borrowed);
        settle().await;

        assert_eq!(pool.idle_count().await, 0);
        assert_eq!(closed.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retiring_early_closes_the_stale_and_keeps_the_current() {
        let (pool, _, closed, generation) = counting();

        // Two idle connections from the current generation.
        let first = pool.acquire().await.unwrap();
        let second = pool.acquire().await.unwrap();
        drop(first);
        drop(second);
        settle().await;
        assert_eq!(pool.idle_count().await, 2);

        generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        assert_eq!(pool.retire_superseded().await, 2);
        assert_eq!(pool.idle_count().await, 0);
        assert_eq!(closed.load(std::sync::atomic::Ordering::SeqCst), 2);

        // And it leaves current ones alone.
        drop(pool.acquire().await.unwrap());
        settle().await;
        assert_eq!(pool.retire_superseded().await, 0);
        assert_eq!(pool.idle_count().await, 1);
    }

    #[tokio::test]
    async fn a_pool_with_static_credentials_never_retires_anything() {
        // Everything above must cost nothing for the ordinary case, where the
        // generation is zero on both sides forever.
        let (pool, opened, closed, _) = counting();

        for _ in 0..5 {
            drop(pool.acquire().await.unwrap());
            settle().await;
        }

        assert_eq!(opened.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(closed.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// A driver that refuses to connect, so the pool can be tested with no
    /// database anywhere near it.
    struct Unreachable;

    impl Driver for Unreachable {
        fn dialect(&self) -> Arc<dyn Dialect> {
            Arc::new(Postgres)
        }

        fn connect(&self) -> BoxFuture<'_, Result<Box<dyn DriverConnection>>> {
            Box::pin(async { Err(Error::msg("nothing is listening")) })
        }

        fn describe(&self) -> String {
            "test://unreachable".into()
        }

        fn max_connections(&self) -> usize {
            3
        }
    }

    #[tokio::test]
    async fn a_pool_opens_nothing_until_it_is_used() {
        let pool = Pool::new(Arc::new(Unreachable));
        assert_eq!(pool.idle_count().await, 0);
    }

    #[tokio::test]
    async fn acquiring_reports_the_drivers_failure() {
        let pool = Pool::new(Arc::new(Unreachable));

        let error = match pool.acquire().await {
            Err(error) => error.to_string(),
            Ok(_) => panic!("this driver cannot connect"),
        };
        assert!(error.contains("nothing is listening"), "{error}");
    }

    #[tokio::test]
    async fn the_pool_carries_its_drivers_dialect() {
        let pool = Pool::new(Arc::new(Unreachable));
        assert_eq!(pool.dialect().name(), "postgres");
    }
}
