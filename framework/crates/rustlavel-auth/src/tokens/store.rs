//! Where API tokens live between requests.
//!
//! One driver ships: [`MemoryTokenStore`], which is the right answer for tests
//! and a development server and the wrong answer for anything that restarts or
//! runs more than one process.
//!
//! There is deliberately no database driver here, and this crate deliberately
//! does not depend on `rustlavel-db`. Tokens belong next to the users they
//! authenticate, in the application's own schema, under whatever key type that
//! table already uses — so an application implements [`TokenStore`] over its
//! own `personal_access_tokens` table. Everything needed to write a row is
//! reachable from [`Token`]: `id`, `identity`, `name`, `digest`, `scopes`,
//! `created_at`, `expires_at`, `last_used_at`.

use super::Token;
use crate::guard::Identity;
use rustlavel_core::Result;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// What a token store's operations return.
///
/// A boxed future rather than an `async fn` in the trait, for the same reason
/// [`crate::store::StoreFuture`] is: the middleware holds `dyn TokenStore` —
/// whichever driver the application picked — and cannot be generic over it
/// without a type parameter spreading into every route.
pub type TokenFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

pub trait TokenStore: Send + Sync + 'static {
    /// Persist a newly issued token and return it as stored.
    ///
    /// The returned token is what the credential is built from, so a table
    /// that allocates its own primary key returns the token with that id
    /// ([`Token::with_id`]). The id must not contain a `|`, or the credential
    /// cannot be split back apart.
    fn create(&self, token: Token) -> TokenFuture<'_, Token>;

    /// Load one token by id, or `None` when there is no such row.
    ///
    /// Expiry is *not* checked here — [`super::authenticate`] does that, after
    /// the secret, so that an expiry cannot be probed without the secret.
    fn find<'a>(&'a self, id: &'a str) -> TokenFuture<'a, Option<Token>>;

    /// Revoke one token. Deleting an id that is already gone is not an error.
    fn delete<'a>(&'a self, id: &'a str) -> TokenFuture<'a, ()>;

    /// Revoke every token belonging to one user.
    ///
    /// What "log out everywhere" and a password change both call. A password
    /// change that leaves the API tokens alive has not actually locked anybody
    /// out.
    fn delete_for_identity<'a>(&'a self, identity: &'a Identity) -> TokenFuture<'a, ()>;

    /// Stamp `last_used_at`, in unix seconds.
    ///
    /// Called on every authenticated request, so a driver over a real database
    /// may reasonably coarsen it — writing a row per request to record a
    /// timestamp nobody reads to the second is a lot of write load for a
    /// "last used 2 hours ago" label.
    fn touch<'a>(&'a self, id: &'a str, at: u64) -> TokenFuture<'a, ()>;
}

/// A token store shared between the middleware and whatever issues tokens.
pub type SharedTokenStore = Arc<dyn TokenStore>;

/// Tokens held in this process's memory.
///
/// Everything is lost on restart and nothing is shared between processes, which
/// makes it right for tests and for `cargo run`, and wrong for a deployment
/// where a token issued by one worker must be accepted by another.
pub struct MemoryTokenStore {
    tokens: Mutex<HashMap<String, Token>>,
}

impl MemoryTokenStore {
    pub fn new() -> Self {
        MemoryTokenStore { tokens: Mutex::new(HashMap::new()) }
    }

    /// How many tokens are held. For tests.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Token>> {
        self.tokens.lock().expect("token store lock poisoned")
    }

    /// Everything held, digests spelled out in hex.
    ///
    /// Only for the test that proves a plain-text secret never reaches the
    /// store: [`Token`]'s own `Debug` redacts the digest, so a dump built from
    /// it could not tell that test anything.
    #[cfg(test)]
    pub(crate) fn dump(&self) -> String {
        let mut out = String::new();
        for (id, token) in self.lock().iter() {
            let digest: String = token.digest().iter().map(|byte| format!("{byte:02x}")).collect();
            out.push_str(&format!("{id} {token:?} {digest}\n"));
        }
        out
    }
}

impl Default for MemoryTokenStore {
    fn default() -> Self {
        MemoryTokenStore::new()
    }
}

