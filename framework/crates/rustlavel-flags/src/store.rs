//! Where an answer is written down so it outlives the process.
//!
//! A resolver is code, and code only changes with a deploy. The store is the
//! other half: somewhere an operator can put an answer *now* — this customer
//! gets the new checkout today, that flag goes off while we work out what broke
//! — and have it survive a restart and be seen by every process behind the load
//! balancer.
//!
//! An entry is an **override**, not a cache. That distinction runs through the
//! whole crate: a stored value is a decision somebody made, so it beats the
//! resolver rather than standing in for it, and nothing expires it. See
//! [`Flags`](crate::Flags) for the precedence chain it sits in.
//!
//! Only the in-memory implementation ships here. A database-backed store is a
//! dozen lines against this trait, and it is not written for you because the
//! table belongs to the application: its name, its columns, and above all which
//! migration owns it are decisions this crate should not be making.

use crate::scope::Scope;
use rustlavel_core::Result;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

/// A boxed future borrowed from the store and its arguments.
///
/// Mirrors [`rustlavel_cache::store::BoxFuture`] and exists for the same
/// reason: [`FlagStore`] has to be dyn-compatible, because an application
/// chooses its store at boot and everything downstream holds an
/// `Arc<dyn FlagStore>`. `async fn` in a trait is not dyn-compatible, so the
/// methods return this instead.
///
/// [`rustlavel_cache::store::BoxFuture`]: https://docs.rs/rustlavel-cache
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Somewhere overrides are kept.
///
/// Every method is keyed by a `(flag, scope)` pair, and the store is free to
/// key its rows however it likes as long as [`Scope::key`] distinctions are
/// preserved — a user and a tenant with the same name are two subjects.
pub trait FlagStore: Send + Sync + 'static {
    /// The store's name, for error messages and boot logs.
    fn name(&self) -> &'static str;

    /// The override for one flag and scope: `None` when nobody has set one.
    ///
    /// `None` and `Some(false)` are emphatically not the same answer. `None`
    /// means "no opinion, ask the resolver"; `Some(false)` means "somebody
    /// turned this off". A store that collapses the two turns every override
    /// into a suggestion.
    fn get<'a>(&'a self, flag: &'a str, scope: &'a Scope) -> BoxFuture<'a, Result<Option<bool>>>;

    /// Write an override, replacing any that was there.
    fn set<'a>(&'a self, flag: &'a str, scope: &'a Scope, value: bool)
    -> BoxFuture<'a, Result<()>>;

    /// Remove an override, putting the flag back in the resolver's hands.
    ///
    /// Not an error when there was nothing to remove: an operator clearing an
    /// override they are not sure they set wants it gone either way.
    fn forget<'a>(&'a self, flag: &'a str, scope: &'a Scope) -> BoxFuture<'a, Result<()>>;

    /// Remove every override this store holds.
    ///
    /// Worth being careful with. It does not "reset the flags" — it deletes
    /// every decision an operator ever recorded, including the ones holding a
    /// feature off, and the next request resolves them all from scratch.
    fn flush(&self) -> BoxFuture<'_, Result<()>>;
}

