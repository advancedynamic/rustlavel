//! Named database connections, and one budget across all of them.
//!
//! An application with one database wants [`Database`] and nothing else. An
//! application with a database *per tenant* wants several, by name, and — this
//! is the part that is easy to leave out — it wants somebody counting.
//!
//! **Why the counting cannot live in a pool.** Each [`Pool`] is well behaved on
//! its own: a semaphore caps how many of its connections are borrowed at once,
//! and a caller that arrives when they are all busy waits instead of failing.
//! But a permit is released the moment a connection is handed back, while the
//! socket stays open in the idle queue. So a pool at rest holds connections and
//! reads as idle, and fifty pools at rest hold fifty pools' worth. Nothing in a
//! pool knows the other forty-nine exist.
//!
//! The wall is the server's. PostgreSQL ships with `max_connections = 100`;
//! fifty tenants at the default ten apiece is five hundred. Somewhere around
//! the tenth tenant a connection is refused — and it is refused to whichever
//! tenant happened to need a *new* connection at that moment, which is very
//! likely a quiet one, while the busy tenant that filled the budget carries on
//! reusing what it already holds. The company that suffers is not the company
//! that caused it, and the error points at the database rather than at the
//! code. That is a bad afternoon.
//!
//! So the budget is here, one level up, where the count can exist: this
//! registry knows every pool, asks each how many sockets it is holding, and
//! closes idle ones — least recently used first — before opening more.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::DatabaseConfig;
use crate::{Database, Result};

/// How many connections a registry allows across every named connection.
///
/// A hundred is PostgreSQL's own default for `max_connections`, and a client
/// that defaults to its server's whole allowance is a client that leaves
/// nothing for anybody else — so this is deliberately under it. Raise it with
/// [`Connections::with_budget`] once the server has been raised too.
pub const DEFAULT_BUDGET: usize = 80;

struct Entry {
    database: Database,
    /// A counter, not a clock: it only ever has to order two entries, and a
    /// clock would make the tests depend on time passing.
    used_at: u64,
}

/// Database connections held by name, under a shared budget.
///
/// Cloning shares the registry, so it can be registered as application state
/// and reached from a handler.
#[derive(Clone)]
pub struct Connections {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
    budget: usize,
    tick: Arc<std::sync::atomic::AtomicU64>,
}

impl Default for Connections {
    fn default() -> Self {
        Connections::with_budget(DEFAULT_BUDGET)
    }
}

impl Connections {
    pub fn new() -> Connections {
        Connections::default()
    }

    /// A registry allowing `budget` open connections across every name.
    pub fn with_budget(budget: usize) -> Connections {
        Connections {
            inner: Arc::new(Mutex::new(HashMap::new())),
            budget: budget.max(1),
            tick: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Put a connection under a name, replacing any connection already there.
    ///
    /// For the connections an application knows about at boot — the central
    /// one, a read replica — rather than for tenants, which arrive later and
    /// go through [`get_or_open`](Connections::get_or_open).
    pub async fn insert(&self, name: impl Into<String>, database: Database) {
        let used_at = self.next_tick();
        self.inner.lock().await.insert(name.into(), Entry { database, used_at });
    }

    /// The connection under this name, if it is open.
    ///
    /// Touching it moves it to the front of the queue, so the connection an
    /// application is using is the last one eviction takes.
    pub async fn get(&self, name: &str) -> Option<Database> {
        let used_at = self.next_tick();
        let mut held = self.inner.lock().await;
        let entry = held.get_mut(name)?;
        entry.used_at = used_at;
        Some(entry.database.clone())
    }

    /// The connection under this name, opening it from `config` if it is not
    /// already there.
    ///
    /// This is the call a tenancy middleware makes on every request: the second
    /// request for a tenant reuses the first request's pool, which is the whole
    /// point — opening a connection per request would add a round trip to every
    /// page and exhaust the server besides.
    ///
    /// Room is made before opening, which is why this can close somebody else's
    /// idle connections. It never closes a *borrowed* one.
    pub async fn get_or_open(&self, name: &str, config: DatabaseConfig) -> Result<Database> {
        if let Some(database) = self.get(name).await {
            return Ok(database);
        }

        self.make_room(config.max_connections).await;

        let database = Database::with_config(config).await?;
        self.insert(name, database.clone()).await;
        Ok(database)
    }

    /// Close a named connection's idle sockets and forget it.
    pub async fn forget(&self, name: &str) -> bool {
        let Some(entry) = self.inner.lock().await.remove(name) else { return false };
        let open = entry.database.pool().open_count().await;
        entry.database.pool().close_idle(open).await;
        true
    }

    /// Every name currently held.
    pub async fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.inner.lock().await.keys().cloned().collect();
        names.sort();
        names
    }

    /// How many sockets are open across every named connection.
    pub async fn open_count(&self) -> usize {
        let entries: Vec<Database> =
            self.inner.lock().await.values().map(|entry| entry.database.clone()).collect();

        let mut total = 0;
        for database in entries {
            total += database.pool().open_count().await;
        }
        total
    }

    /// Free enough idle connections that `wanted` more would still fit.
    ///
    /// Least recently used first, and idle only. If every connection is
    /// borrowed there is nothing to free and this returns having done what it
    /// could — the pools' own semaphores then make callers wait, which is the
    /// right answer to "genuinely too busy" and better than refusing.
    async fn make_room(&self, wanted: usize) {
        let mut open = self.open_count().await;
        if open + wanted <= self.budget {
            return;
        }

        let mut candidates: Vec<(u64, Database)> = self
            .inner
            .lock()
            .await
            .values()
            .map(|entry| (entry.used_at, entry.database.clone()))
            .collect();
        candidates.sort_by_key(|(used_at, _)| *used_at);

        for (_, database) in candidates {
            if open + wanted <= self.budget {
                return;
            }
            let over = (open + wanted) - self.budget;
            open -= database.pool().close_idle(over).await;
        }
    }

    fn next_tick(&self) -> u64 {
        self.tick.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names are how central and tenant connections coexist. Keyed by type
    /// alone — which is what `state::<Database>()` does — there can only ever
    /// be one.
    #[tokio::test]
    async fn connections_are_held_by_name() {
        let registry = Connections::new();
        assert!(registry.get("central").await.is_none());
        assert_eq!(registry.names().await, Vec::<String>::new());
        assert!(!registry.forget("central").await);
    }

    #[test]
    fn the_default_budget_leaves_the_server_something() {
        // PostgreSQL's own default is 100. A client that claims all of it
        // leaves nothing for psql, for a backup, or for the next deploy — so
        // this fails to compile rather than fails at run time if the default
        // is ever raised to meet it.
        const { assert!(DEFAULT_BUDGET < 100) };
        assert_eq!(Connections::new().budget(), DEFAULT_BUDGET);
    }

    /// A budget of zero would mean a registry that can never open anything,
    /// which is a configuration mistake rather than an intention.
    #[test]
    fn a_budget_is_at_least_one() {
        assert_eq!(Connections::with_budget(0).budget(), 1);
    }

    #[tokio::test]
    async fn an_empty_registry_holds_nothing_open() {
        assert_eq!(Connections::new().open_count().await, 0);
    }

    /// Making room on an empty registry must not hang or panic: it is the path
    /// every first tenant takes.
    #[tokio::test]
    async fn making_room_when_there_is_nothing_to_free_is_harmless() {
        let registry = Connections::with_budget(4);
        registry.make_room(10).await;
        assert_eq!(registry.open_count().await, 0);
    }
}
