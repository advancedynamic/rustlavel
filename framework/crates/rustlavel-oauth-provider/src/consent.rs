//! What a user has already agreed to.
//!
//! A recorded grant is the difference between "authorise Checkout to read your
//! orders?" being asked once and being asked on every login. It is also how a
//! user revokes an application: forget the grant and the next authorisation
//! request has to ask again.
//!
//! The grant records the scopes that were agreed to, not merely that something
//! was. A client that comes back asking for one more scope has to ask the user
//! about it — otherwise the first, modest consent becomes permission for
//! everything the client is registered for.

use crate::store::StoreFuture;
use rustlavel_oauth::Scopes;
use std::collections::HashMap;
use std::sync::Mutex;

/// One user's standing consent for one client.
#[derive(Debug, Clone)]
pub struct Grant {
    pub client_id: String,
    pub user_id: String,
    pub scopes: Scopes,
    pub granted_at: u64,
}

impl Grant {
    pub fn new(
        client_id: impl Into<String>,
        user_id: impl Into<String>,
        scopes: Scopes,
        granted_at: u64,
    ) -> Grant {
        Grant { client_id: client_id.into(), user_id: user_id.into(), scopes, granted_at }
    }
}

pub trait ConsentStore: Send + Sync + 'static {
    fn find<'a>(&'a self, client_id: &'a str, user_id: &'a str) -> StoreFuture<'a, Option<Grant>>;

    /// Record consent, replacing any earlier grant for the same pair.
    ///
    /// Replacing rather than merging: the scopes on the screen the user just
    /// approved are the scopes they agreed to. Accumulating across visits would
    /// mean a grant nobody ever saw in full.
    fn record<'a>(&'a self, grant: Grant) -> StoreFuture<'a, ()>;

    /// Withdraw consent. What "revoke this application" calls, alongside
    /// [`crate::TokenStore::revoke_family`].
    fn forget<'a>(&'a self, client_id: &'a str, user_id: &'a str) -> StoreFuture<'a, ()>;
}

/// Grants held in this process's memory.
#[derive(Default)]
pub struct MemoryConsentStore {
    grants: Mutex<HashMap<(String, String), Grant>>,
}

impl MemoryConsentStore {
    pub fn new() -> MemoryConsentStore {
        MemoryConsentStore::default()
    }

    pub fn len(&self) -> usize {
        self.grants.lock().expect("consent store lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ConsentStore for MemoryConsentStore {
    fn find<'a>(&'a self, client_id: &'a str, user_id: &'a str) -> StoreFuture<'a, Option<Grant>> {
        Box::pin(async move {
            let key = (client_id.to_string(), user_id.to_string());
            Ok(self.grants.lock().expect("consent store lock poisoned").get(&key).cloned())
        })
    }

    fn record<'a>(&'a self, grant: Grant) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let key = (grant.client_id.clone(), grant.user_id.clone());
            self.grants.lock().expect("consent store lock poisoned").insert(key, grant);
            Ok(())
        })
    }

    fn forget<'a>(&'a self, client_id: &'a str, user_id: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let key = (client_id.to_string(), user_id.to_string());
            self.grants.lock().expect("consent store lock poisoned").remove(&key);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[tokio::test]
    async fn a_grant_is_found_for_its_own_client_and_user_only() {
        let store = MemoryConsentStore::new();
        store
            .record(Grant::new("web", "7", Scopes::of(["read"]), NOW))
            .await
            .expect("recorded");

        assert!(store.find("web", "7").await.expect("lookup").is_some());
        assert!(store.find("web", "8").await.expect("lookup").is_none());
        assert!(store.find("other", "7").await.expect("lookup").is_none());
    }

    #[tokio::test]
    async fn a_new_grant_replaces_the_old_one_rather_than_accumulating() {
        // If these merged, a user who once approved `read` and later approved
        // only `write` would end up granting both without ever seeing that.
        let store = MemoryConsentStore::new();
        store.record(Grant::new("web", "7", Scopes::of(["read"]), NOW)).await.expect("recorded");
        store.record(Grant::new("web", "7", Scopes::of(["write"]), NOW)).await.expect("recorded");

        let grant = store.find("web", "7").await.expect("lookup").expect("found");
        assert_eq!(grant.scopes, Scopes::of(["write"]));
        assert_eq!(store.len(), 1);
    }

    #[tokio::test]
    async fn a_forgotten_grant_has_to_be_asked_for_again() {
        let store = MemoryConsentStore::new();
        store.record(Grant::new("web", "7", Scopes::of(["read"]), NOW)).await.expect("recorded");

        store.forget("web", "7").await.expect("forgotten");

        assert!(store.find("web", "7").await.expect("lookup").is_none());
    }

    #[tokio::test]
    async fn a_grant_covers_only_what_it_records() {
        let store = MemoryConsentStore::new();
        store.record(Grant::new("web", "7", Scopes::of(["read"]), NOW)).await.expect("recorded");

        let grant = store.find("web", "7").await.expect("lookup").expect("found");
        assert!(grant.scopes.covers(&Scopes::of(["read"])));
        assert!(!grant.scopes.covers(&Scopes::of(["read", "write"])));
    }
}
