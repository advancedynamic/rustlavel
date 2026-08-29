//! Access and refresh tokens, and the family that ties them together.
//!
//! # Why the tokens are opaque
//!
//! An access token here is 256 random bits, stored as a digest and looked up on
//! every request. The obvious alternative is a signed JWT that the resource
//! server verifies without asking anyone — faster, and stateless.
//!
//! It is also unrevocable. A JWT is valid because its signature says so, which
//! means a token that has been stolen, or belongs to a user who was just
//! deleted, or to a client whose access was withdrawn an hour ago, keeps
//! working until it expires. The usual repair is a revocation list the resource
//! server checks — at which point it is asking the server on every request
//! anyway, and has traded a lookup for a lookup plus a signature.
//!
//! Revocation that actually revokes is worth a hash-map lookup, so tokens are
//! opaque. That is also what makes [`crate::endpoints::revoke`] and
//! [`crate::endpoints::introspect`] able to tell the truth.
//!
//! # Families
//!
//! Every token descended from one authorisation carries the same `family` id:
//! the access and refresh tokens minted from a code, and everything a refresh
//! rotates into afterwards. It exists so that one detection — a replayed
//! authorisation code, a reused refresh token — can revoke the whole lineage
//! rather than the single token that happened to be presented. Revoking only
//! the presented token leaves the thief holding the other one.

use crate::store::{StoreFuture, digest};
use rustlavel_auth::{base64, random};
use rustlavel_oauth::Scopes;
use std::collections::HashMap;
use std::sync::Mutex;

/// An hour, which is Laravel Passport's default and a reasonable one: long
/// enough that refreshing is rare, short enough that a leak has an end.
pub const DEFAULT_ACCESS_TTL: u64 = 3600;

/// Thirty days, again matching Passport.
pub const DEFAULT_REFRESH_TTL: u64 = 30 * 24 * 3600;

/// A 256-bit opaque token, base64url so it needs no escaping anywhere.
pub fn generate() -> String {
    base64::encode_url(&random::bytes(32))
}

/// An identifier for a record or a family. Not a secret; never presented.
fn identifier() -> String {
    random::hex(16)
}

/// One issued access token, as stored. The token itself is not here.
#[derive(Clone)]
pub struct AccessToken {
    pub id: String,
    hash: String,
    pub client_id: String,
    /// `None` for `client_credentials`: that grant has no user behind it, and a
    /// resource server must be able to tell the two apart.
    pub user_id: Option<String>,
    pub scopes: Scopes,
    pub family: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub revoked: bool,
}

impl AccessToken {
    pub fn issued(token: &str, now: u64, ttl: u64) -> AccessToken {
        AccessToken {
            id: identifier(),
            hash: digest(token),
            client_id: String::new(),
            user_id: None,
            scopes: Scopes::new(),
            family: String::new(),
            issued_at: now,
            expires_at: now.saturating_add(ttl),
            revoked: false,
        }
    }

    pub fn for_client(mut self, client_id: impl Into<String>) -> AccessToken {
        self.client_id = client_id.into();
        self
    }

    /// `None` for `client_credentials`, which has no user behind it.
    pub fn for_user(mut self, user_id: Option<&str>) -> AccessToken {
        self.user_id = user_id.map(str::to_string);
        self
    }

    pub fn granting(mut self, scopes: Scopes) -> AccessToken {
        self.scopes = scopes;
        self
    }

    pub fn in_family(mut self, family: impl Into<String>) -> AccessToken {
        self.family = family.into();
        self
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Whether the token may be used right now.
    pub fn is_live(&self, now: u64) -> bool {
        !self.revoked && now < self.expires_at
    }
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessToken")
            .field("id", &self.id)
            .field("hash", &"<redacted>")
            .field("client_id", &self.client_id)
            .field("user_id", &self.user_id)
            .field("scopes", &self.scopes)
            .field("family", &self.family)
            .field("expires_at", &self.expires_at)
            .field("revoked", &self.revoked)
            .finish()
    }
}