/// A count, and nothing else.
///
/// Printing the tokens themselves would put thirty-two digests into whatever
/// log line printed the store.
impl std::fmt::Debug for MemoryTokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryTokenStore").field("tokens", &self.len()).finish()
    }
}

impl TokenStore for MemoryTokenStore {
    fn create(&self, token: Token) -> TokenFuture<'_, Token> {
        Box::pin(async move {
            self.lock().insert(token.id().to_string(), token.clone());
            Ok(token)
        })
    }

    fn find<'a>(&'a self, id: &'a str) -> TokenFuture<'a, Option<Token>> {
        Box::pin(async move { Ok(self.lock().get(id).cloned()) })
    }

    fn delete<'a>(&'a self, id: &'a str) -> TokenFuture<'a, ()> {
        Box::pin(async move {
            self.lock().remove(id);
            Ok(())
        })
    }

    fn delete_for_identity<'a>(&'a self, identity: &'a Identity) -> TokenFuture<'a, ()> {
        Box::pin(async move {
            self.lock().retain(|_, token| token.identity() != identity);
            Ok(())
        })
    }

    fn touch<'a>(&'a self, id: &'a str, at: u64) -> TokenFuture<'a, ()> {
        Box::pin(async move {
            if let Some(token) = self.lock().get_mut(id) {
                token.set_last_used_at(at);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::{NewToken, Scopes, digest};

    #[tokio::test]
    async fn the_memory_store_round_trips_a_token() {
        let store = MemoryTokenStore::new();
        assert!(store.is_empty());

        let token = Token::new("7", Identity::new("41"), "iPhone", digest("s"), Scopes::any());
        let stored = store.create(token).await.unwrap();

        assert_eq!(store.len(), 1);

        let found = store.find("7").await.unwrap().expect("it was just created");
        assert_eq!(found.id(), stored.id());
        assert_eq!(found.name(), "iPhone");
        assert_eq!(found.digest(), &digest("s"));
    }

    #[tokio::test]
    async fn an_unknown_id_is_not_found_rather_than_an_error() {
        let store = MemoryTokenStore::new();

        assert!(store.find("nobody").await.unwrap().is_none());
        assert!(store.find("").await.unwrap().is_none());

        // Deleting something that is not there is not a failure either.
        store.delete("nobody").await.unwrap();
        store.touch("nobody", 1).await.unwrap();
    }

    #[tokio::test]
    async fn deleting_for_one_identity_leaves_the_others() {
        let store = MemoryTokenStore::new();
        for (id, owner) in [("a", "41"), ("b", "41"), ("c", "99")] {
            let token = Token::new(id, Identity::new(owner), id, digest(id), Scopes::any());
            store.create(token).await.unwrap();
        }

        store.delete_for_identity(&Identity::new("41")).await.unwrap();

        assert!(store.find("a").await.unwrap().is_none());
        assert!(store.find("b").await.unwrap().is_none());
        assert!(store.find("c").await.unwrap().is_some());
        assert_eq!(store.len(), 1);
    }

    #[tokio::test]
    async fn touching_records_the_moment_and_nothing_else() {
        let store = MemoryTokenStore::new();
        let token = Token::new("7", Identity::new("41"), "iPhone", digest("s"), Scopes::any());
        store.create(token).await.unwrap();

        store.touch("7", 1_700_000_000).await.unwrap();

        let found = store.find("7").await.unwrap().unwrap();
        assert_eq!(found.last_used_at(), Some(1_700_000_000));
        assert_eq!(found.name(), "iPhone");
        assert_eq!(found.digest(), &digest("s"));
    }

    #[tokio::test]
    async fn a_store_works_behind_a_trait_object() {
        let store: SharedTokenStore = Arc::new(MemoryTokenStore::new());

        let issued =
            NewToken::new(Identity::new("41"), "iPhone").any().issue(&*store).await.unwrap();

        assert!(store.find(issued.token().id()).await.unwrap().is_some());
        store.delete(issued.token().id()).await.unwrap();
        assert!(store.find(issued.token().id()).await.unwrap().is_none());
    }

    #[test]
    fn the_stores_debug_output_is_a_count() {
        let printed = format!("{:?}", MemoryTokenStore::new());

        assert!(printed.contains("MemoryTokenStore"), "printed {printed}");
        assert!(printed.contains('0'));
    }
}
