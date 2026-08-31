//! The half of a passkey the server is allowed to keep.
//!
//! What is stored here cannot log anybody in. That is the whole point: a
//! stolen password table is a stolen account, and a stolen credential table is
//! a list of public keys and some metadata. It is still worth protecting — the
//! user handles in it identify people — but losing it does not lose the site.
//!
//! There is deliberately no database driver here, and this crate deliberately
//! does not depend on `rustlavel-db`. Credentials belong beside the users they
//! authenticate, in the application's own schema, under whatever key type that
//! table already uses — so an application implements [`CredentialStore`] over
//! its own table, exactly as it does for `rustlavel-auth`'s tokens.

use crate::ceremony::hex;
use crate::cose::CoseKey;
use rustlavel_auth::base64;
use rustlavel_core::{Error, Result};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// One registered passkey.
#[derive(Clone)]
pub struct Credential {
    id: Vec<u8>,
    key: CoseKey,
    sign_count: u32,
    user_handle: Vec<u8>,
    aaguid: [u8; 16],
    created_at: u64,
    last_used_at: Option<u64>,
}

impl Credential {
    pub fn new(
        id: impl Into<Vec<u8>>,
        key: CoseKey,
        sign_count: u32,
        user_handle: impl Into<Vec<u8>>,
        aaguid: [u8; 16],
        created_at: u64,
    ) -> Credential {
        Credential {
            id: id.into(),
            key,
            sign_count,
            user_handle: user_handle.into(),
            aaguid,
            created_at,
            last_used_at: None,
        }
    }

    /// The credential id the authenticator chose, as raw bytes.
    ///
    /// Opaque, and up to 1023 bytes long — long enough that a `varchar(255)`
    /// column will silently truncate one, which turns a working passkey into a
    /// login that fails forever. Store it as bytes, or as base64url in a column
    /// with room for it.
    pub fn id(&self) -> &[u8] {
        &self.id
    }

    /// The id as the browser writes it, for a `varchar` column or a JSON body.
    pub fn id_base64(&self) -> String {
        base64::encode_url(&self.id)
    }

    pub fn key(&self) -> &CoseKey {
        &self.key
    }

    /// The last counter this credential reported.
    ///
    /// Zero means the authenticator does not keep one, which is normal for a
    /// passkey synced across devices — there is no single device to count.
    pub fn sign_count(&self) -> u32 {
        self.sign_count
    }

    pub fn user_handle(&self) -> &[u8] {
        &self.user_handle
    }

    pub fn aaguid(&self) -> &[u8; 16] {
        &self.aaguid
    }

    pub fn created_at(&self) -> u64 {
        self.created_at
    }

    pub fn last_used_at(&self) -> Option<u64> {
        self.last_used_at
    }

    pub fn set_sign_count(&mut self, sign_count: u32) {
        self.sign_count = sign_count;
    }

    pub fn set_last_used_at(&mut self, at: u64) {
        self.last_used_at = Some(at);
    }
}

/// Enough to tell two credentials apart, and nothing that identifies a person.
///
/// The user handle and the credential id both point at one human being, and a
/// login handler is exactly the code whose logs get pasted into a bug report.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("id", &format_args!("<{} bytes>", self.id.len()))
            .field("key", &self.key)
            .field("sign_count", &self.sign_count)
            .field("user_handle", &format_args!("<{} bytes>", self.user_handle.len()))
            .field("aaguid", &format_args!("{}", hex(&self.aaguid)))
            .field("created_at", &self.created_at)
            .field("last_used_at", &self.last_used_at)
            .finish()
    }
}