/// One issued refresh token, as stored.
#[derive(Clone)]
pub struct RefreshToken {
    pub id: String,
    hash: String,
    pub client_id: String,
    pub user_id: Option<String>,
    pub scopes: Scopes,
    pub family: String,
    /// The access token minted alongside it, revoked when this one rotates so a
    /// refresh does not leave the previous access token live for another hour.
    pub access_token_id: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub revoked: bool,
    /// Set once this token has been exchanged. A rotated token presented again
    /// is the signal that something has a copy of it.
    pub rotated: bool,
}

impl RefreshToken {
    pub fn issued(token: &str, now: u64, ttl: u64) -> RefreshToken {
        RefreshToken {
            id: identifier(),
            hash: digest(token),
            client_id: String::new(),
            user_id: None,
            scopes: Scopes::new(),
            family: String::new(),
            access_token_id: String::new(),
            issued_at: now,
            expires_at: now.saturating_add(ttl),
            revoked: false,
            rotated: false,
        }
    }

    pub fn for_client(mut self, client_id: impl Into<String>) -> RefreshToken {
        self.client_id = client_id.into();
        self
    }

    pub fn for_user(mut self, user_id: Option<&str>) -> RefreshToken {
        self.user_id = user_id.map(str::to_string);
        self
    }

    pub fn granting(mut self, scopes: Scopes) -> RefreshToken {
        self.scopes = scopes;
        self
    }

    pub fn in_family(mut self, family: impl Into<String>) -> RefreshToken {
        self.family = family.into();
        self
    }

    /// The access token minted alongside this one, so a rotation can retire it.
    pub fn alongside(mut self, access_token_id: impl Into<String>) -> RefreshToken {
        self.access_token_id = access_token_id.into();
        self
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn is_live(&self, now: u64) -> bool {
        !self.revoked && !self.rotated && now < self.expires_at
    }
}

impl std::fmt::Debug for RefreshToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshToken")
            .field("id", &self.id)
            .field("hash", &"<redacted>")
            .field("client_id", &self.client_id)
            .field("user_id", &self.user_id)
            .field("scopes", &self.scopes)
            .field("family", &self.family)
            .field("expires_at", &self.expires_at)
            .field("revoked", &self.revoked)
            .field("rotated", &self.rotated)
            .finish()
    }
}

/// Where issued tokens live.
pub trait TokenStore: Send + Sync + 'static {
    fn store_access<'a>(&'a self, token: AccessToken) -> StoreFuture<'a, ()>;
    fn store_refresh<'a>(&'a self, token: RefreshToken) -> StoreFuture<'a, ()>;

    /// Look a token up by its digest — the record whatever its state, so the
    /// caller can tell "revoked" apart from "never existed" and act on it.
    fn find_access<'a>(&'a self, hash: &'a str) -> StoreFuture<'a, Option<AccessToken>>;
    fn find_refresh<'a>(&'a self, hash: &'a str) -> StoreFuture<'a, Option<RefreshToken>>;

    /// Mark a refresh token rotated, returning whether *this* call did it.
    ///
    /// The return value is the whole point: two requests presenting the same
    /// live refresh token must not both be told yes. Implementations must make
    /// this a compare-and-set, not a read followed by a write — otherwise a
    /// stolen refresh token racing the legitimate one is issued a second family
    /// and nothing notices.
    fn rotate<'a>(&'a self, id: &'a str) -> StoreFuture<'a, bool>;

    fn revoke_access<'a>(&'a self, id: &'a str) -> StoreFuture<'a, ()>;
    fn revoke_refresh<'a>(&'a self, id: &'a str) -> StoreFuture<'a, ()>;

    /// Revoke everything descended from one authorisation, returning how many
    /// records changed. This is what a detected replay or reuse calls.
    fn revoke_family<'a>(&'a self, family: &'a str) -> StoreFuture<'a, usize>;

    /// Drop records that are past their lifetime. Housekeeping.
    fn purge<'a>(&'a self, now: u64) -> StoreFuture<'a, usize>;
}

/// Tokens held in this process's memory.
///
/// Wrong for production for the usual reason and one extra: a restart would
/// empty the store, and an empty store answers "unknown token" to everything —
/// which fails safe — but it also loses every revocation, and a revocation that
/// a restart undoes is not a revocation.
#[derive(Default)]
pub struct MemoryTokenStore {
    access: Mutex<HashMap<String, AccessToken>>,
    refresh: Mutex<HashMap<String, RefreshToken>>,
}

