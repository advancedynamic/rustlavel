//! Registered clients: who is allowed to ask this server for a token.
//!
//! ```ignore
//! let clients = MemoryClientStore::new();
//! clients.register(
//!     Client::confidential("checkout", "the-generated-secret")
//!         .named("Checkout")
//!         .redirect_uri("https://checkout.test/oauth/callback")
//!         .scopes(Scopes::of(["orders.read", "orders.write"])),
//! )?;
//! ```
//!
//! A production application implements [`ClientStore`] against its own table
//! instead; see the module documentation on [`crate::store`] for why this crate
//! has no database of its own.

use crate::redirect;
use crate::store::{StoreFuture, digest, digest_matches};
use rustlavel_core::{Error, Result};
use rustlavel_oauth::Scopes;
use std::collections::HashMap;
use std::sync::Mutex;

/// A client that may ask this server to authorise a user.
///
/// Note that "confidential" is not a field: a client is confidential exactly
/// when it has a secret. Storing the two separately would allow a client marked
/// confidential with no secret to authenticate as anyone who guesses its id,
/// which is the sort of state a schema should make unrepresentable rather than
/// validate.
#[derive(Clone)]
pub struct Client {
    pub id: String,
    /// Shown to the user on the consent screen. Defaults to the id.
    pub name: String,
    /// SHA-256 of the secret; `None` for a public client. See
    /// [`crate::store::digest`] for why this hash and not argon2.
    secret_hash: Option<String>,
    /// Matched byte-for-byte. See [`crate::redirect`].
    pub redirect_uris: Vec<String>,
    /// The most this client may ever be granted, whatever it asks for.
    pub scopes: Scopes,
    /// Whether this client is us. A first-party client with a recorded grant
    /// skips the consent screen — asking a user to authorise our own front end
    /// to talk to our own API teaches them to click through consent screens.
    pub first_party: bool,
}

impl Client {
    /// A client that can keep a secret: a server-side application.
    ///
    /// The secret is hashed immediately and the plaintext is dropped, so the
    /// caller is the last thing that ever holds it. Generate it with
    /// [`generate_secret`] and show it to the operator once.
    pub fn confidential(id: impl Into<String>, secret: &str) -> Client {
        Client { secret_hash: Some(digest(secret)), ..Client::public(id) }
    }

    /// A client that cannot keep a secret: a browser or mobile application.
    ///
    /// Its authenticity is established by PKCE and the registered redirect URI,
    /// not by a credential — anything shipped inside a downloadable binary is
    /// public whatever it is called.
    pub fn public(id: impl Into<String>) -> Client {
        let id = id.into();
        Client {
            name: id.clone(),
            id,
            secret_hash: None,
            redirect_uris: Vec::new(),
            scopes: Scopes::new(),
            first_party: false,
        }
    }

    /// Rebuild a client from a stored hash, for a [`ClientStore`] backed by a
    /// table. The plaintext secret is not needed and should not be available.
    pub fn from_secret_hash(id: impl Into<String>, secret_hash: impl Into<String>) -> Client {
        Client { secret_hash: Some(secret_hash.into()), ..Client::public(id) }
    }

    pub fn named(mut self, name: impl Into<String>) -> Client {
        self.name = name.into();
        self
    }

    pub fn redirect_uri(mut self, uri: impl Into<String>) -> Client {
        self.redirect_uris.push(uri.into());
        self
    }

    pub fn scopes(mut self, scopes: Scopes) -> Client {
        self.scopes = scopes;
        self
    }

    /// Mark this client as one of ours. Read [`Client::first_party`] first.
    pub fn first_party(mut self) -> Client {
        self.first_party = true;
        self
    }

    /// Whether this client holds a secret, and so can authenticate itself.
    pub fn is_confidential(&self) -> bool {
        self.secret_hash.is_some()
    }

    /// The stored hash, so an application can persist it. Never the secret.
    pub fn secret_hash(&self) -> Option<&str> {
        self.secret_hash.as_deref()
    }

    /// Whether the presented secret is this client's.
    ///
    /// A public client fails this unconditionally: it has no secret, so any
    /// secret presented for it is either a mistake or an attempt.
    pub fn verify_secret(&self, presented: &str) -> bool {
        match &self.secret_hash {
            Some(stored) => digest_matches(presented, stored),
            None => false,
        }
    }

    /// Whether this exact redirect URI was registered. See [`crate::redirect`].
    pub fn allows_redirect(&self, uri: &str) -> bool {
        redirect::is_registered(&self.redirect_uris, uri)
    }

    /// Whether this client is fit to be registered.
    ///
    /// Checked when the client is added rather than when it is used, so a bad
    /// redirect URI is a boot-time error with an explanation instead of a
    /// redirect that quietly works for the wrong people.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("a client needs an id".to_string());
        }
        for uri in &self.redirect_uris {
            redirect::validate_registration(uri)
                .map_err(|why| format!("client {:?}: {why}", self.id))?;
        }
        Ok(())
    }
}

/// `Debug` prints no hash. It is not a usable credential, but it is the value
/// an attacker with a stolen log needs in order to know a comparison succeeded.
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("secret_hash", &self.secret_hash.as_ref().map(|_| "<redacted>"))
            .field("redirect_uris", &self.redirect_uris)
            .field("scopes", &self.scopes)
            .field("first_party", &self.first_party)
            .finish()
    }
}