/// What a credential store's operations return.
pub type CredentialFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Where registered passkeys live.
pub trait CredentialStore: Send + Sync + 'static {
    /// Store a newly registered credential, and refuse a duplicate id.
    ///
    /// The refusal is not tidiness. A credential id is the only thing an
    /// assertion carries to say which key to check it against, so two rows
    /// sharing one id means a login that resolves to whichever the query
    /// happened to return — an attacker who can get a chosen id registered to
    /// their own account could then answer for somebody else's. Back this with
    /// a unique index; do not rely on the check the ceremony does first,
    /// because that check and the insert are not one operation.
    fn create(&self, credential: Credential) -> CredentialFuture<'_, Credential>;

    /// Load one credential by id, or `None` when there is no such row.
    fn find<'a>(&'a self, id: &'a [u8]) -> CredentialFuture<'a, Option<Credential>>;

    /// Every credential belonging to one user handle.
    ///
    /// What `excludeCredentials` and `allowCredentials` are built from, and
    /// what a "your passkeys" settings page lists.
    fn find_for_user<'a>(&'a self, user_handle: &'a [u8]) -> CredentialFuture<'a, Vec<Credential>>;

    /// Record a successful assertion: the new counter and the moment.
    ///
    /// The counter write is the part that matters. A store that reads the
    /// counter and never advances it can never notice a cloned authenticator,
    /// because every assertion is compared against the same stale value.
    fn record_use<'a>(
        &'a self,
        id: &'a [u8],
        sign_count: u32,
        at: u64,
    ) -> CredentialFuture<'a, ()>;

    /// Remove one credential. Deleting an id that is already gone is not an
    /// error.
    fn delete<'a>(&'a self, id: &'a [u8]) -> CredentialFuture<'a, ()>;
}

/// A credential store shared between the ceremony endpoints.
pub type SharedCredentialStore = Arc<dyn CredentialStore>;

/// Credentials held in this process's memory.
///
/// Right for tests and for `cargo run`. Everything is lost on restart, which
/// for this store means every user's passkey is lost with it.
pub struct MemoryCredentialStore {
    credentials: Mutex<HashMap<Vec<u8>, Credential>>,
}