impl MemoryTokenStore {
    pub fn new() -> MemoryTokenStore {
        MemoryTokenStore::default()
    }

    /// How many access tokens are usable right now. For tests.
    pub fn live_access(&self, now: u64) -> usize {
        self.access.lock().expect("token store lock poisoned").values().filter(|t| t.is_live(now)).count()
    }

    /// How many refresh tokens are usable right now. For tests.
    pub fn live_refresh(&self, now: u64) -> usize {
        self.refresh.lock().expect("token store lock poisoned").values().filter(|t| t.is_live(now)).count()
    }
}

impl TokenStore for MemoryTokenStore {
    fn store_access<'a>(&'a self, token: AccessToken) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.access
                .lock()
                .expect("token store lock poisoned")
                .insert(token.hash.clone(), token);
            Ok(())
        })
    }

    fn store_refresh<'a>(&'a self, token: RefreshToken) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.refresh
                .lock()
                .expect("token store lock poisoned")
                .insert(token.hash.clone(), token);
            Ok(())
        })
    }

    fn find_access<'a>(&'a self, hash: &'a str) -> StoreFuture<'a, Option<AccessToken>> {
        Box::pin(async move {
            Ok(self.access.lock().expect("token store lock poisoned").get(hash).cloned())
        })
    }

    fn find_refresh<'a>(&'a self, hash: &'a str) -> StoreFuture<'a, Option<RefreshToken>> {
        Box::pin(async move {
            Ok(self.refresh.lock().expect("token store lock poisoned").get(hash).cloned())
        })
    }

    fn rotate<'a>(&'a self, id: &'a str) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            // One lock around the test and the set: this is the compare-and-set
            // the trait requires.
            let mut refresh = self.refresh.lock().expect("token store lock poisoned");
            let Some(token) = refresh.values_mut().find(|token| token.id == id) else {
                return Ok(false);
            };
            if token.rotated {
                return Ok(false);
            }
            token.rotated = true;
            Ok(true)
        })
    }

    fn revoke_access<'a>(&'a self, id: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let mut access = self.access.lock().expect("token store lock poisoned");
            if let Some(token) = access.values_mut().find(|token| token.id == id) {
                token.revoked = true;
            }
            Ok(())
        })
    }

    fn revoke_refresh<'a>(&'a self, id: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let mut refresh = self.refresh.lock().expect("token store lock poisoned");
            if let Some(token) = refresh.values_mut().find(|token| token.id == id) {
                token.revoked = true;
            }
            Ok(())
        })
    }

    fn revoke_family<'a>(&'a self, family: &'a str) -> StoreFuture<'a, usize> {
        Box::pin(async move {
            let mut changed = 0;
            let mut access = self.access.lock().expect("token store lock poisoned");
            for token in access.values_mut().filter(|t| t.family == family && !t.revoked) {
                token.revoked = true;
                changed += 1;
            }
            drop(access);

            let mut refresh = self.refresh.lock().expect("token store lock poisoned");
            for token in refresh.values_mut().filter(|t| t.family == family && !t.revoked) {
                token.revoked = true;
                changed += 1;
            }
            Ok(changed)
        })
    }

    fn purge<'a>(&'a self, now: u64) -> StoreFuture<'a, usize> {
        Box::pin(async move {
            let mut removed = 0;

            let mut access = self.access.lock().expect("token store lock poisoned");
            let before = access.len();
            access.retain(|_, token| now < token.expires_at);
            removed += before - access.len();
            drop(access);

            let mut refresh = self.refresh.lock().expect("token store lock poisoned");
            let before = refresh.len();
            refresh.retain(|_, token| now < token.expires_at);
            removed += before - refresh.len();

            Ok(removed)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn access(token: &str, family: &str) -> AccessToken {
        AccessToken::issued(token, NOW, DEFAULT_ACCESS_TTL)
            .for_client("web")
            .for_user(Some("7"))
            .granting(Scopes::of(["read"]))
            .in_family(family)
    }

    fn refresh(token: &str, family: &str, access_token_id: &str) -> RefreshToken {
        RefreshToken::issued(token, NOW, DEFAULT_REFRESH_TTL)
            .for_client("web")
            .for_user(Some("7"))
            .granting(Scopes::of(["read"]))
            .in_family(family)
            .alongside(access_token_id)
    }

    async fn populated() -> (MemoryTokenStore, AccessToken, RefreshToken) {
        let store = MemoryTokenStore::new();
        let at = access("at-1", "family-1");
        let rt = refresh("rt-1", "family-1", &at.id);
        store.store_access(at.clone()).await.expect("stored");
        store.store_refresh(rt.clone()).await.expect("stored");
        (store, at, rt)
    }

    #[test]
    fn a_generated_token_is_256_bits_and_url_safe() {
        let token = generate();

        assert_eq!(token.len(), 43);
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(token, generate());
    }

    #[test]
    fn debug_prints_no_digest_for_either_token() {
        let at = access("at-1", "f");
        let rt = refresh("rt-1", "f", &at.id);

        assert!(!format!("{at:?}").contains(at.hash()));
        assert!(!format!("{rt:?}").contains(rt.hash()));
        assert!(format!("{at:?}").contains("redacted"));
    }

    #[tokio::test]
    async fn a_token_is_found_by_its_digest_and_not_by_its_plaintext() {
        let (store, _, _) = populated().await;

        assert!(store.find_access(&digest("at-1")).await.expect("lookup").is_some());
        assert!(store.find_access("at-1").await.expect("lookup").is_none());
        assert!(store.find_refresh(&digest("rt-1")).await.expect("lookup").is_some());
    }

    #[tokio::test]
    async fn rotation_succeeds_exactly_once() {
        // The compare-and-set the trait promises. Without it, a stolen refresh
        // token racing the real one is quietly issued its own family.
        let (store, _, rt) = populated().await;

        assert!(store.rotate(&rt.id).await.expect("rotate"));
        assert!(!store.rotate(&rt.id).await.expect("rotate"), "the second must lose");
        assert!(!store.rotate("no-such-id").await.expect("rotate"));
    }

    #[tokio::test]
    async fn a_rotated_token_is_no_longer_live() {
        let (store, _, rt) = populated().await;
        store.rotate(&rt.id).await.expect("rotate");

        let reloaded = store.find_refresh(&digest("rt-1")).await.expect("lookup").expect("found");
        assert!(reloaded.rotated);
        assert!(!reloaded.is_live(NOW));
    }

    #[tokio::test]
    async fn revoking_a_family_takes_down_both_halves_of_it() {
        let (store, _, _) = populated().await;
        store.store_access(access("at-2", "family-2")).await.expect("stored");

        assert_eq!(store.revoke_family("family-1").await.expect("revoked"), 2);
        assert_eq!(store.live_access(NOW), 1, "the other family is untouched");
        assert_eq!(store.live_refresh(NOW), 0);

        // Idempotent: revoking again changes nothing, so a second detection of
        // the same theft does not report a fresh revocation.
        assert_eq!(store.revoke_family("family-1").await.expect("revoked"), 0);
    }

    #[tokio::test]
    async fn revoking_one_token_leaves_its_sibling_alone() {
        let (store, at, _) = populated().await;

        store.revoke_access(&at.id).await.expect("revoked");

        assert_eq!(store.live_access(NOW), 0);
        assert_eq!(store.live_refresh(NOW), 1);
    }

    #[tokio::test]
    async fn an_expired_token_is_not_live_and_is_eventually_purged() {
        let (store, _, _) = populated().await;

        assert_eq!(store.live_access(NOW + DEFAULT_ACCESS_TTL), 0);
        assert_eq!(store.live_refresh(NOW + DEFAULT_ACCESS_TTL), 1, "refresh outlives access");

        assert_eq!(store.purge(NOW + DEFAULT_ACCESS_TTL).await.expect("purged"), 1);
        assert_eq!(store.purge(NOW + DEFAULT_REFRESH_TTL).await.expect("purged"), 1);
    }
}