/// Overrides in a map, for a single process.
///
/// Cheap to clone, and clones share one map, so the store can be handed to a
/// [`Flags`](crate::Flags) and still be written to from an admin route.
///
/// The limitation is the obvious one and it is worth saying out loud: **an
/// override set here reaches this process and no other.** On one machine that
/// is everything; behind a load balancer it means an operator turning a flag
/// off has turned it off for a third of the traffic. This is the right store
/// for a single-process application, for tests, and for a default that works
/// before anybody has chosen a database — and the wrong one for an incident
/// switch across a fleet.
#[derive(Clone, Default)]
pub struct MemoryStore {
    entries: Arc<RwLock<HashMap<(String, String), bool>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        MemoryStore::default()
    }

    /// How many overrides are stored. Mostly for tests and a status page.
    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A poisoned lock means a thread panicked while holding it. The map is
    /// still a map — nothing here can leave it half-written — so the entries
    /// are recovered rather than propagating the panic into every later
    /// request.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<(String, String), bool>> {
        self.entries.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<(String, String), bool>> {
        self.entries.write().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl FlagStore for MemoryStore {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn get<'a>(&'a self, flag: &'a str, scope: &'a Scope) -> BoxFuture<'a, Result<Option<bool>>> {
        Box::pin(async move {
            let key = (flag.to_string(), scope.key());
            Ok(self.read().get(&key).copied())
        })
    }

    fn set<'a>(
        &'a self,
        flag: &'a str,
        scope: &'a Scope,
        value: bool,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.write().insert((flag.to_string(), scope.key()), value);
            Ok(())
        })
    }

    fn forget<'a>(&'a self, flag: &'a str, scope: &'a Scope) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.write().remove(&(flag.to_string(), scope.key()));
            Ok(())
        })
    }

    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.write().clear();
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_override_round_trips() {
        let store = MemoryStore::new();
        let scope = Scope::user_id(41);

        assert_eq!(store.get("new-checkout", &scope).await.unwrap(), None);

        store.set("new-checkout", &scope, true).await.unwrap();
        assert_eq!(store.get("new-checkout", &scope).await.unwrap(), Some(true));

        store.set("new-checkout", &scope, false).await.unwrap();
        assert_eq!(store.get("new-checkout", &scope).await.unwrap(), Some(false));

        store.forget("new-checkout", &scope).await.unwrap();
        assert_eq!(store.get("new-checkout", &scope).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_stored_false_is_not_the_same_answer_as_nothing_stored() {
        let store = MemoryStore::new();
        let scope = Scope::user_id(7);

        store.set("beta-search", &scope, false).await.unwrap();

        // The distinction the whole precedence chain rests on.
        assert_eq!(store.get("beta-search", &scope).await.unwrap(), Some(false));
        assert_eq!(store.get("never-set", &scope).await.unwrap(), None);
    }

    #[tokio::test]
    async fn scopes_and_flags_are_kept_apart() {
        let store = MemoryStore::new();

        store.set("new-checkout", &Scope::user("acme"), true).await.unwrap();

        assert_eq!(store.get("new-checkout", &Scope::tenant("acme")).await.unwrap(), None);
        assert_eq!(store.get("new-checkout", &Scope::user("other")).await.unwrap(), None);
        assert_eq!(store.get("beta-search", &Scope::user("acme")).await.unwrap(), None);
        assert_eq!(store.get("new-checkout", &Scope::none()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn forgetting_something_that_was_never_there_is_not_an_error() {
        let store = MemoryStore::new();
        store.forget("never-set", &Scope::none()).await.unwrap();
    }

    #[tokio::test]
    async fn flush_empties_the_store() {
        let store = MemoryStore::new();
        store.set("a", &Scope::user_id(1), true).await.unwrap();
        store.set("b", &Scope::none(), false).await.unwrap();
        assert_eq!(store.len(), 2);

        store.flush().await.unwrap();

        assert!(store.is_empty());
        assert_eq!(store.get("a", &Scope::user_id(1)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_clone_shares_the_same_entries() {
        // What makes it usable as application state: the handle a route writes
        // through and the handle `Flags` reads through are the same map.
        let store = MemoryStore::new();
        let handle = store.clone();

        handle.set("new-checkout", &Scope::none(), true).await.unwrap();

        assert_eq!(store.get("new-checkout", &Scope::none()).await.unwrap(), Some(true));
        assert_eq!(store.name(), "memory");
    }

    #[tokio::test]
    async fn it_works_through_a_trait_object() {
        let store: Arc<dyn FlagStore> = Arc::new(MemoryStore::new());

        store.set("new-checkout", &Scope::user_id(41), true).await.unwrap();

        assert_eq!(store.get("new-checkout", &Scope::user_id(41)).await.unwrap(), Some(true));
    }
}