impl MemoryCredentialStore {
    pub fn new() -> MemoryCredentialStore {
        MemoryCredentialStore { credentials: Mutex::new(HashMap::new()) }
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Vec<u8>, Credential>> {
        self.credentials.lock().expect("credential store lock poisoned")
    }
}

impl Default for MemoryCredentialStore {
    fn default() -> Self {
        MemoryCredentialStore::new()
    }
}

/// A count, and nothing else.
impl std::fmt::Debug for MemoryCredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryCredentialStore").field("credentials", &self.len()).finish()
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn create(&self, credential: Credential) -> CredentialFuture<'_, Credential> {
        Box::pin(async move {
            let mut credentials = self.lock();
            if credentials.contains_key(credential.id()) {
                return Err(Error::msg(
                    "that credential id is already registered. Two rows sharing one id means a \
                     login resolves to whichever the store happens to return.",
                ));
            }

            credentials.insert(credential.id().to_vec(), credential.clone());
            Ok(credential)
        })
    }

    fn find<'a>(&'a self, id: &'a [u8]) -> CredentialFuture<'a, Option<Credential>> {
        Box::pin(async move { Ok(self.lock().get(id).cloned()) })
    }

    fn find_for_user<'a>(&'a self, user_handle: &'a [u8]) -> CredentialFuture<'a, Vec<Credential>> {
        Box::pin(async move {
            let mut found: Vec<Credential> = self
                .lock()
                .values()
                .filter(|credential| credential.user_handle() == user_handle)
                .cloned()
                .collect();

            // A HashMap has no order, and an unstable `excludeCredentials` list
            // makes a test that compares two of them flap for no reason.
            found.sort_by(|a, b| a.id().cmp(b.id()));
            Ok(found)
        })
    }

    fn record_use<'a>(
        &'a self,
        id: &'a [u8],
        sign_count: u32,
        at: u64,
    ) -> CredentialFuture<'a, ()> {
        Box::pin(async move {
            if let Some(credential) = self.lock().get_mut(id) {
                credential.set_sign_count(sign_count);
                credential.set_last_used_at(at);
            }
            Ok(())
        })
    }

    fn delete<'a>(&'a self, id: &'a [u8]) -> CredentialFuture<'a, ()> {
        Box::pin(async move {
            self.lock().remove(id);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cbor::Cbor;

    fn key(seed: u8) -> CoseKey {
        let mut bytes = vec![0xa5, 0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21, 0x58, 0x20];
        bytes.extend_from_slice(&[seed; 32]);
        bytes.extend_from_slice(&[0x22, 0x58, 0x20]);
        bytes.extend_from_slice(&[seed.wrapping_add(1); 32]);
        CoseKey::parse(&Cbor::parse(&bytes).unwrap()).unwrap()
    }

    fn credential(id: &[u8], user: &[u8]) -> Credential {
        Credential::new(id.to_vec(), key(id[0]), 0, user.to_vec(), [0u8; 16], 1_700_000_000)
    }

    #[tokio::test]
    async fn the_memory_store_round_trips_a_credential() {
        let store = MemoryCredentialStore::new();
        assert!(store.is_empty());

        store.create(credential(b"abc", b"user-41")).await.unwrap();

        let found = store.find(b"abc").await.unwrap().expect("it was just created");
        assert_eq!(found.id(), b"abc");
        assert_eq!(found.user_handle(), b"user-41");
        assert_eq!(found.sign_count(), 0);
        assert_eq!(found.id_base64(), "YWJj");
        assert_eq!(store.len(), 1);
    }

    #[tokio::test]
    async fn a_credential_id_registered_twice_is_refused_by_the_store_itself() {
        // The ceremony checks first, but the check and the insert are not one
        // operation — two requests racing get past it, and this is what stops
        // them.
        let store = MemoryCredentialStore::new();
        store.create(credential(b"abc", b"user-41")).await.unwrap();

        let error = store.create(credential(b"abc", b"user-99")).await.unwrap_err().to_string();
        assert!(error.contains("already registered"), "got {error}");
        assert_eq!(store.find(b"abc").await.unwrap().unwrap().user_handle(), b"user-41");
    }

    #[tokio::test]
    async fn a_user_can_be_asked_for_every_credential_they_own() {
        let store = MemoryCredentialStore::new();
        for (id, user) in [(b"aaa", b"user-41"), (b"bbb", b"user-41"), (b"ccc", b"user-99")] {
            store.create(credential(id, user)).await.unwrap();
        }

        let theirs = store.find_for_user(b"user-41").await.unwrap();
        assert_eq!(theirs.len(), 2);
        assert_eq!(theirs[0].id(), b"aaa");
        assert_eq!(theirs[1].id(), b"bbb");

        assert!(store.find_for_user(b"nobody").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn recording_a_use_advances_the_counter_a_clone_would_be_caught_by() {
        let store = MemoryCredentialStore::new();
        store.create(credential(b"abc", b"user-41")).await.unwrap();

        store.record_use(b"abc", 7, 1_700_000_100).await.unwrap();

        let found = store.find(b"abc").await.unwrap().unwrap();
        assert_eq!(found.sign_count(), 7);
        assert_eq!(found.last_used_at(), Some(1_700_000_100));
    }

    #[tokio::test]
    async fn an_unknown_id_is_not_found_rather_than_an_error() {
        let store = MemoryCredentialStore::new();

        assert!(store.find(b"nobody").await.unwrap().is_none());
        store.delete(b"nobody").await.unwrap();
        store.record_use(b"nobody", 1, 1).await.unwrap();
    }

    #[tokio::test]
    async fn a_store_works_behind_a_trait_object() {
        let store: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());

        store.create(credential(b"abc", b"user-41")).await.unwrap();
        assert!(store.find(b"abc").await.unwrap().is_some());

        store.delete(b"abc").await.unwrap();
        assert!(store.find(b"abc").await.unwrap().is_none());
    }

    #[test]
    fn debug_output_names_nobody() {
        let printed = format!("{:?}", credential(b"abc", b"user-41"));

        assert!(!printed.contains("user-41"), "the user handle was printed: {printed}");
        assert!(printed.contains("<7 bytes>"), "got {printed}");
        assert!(printed.contains("<public key>"), "the key was printed: {printed}");
        assert!(format!("{:?}", MemoryCredentialStore::new()).contains("credentials"));
    }
}