/// A 256-bit client secret, base64url so it survives a form body untouched.
///
/// Show it to the operator once, at registration. Nothing stores it: what this
/// crate keeps is [`crate::store::digest`] of it, so a secret that is lost is
/// rotated rather than recovered.
pub fn generate_secret() -> String {
    rustlavel_auth::base64::encode_url(&rustlavel_auth::random::bytes(32))
}

/// Where registered clients are read from.
pub trait ClientStore: Send + Sync + 'static {
    /// The client with this id, or `None` if it is unknown or disabled.
    fn find<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<Client>>;
}

/// Clients held in this process's memory: right for tests and a development
/// server, wrong for anything that restarts.
#[derive(Default)]
pub struct MemoryClientStore {
    clients: Mutex<HashMap<String, Client>>,
}

impl MemoryClientStore {
    pub fn new() -> MemoryClientStore {
        MemoryClientStore::default()
    }

    /// Add a client, refusing one that could not be safely authorised.
    pub fn register(&self, client: Client) -> Result<()> {
        client.validate().map_err(Error::msg)?;
        self.clients
            .lock()
            .expect("client store lock poisoned")
            .insert(client.id.clone(), client);
        Ok(())
    }

    /// Add a client while building the store: `MemoryClientStore::new().with(client)`.
    ///
    /// Panics on an invalid client, because this form is used where there is no
    /// `Result` to return — a `main.rs` line or a test — and a client with an
    /// unusable redirect URI is a mistake to fix, not a condition to handle.
    pub fn with(self, client: Client) -> MemoryClientStore {
        self.register(client).expect("client is registerable");
        self
    }

    pub fn forget(&self, id: &str) {
        self.clients.lock().expect("client store lock poisoned").remove(id);
    }

    pub fn len(&self) -> usize {
        self.clients.lock().expect("client store lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ClientStore for MemoryClientStore {
    fn find<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<Client>> {
        Box::pin(async move {
            Ok(self.clients.lock().expect("client store lock poisoned").get(id).cloned())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_confidential_client_verifies_its_own_secret_and_no_other() {
        let client = Client::confidential("web", "s3cret");

        assert!(client.is_confidential());
        assert!(client.verify_secret("s3cret"));
        assert!(!client.verify_secret("s3creT"));
        assert!(!client.verify_secret(""));
    }

    #[test]
    fn a_public_client_accepts_no_secret_at_all() {
        // Including the empty string, which is what a client that forgot to
        // send `client_secret` would present.
        let client = Client::public("spa");

        assert!(!client.is_confidential());
        assert!(!client.verify_secret(""));
        assert!(!client.verify_secret("anything"));
    }

    #[test]
    fn the_secret_is_not_kept_anywhere_on_the_client() {
        let client = Client::confidential("web", "s3cret");

        assert_ne!(client.secret_hash(), Some("s3cret"));
        // And presenting the stored hash is not presenting the secret.
        assert!(!client.verify_secret(client.secret_hash().expect("hashed")));
    }

    #[test]
    fn debug_prints_no_hash() {
        let printed = format!("{:?}", Client::confidential("web", "s3cret"));

        assert!(!printed.contains("s3cret"));
        assert!(!printed.contains(&digest("s3cret")));
        assert!(printed.contains("redacted"));
    }

    #[test]
    fn a_client_from_a_stored_hash_verifies_the_same_way() {
        let stored = Client::confidential("web", "s3cret");
        let loaded =
            Client::from_secret_hash("web", stored.secret_hash().expect("hashed").to_string());

        assert!(loaded.verify_secret("s3cret"));
    }

    #[test]
    fn a_generated_secret_is_long_random_and_url_safe() {
        let secret = generate_secret();

        assert_eq!(secret.len(), 43, "32 bytes, base64url, unpadded");
        assert!(secret.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(secret, generate_secret());
    }

    #[test]
    fn redirect_matching_goes_through_the_registered_set_exactly() {
        let client = Client::public("spa").redirect_uri("https://a.test/cb");

        assert!(client.allows_redirect("https://a.test/cb"));
        assert!(!client.allows_redirect("https://a.test/cb/"));
        assert!(!client.allows_redirect("https://a.test.evil.com/cb"));
    }

    #[test]
    fn registration_refuses_a_client_with_an_unsafe_redirect_uri() {
        let store = MemoryClientStore::new();

        let error = store
            .register(Client::public("spa").redirect_uri("http://example.com/cb"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("spa"), "the message names the client: {error}");
        assert!(error.contains("clear text"), "got {error}");
        assert!(store.is_empty(), "a refused client must not be half-registered");
    }

    #[tokio::test]
    async fn the_store_finds_what_was_registered_and_nothing_else() {
        let store = MemoryClientStore::new()
            .with(Client::public("spa").redirect_uri("https://a.test/cb"));

        assert!(store.find("spa").await.expect("lookup").is_some());
        assert!(store.find("nope").await.expect("lookup").is_none());
        // Lookup is exact: no case folding, no trimming.
        assert!(store.find("SPA").await.expect("lookup").is_none());
        assert!(store.find(" spa").await.expect("lookup").is_none());
    }

    #[tokio::test]
    async fn a_forgotten_client_stops_resolving() {
        let store = MemoryClientStore::new().with(Client::public("spa"));
        store.forget("spa");

        assert!(store.find("spa").await.expect("lookup").is_none());
    }
}
