//! API tokens: `Authorization: Bearer <id>|<secret>`, in Sanctum's shape.
//!
//! A session is a browser mechanism. A mobile app, a command-line client or a
//! single-page application talking to its own API has no cookie jar and no CSRF
//! token to echo; it has one long-lived credential it was handed once. That is
//! what this module issues.
//!
//! ```ignore
//! let store = Arc::new(MemoryTokenStore::new());
//!
//! // Issued once, at the end of a login or from a settings page.
//! let issued = NewToken::new(Identity::new("41"), "iPhone")
//!     .scope("posts:read")
//!     .scope("posts:write")
//!     .expires_in(Duration::from_secs(90 * 86_400))
//!     .issue(&*store)
//!     .await?;
//! println!("{}", issued.plain_text()); // the only time it exists in the clear
//!
//! router.group("/api", |r| {
//!     r.middleware(RequireApiToken::shared(Arc::clone(&store)));
//!     r.get("/me", |req: Request| async move {
//!         format!("user {}", req.identity().unwrap().id())
//!     });
//!
//!     r.group("/posts", |r| {
//!         r.middleware(RequireScope::new("posts:write"));
//!         r.post("", publish);
//!     });
//! });
//! ```
//!
//! # Why the token looks like `<id>|<secret>`
//!
//! Only a hash of the secret is ever stored, so a token presented on a request
//! cannot be looked up by equality. Carrying the record's id in front of the
//! secret turns verification into one indexed primary-key fetch. Without it the
//! only correct implementation is to load every token in the table and hash
//! against each one — which is fine with ten tokens and a denial of service
//! with a hundred thousand.
//!
//! The id is not a secret. It identifies a row; it opens nothing.
//!
//! # Where the tokens live
//!
//! This module deliberately does not depend on `rustlavel-db`. [`TokenStore`]
//! is a trait, [`MemoryTokenStore`] is the implementation used by tests and by
//! a development server, and a real application writes its own against its own
//! `personal_access_tokens` table. Everything needed to persist a token — the
//! digest included — is reachable through [`Token`]'s accessors.
//!
//! [`MemoryTokenStore`]: store::MemoryTokenStore
//! [`TokenStore`]: store::TokenStore

pub mod middleware;
pub mod store;

use crate::guard::Identity;
use crate::{base64, constant_time_eq, random, unix_now};
use rustlavel_core::{Error, Json};
use rustlavel_http::response::{IntoResponse, Response};
use rustlavel_http::Status;
use sha2::{Digest, Sha256};
use std::time::Duration;

pub use middleware::{RequireApiToken, RequireScope, TokenExt, bearer};
pub use store::{MemoryTokenStore, SharedTokenStore, TokenFuture, TokenStore};

/// What separates the id from the secret, as Sanctum spells it.
pub const SEPARATOR: char = '|';

/// The scope that means "everything", as Sanctum spells it.
pub const WILDCARD: &str = "*";

/// How many random bytes go into a secret.
///
/// 256 bits, from the operating system's CSPRNG. There is no meaningful attack
/// on a value this size that is not just "steal it from the client", which is a
/// problem no length fixes.
pub const SECRET_BYTES: usize = 32;

/// The longest string this module will look at before rejecting it.
///
/// A token we minted is well under a hundred characters. Refusing anything
/// longer means a client cannot make the server hash a megabyte per request by
/// sending a megabyte-long `Authorization` header.
pub const MAX_PRESENTED_LENGTH: usize = 512;

/// The SHA-256 of a secret, as stored.
///
/// Not argon2, and that is on purpose. Argon2 is the right answer for
/// passwords, which are short, low-entropy and chosen by people, so an attacker
/// holding the hashes can guess them; the whole point of a KDF is to make each
/// guess expensive. A secret here is 256 bits of CSPRNG output, so there is
/// nothing to guess — offline search is already impossible — and the cost that
/// protects a password buys nothing. What it does buy is a self-inflicted
/// denial of service: argon2 is tuned to take tens of milliseconds and hundreds
/// of megabytes, and an API verifies a token on *every* request.
pub fn digest(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

/// Split a presented token into its id and its secret.
///
/// Every shape that is not exactly `<id>|<secret>`, with both halves present,
/// is rejected here — including the empty string, a bare separator, and
/// anything past [`MAX_PRESENTED_LENGTH`]. Nothing downstream has to wonder
/// whether it was handed a well-formed token.
pub fn split(presented: &str) -> Option<(&str, &str)> {
    if presented.is_empty() || presented.len() > MAX_PRESENTED_LENGTH {
        return None;
    }

    let (id, secret) = presented.split_once(SEPARATOR)?;
    (!id.is_empty() && !secret.is_empty()).then_some((id, secret))
}

/// What a token is allowed to do.
///
/// A plain list of strings, compared exactly. It is deliberately not an enum
/// and deliberately not tied to any other package: an application names its own
/// scopes, and a framework that ships a fixed vocabulary is a framework you
/// fight the first time you add a feature.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scopes(Vec<String>);

impl Scopes {
    /// No scopes at all — a token that can do nothing.
    pub fn new() -> Self {
        Scopes::default()
    }

    /// The wildcard: everything this user can do.
    ///
    /// Sanctum's default, and the right one for a first-party mobile app that
    /// is simply the user on another device.
    pub fn any() -> Self {
        Scopes(vec![WILDCARD.to_string()])
    }

    /// Add one scope, ignoring a repeat.
    pub fn push(&mut self, scope: impl Into<String>) {
        let scope = scope.into();
        if !self.0.contains(&scope) {
            self.0.push(scope);
        }
    }

    /// Add one scope, for chaining: `Scopes::new().with("posts:read")`.
    pub fn with(mut self, scope: impl Into<String>) -> Self {
        self.push(scope);
        self
    }

    /// Whether this list grants `scope`.
    ///
    /// The match is exact, apart from the wildcard. `posts:write` does not
    /// grant `posts:writeextra` and does not grant `posts:write:all`: a prefix
    /// match here would mean that adding a *narrower*-sounding scope later
    /// silently hands it to every token that already held the shorter one.
    pub fn can(&self, scope: &str) -> bool {
        self.0.iter().any(|held| held == WILDCARD || held == scope)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, String> {
        self.0.iter()
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

impl<S: Into<String>> FromIterator<S> for Scopes {
    fn from_iter<I: IntoIterator<Item = S>>(iter: I) -> Self {
        let mut scopes = Scopes::new();
        for scope in iter {
            scopes.push(scope);
        }
        scopes
    }
}

impl std::fmt::Display for Scopes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.join(" "))
    }
}

/// One issued token, as the store keeps it: metadata plus a digest.
///
/// The secret is not in here and never was. A store hands these back, the
/// middleware attaches one to the request, and a handler reads it with
/// `req.api_token()`.
#[derive(Clone)]
pub struct Token {
    id: String,
    identity: Identity,
    name: String,
    digest: [u8; 32],
    scopes: Scopes,
    created_at: u64,
    expires_at: Option<u64>,
    last_used_at: Option<u64>,
}

impl Token {
    /// Rebuild a token from a stored row.
    ///
    /// This is the constructor an application's own [`TokenStore`] uses when it
    /// reads a record back out of its table. Issuing a *new* token goes through
    /// [`NewToken`], which is the only place a secret is generated.
    pub fn new(
        id: impl Into<String>,
        identity: Identity,
        name: impl Into<String>,
        digest: [u8; 32],
        scopes: Scopes,
    ) -> Self {
        Token {
            id: id.into(),
            identity,
            name: name.into(),
            digest,
            scopes,
            created_at: unix_now(),
            expires_at: None,
            last_used_at: None,
        }
    }

    /// Give the token a different id.
    ///
    /// For a store whose table allocates the key itself: `create` returns the
    /// token it actually stored, and the plain-text credential is assembled
    /// from that id.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn with_created_at(mut self, at: u64) -> Self {
        self.created_at = at;
        self
    }

    pub fn with_expires_at(mut self, at: Option<u64>) -> Self {
        self.expires_at = at;
        self
    }

    pub fn with_last_used_at(mut self, at: Option<u64>) -> Self {
        self.last_used_at = at;
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Who the token speaks for.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// What the person called it — "iPhone", "CI deploy key".
    ///
    /// Purely so a revocation screen lists something a human recognises.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn scopes(&self) -> &Scopes {
        &self.scopes
    }

    /// The stored SHA-256 of the secret, for a store to persist.
    ///
    /// Safe to write to a database column; not safe to write to a log. It
    /// proves nothing on its own, but it is the only value that verifies a
    /// token, so anywhere it leaks is somewhere it can be replayed against.
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    pub fn created_at(&self) -> u64 {
        self.created_at
    }

    pub fn expires_at(&self) -> Option<u64> {
        self.expires_at
    }

    pub fn last_used_at(&self) -> Option<u64> {
        self.last_used_at
    }

    pub fn set_last_used_at(&mut self, at: u64) {
        self.last_used_at = Some(at);
    }

    /// Whether the token grants `scope`. See [`Scopes::can`].
    pub fn can(&self, scope: &str) -> bool {
        self.scopes.can(scope)
    }

    /// A token with no expiry never expires — that is what `None` means.
    pub fn is_expired(&self) -> bool {
        self.is_expired_at(unix_now())
    }

    pub fn is_expired_at(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|at| now >= at)
    }

    /// Whether `secret` is the one this token was issued with.
    ///
    /// Constant time: the digest is compared with [`constant_time_eq`], so how
    /// long the check took says nothing about how many bytes were right.
    pub fn matches_secret(&self, secret: &str) -> bool {
        constant_time_eq(&self.digest, &digest(secret))
    }
}

/// Redacted by hand rather than derived.
///
/// The digest is the one value that verifies a token. `{:?}` on a request's
/// extensions, or on a store, is exactly the sort of thing that ends up in a
/// log file, and a log file is exactly where it must not be.
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("id", &self.id)
            .field("identity", &self.identity)
            .field("name", &self.name)
            .field("digest", &"<redacted>")
            .field("scopes", &self.scopes)
            .field("created_at", &self.created_at)
            .field("expires_at", &self.expires_at)
            .field("last_used_at", &self.last_used_at)
            .finish()
    }
}

/// A freshly issued token, and the one moment its secret exists in the clear.
///
/// Returned by [`NewToken::issue`] and by nothing else. Show
/// [`plain_text`](Self::plain_text) to the person once; there is no way to
/// recover it afterwards, because the store never had it.
pub struct PlainTextToken {
    token: Token,
    plain_text: String,
}

impl PlainTextToken {
    /// `<id>|<secret>` — what the client sends as its bearer credential.
    pub fn plain_text(&self) -> &str {
        &self.plain_text
    }

    /// The credential, consuming the wrapper.
    pub fn into_plain_text(self) -> String {
        self.plain_text
    }

    /// The stored record: id, name, scopes, expiry.
    pub fn token(&self) -> &Token {
        &self.token
    }

    pub fn into_token(self) -> Token {
        self.token
    }

    pub fn into_parts(self) -> (Token, String) {
        (self.token, self.plain_text)
    }
}

impl std::fmt::Debug for PlainTextToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlainTextToken")
            .field("token", &self.token)
            .field("plain_text", &"<redacted>")
            .finish()
    }
}

/// A token about to be issued: who it belongs to, what it may do, how long for.
///
/// ```ignore
/// let issued = NewToken::new(Identity::new("41"), "iPhone")
///     .scope("posts:read")
///     .expires_in(Duration::from_secs(30 * 86_400))
///     .issue(&*store)
///     .await?;
/// ```
#[derive(Debug, Clone)]
pub struct NewToken {
    identity: Identity,
    name: String,
    scopes: Scopes,
    expires_at: Option<u64>,
}

impl NewToken {
    /// A token for `identity`, named the way its owner will recognise it.
    ///
    /// It starts with no scopes, so a token issued and never granted anything
    /// can do nothing. Defaulting to the wildcard would mean a forgotten line
    /// hands out full access; ask for [`any`](Self::any) when that is meant.
    pub fn new(identity: Identity, name: impl Into<String>) -> Self {
        NewToken { identity, name: name.into(), scopes: Scopes::new(), expires_at: None }
    }

    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scopes.push(scope);
        self
    }

    pub fn scopes(mut self, scopes: Scopes) -> Self {
        self.scopes = scopes;
        self
    }

    /// Grant the wildcard: everything its owner can do.
    pub fn any(self) -> Self {
        self.scopes(Scopes::any())
    }

    /// Expire at a unix timestamp in seconds.
    pub fn expires_at(mut self, at: u64) -> Self {
        self.expires_at = Some(at);
        self
    }

    /// Expire `after` from now.
    pub fn expires_in(self, after: Duration) -> Self {
        self.expires_at(unix_now().saturating_add(after.as_secs()))
    }

    /// Mint the secret, store the digest, and hand back the credential.
    ///
    /// The id is generated here but the *stored* id is the one the store
    /// returns, so a table with its own primary key can replace it and the
    /// credential still points at the right row.
    pub async fn issue(self, store: &dyn TokenStore) -> Result<PlainTextToken, Error> {
        // URL-safe and unpadded, so the credential survives a query string, a
        // header and a shell command line without escaping — and, crucially,
        // contains no `|` of its own to confuse the split.
        let secret = base64::encode_url(&random::bytes(SECRET_BYTES));

        let id = random::hex(16);
        let token = Token::new(id, self.identity, self.name, digest(&secret), self.scopes)
            .with_expires_at(self.expires_at);

        let stored = store.create(token).await?;
        let plain_text = format!("{}{SEPARATOR}{secret}", stored.id());

        Ok(PlainTextToken { token: stored, plain_text })
    }
}

/// Why a presented token was not accepted.
///
/// An unknown id and a wrong secret are the same variant on purpose. They are
/// indistinguishable to the caller, and keeping them apart in the type is how
/// somebody eventually writes a message — or a log line, or a metric — that
/// tells an attacker which of their guesses was half right.
#[derive(Debug)]
pub enum TokenError {
    /// No `Authorization: Bearer` header at all.
    Missing,
    /// Present, but not shaped like `<id>|<secret>`.
    Malformed,
    /// No such token, or the wrong secret for it.
    Invalid,
    /// The right secret for a token that is past its expiry.
    Expired,
    /// The store itself failed. Ours, not the caller's.
    Unavailable(Error),
}

impl TokenError {
    /// What the client is told. Never mentions the token it presented.
    pub fn message(&self) -> &'static str {
        match self {
            TokenError::Missing => "Unauthenticated.",
            // One message for both, for the reason the enum's docs give.
            TokenError::Malformed | TokenError::Invalid => "Invalid API token.",
            // Safe to be specific: only somebody holding the real secret ever
            // sees this, and "it expired" is the one thing they can act on.
            TokenError::Expired => "This API token has expired.",
            TokenError::Unavailable(_) => "Could not verify the API token.",
        }
    }

    pub fn status(&self) -> Status {
        match self {
            TokenError::Unavailable(_) => Status(500),
            _ => Status::UNAUTHORIZED,
        }
    }

    /// The `WWW-Authenticate` challenge, per RFC 6750.
    ///
    /// A bare `Bearer` when nothing was presented — the client may simply not
    /// know it needed to authenticate — and `error="invalid_token"` when
    /// something was, which tells a correct client to stop retrying with the
    /// credential it has and go get another one.
    pub fn challenge(&self) -> &'static str {
        match self {
            TokenError::Missing => "Bearer",
            _ => r#"Bearer error="invalid_token""#,
        }
    }
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for TokenError {}

impl IntoResponse for TokenError {
    fn into_response(self) -> Response {
        // A store that is down is a server fault, and must not be reported as
        // "your credential is bad" — that would have every client throw away a
        // perfectly good token because the database blinked.
        if let TokenError::Unavailable(error) = self {
            return error.into_response();
        }

        Response::new(self.status())
            .with_header("www-authenticate", self.challenge())
            .with_json(Json::object([("message", Json::from(self.message()))]))
    }
}

/// Look a presented token up and check it.
///
/// The order matters. The secret is verified before the expiry, so somebody
/// who does not hold the secret cannot learn whether a given id ever existed
/// and when it lapsed; they get [`TokenError::Invalid`] either way.
///
/// `last_used_at` is stamped on success, the way Sanctum does, so a settings
/// page can show "last used two hours ago" and an unused key is visibly safe to
/// revoke.
pub async fn authenticate(store: &dyn TokenStore, presented: &str) -> Result<Token, TokenError> {
    let (id, secret) = split(presented).ok_or(TokenError::Malformed)?;

    // The id is not a secret — it names a row, it opens nothing — so looking it
    // up before the secret check leaks nothing worth having.
    let found = store.find(id).await.map_err(TokenError::Unavailable)?;
    let Some(mut token) = found else { return Err(TokenError::Invalid) };

    if !token.matches_secret(secret) {
        return Err(TokenError::Invalid);
    }
    if token.is_expired() {
        return Err(TokenError::Expired);
    }

    let now = unix_now();
    if let Err(error) = store.touch(token.id(), now).await {
        // Bookkeeping. The request authenticated; failing it now because a
        // timestamp could not be written would be a database hiccup turning
        // into an outage.
        rustlavel_core::warn!("could not stamp last_used_at on an API token: {error}");
    }
    token.set_last_used_at(now);

    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> MemoryTokenStore {
        MemoryTokenStore::new()
    }

    async fn issue(store: &MemoryTokenStore, id: &str, name: &str) -> PlainTextToken {
        NewToken::new(Identity::new(id), name)
            .any()
            .issue(store)
            .await
            .expect("the memory store never fails")
    }

    #[tokio::test]
    async fn an_issued_token_reads_as_its_id_and_a_secret() {
        let store = store();
        let issued = issue(&store, "41", "iPhone").await;

        let (id, secret) = split(issued.plain_text()).expect("the credential should split");

        assert_eq!(id, issued.token().id());
        assert_eq!(issued.token().identity(), &Identity::new("41"));
        assert_eq!(issued.token().name(), "iPhone");

        // 32 bytes of base64url, unpadded.
        assert_eq!(secret.len(), 43);
        assert!(secret.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[tokio::test]
    async fn two_tokens_issued_together_share_nothing() {
        let store = store();
        let first = issue(&store, "41", "iPhone").await;
        let second = issue(&store, "41", "iPad").await;

        assert_ne!(first.token().id(), second.token().id());
        assert_ne!(first.plain_text(), second.plain_text());
    }

    #[tokio::test]
    async fn the_plain_text_secret_is_never_kept_by_the_store() {
        let store = store();
        let issued = issue(&store, "41", "iPhone").await;
        let (_, secret) = split(issued.plain_text()).unwrap();

        let stored = store.find(issued.token().id()).await.unwrap().expect("it was just created");

        assert_ne!(stored.digest().as_slice(), secret.as_bytes());
        assert_eq!(stored.digest(), &digest(secret), "only the digest is kept");
        assert!(
            !store.dump().contains(secret),
            "the secret was found somewhere in the store's contents"
        );
    }

    #[tokio::test]
    async fn no_type_here_prints_a_secret_in_its_debug_output() {
        let store = store();
        let issued = issue(&store, "41", "iPhone").await;
        let (_, secret) = split(issued.plain_text()).unwrap();

        let printed = format!(
            "{:?} {:?} {:?} {:?}",
            issued,
            issued.token(),
            store,
            RequireApiToken::new(MemoryTokenStore::new()).scope("posts:read")
        );

        assert!(!printed.contains(secret), "a secret escaped into {printed}");
        assert!(!printed.contains(issued.plain_text()));
        assert!(printed.contains("<redacted>"));

        // The harmless metadata is still there, or the redaction has gone too
        // far to be useful when debugging.
        assert!(printed.contains("iPhone"));
        assert!(printed.contains(issued.token().id()));
    }

    #[tokio::test]
    async fn a_valid_token_authenticates() {
        let store = store();
        let issued = issue(&store, "41", "iPhone").await;

        let token = authenticate(&store, issued.plain_text()).await.expect("it should be accepted");

        assert_eq!(token.id(), issued.token().id());
        assert_eq!(token.identity().id(), "41");
    }

    #[tokio::test]
    async fn authenticating_stamps_last_used_at() {
        let store = store();
        let issued = issue(&store, "41", "iPhone").await;

        assert_eq!(issued.token().last_used_at(), None, "a token starts unused");

        let token = authenticate(&store, issued.plain_text()).await.unwrap();
        assert!(token.last_used_at().is_some());

        let stored = store.find(token.id()).await.unwrap().unwrap();
        assert_eq!(stored.last_used_at(), token.last_used_at(), "and it was persisted");
    }

    #[tokio::test]
    async fn an_expired_token_is_rejected() {
        let store = store();
        let issued = NewToken::new(Identity::new("41"), "expired")
            .any()
            .expires_in(Duration::ZERO)
            .issue(&store)
            .await
            .unwrap();

        assert!(issued.token().is_expired());
        let outcome = authenticate(&store, issued.plain_text()).await;
        assert!(matches!(outcome, Err(TokenError::Expired)));

        // A token with no expiry is not accidentally expired.
        let forever = issue(&store, "41", "forever").await;
        assert_eq!(forever.token().expires_at(), None);
        assert!(!forever.token().is_expired());
        assert!(authenticate(&store, forever.plain_text()).await.is_ok());
    }

    #[tokio::test]
    async fn a_known_id_with_the_wrong_secret_is_rejected() {
        let store = store();
        let issued = issue(&store, "41", "iPhone").await;
        let id = issued.token().id();

        // An *empty* secret is not in this list: it never gets as far as the
        // digest, because `split` refuses the shape. It is covered below.
        for guess in ["x", "not-the-secret", &"a".repeat(43)] {
            let presented = format!("{id}{SEPARATOR}{guess}");
            assert!(
                matches!(authenticate(&store, &presented).await, Err(TokenError::Invalid)),
                "the secret {guess:?} should not open token {id}"
            );
        }
    }

    #[tokio::test]
    async fn one_users_token_id_cannot_be_paired_with_anothers_secret() {
        let store = store();
        let mine = issue(&store, "41", "mine").await;
        let yours = issue(&store, "99", "yours").await;

        let (_, my_secret) = split(mine.plain_text()).unwrap();
        let stolen = format!("{}{SEPARATOR}{my_secret}", yours.token().id());

        assert!(matches!(authenticate(&store, &stolen).await, Err(TokenError::Invalid)));

        // And the real pairings still work, so the test is not passing by
        // rejecting everything.
        assert!(authenticate(&store, mine.plain_text()).await.is_ok());
        assert!(authenticate(&store, yours.plain_text()).await.is_ok());
    }

    #[tokio::test]
    async fn a_malformed_token_is_rejected_without_panicking() {
        let store = store();
        let huge = "a".repeat(100_000);
        let long_id = format!("{}{SEPARATOR}secret", "a".repeat(MAX_PRESENTED_LENGTH));

        for presented in [
            "",
            "no-pipe",
            "|",
            "a|",
            "|b",
            "||",
            " ",
            "\0",
            "él|ok",
            huge.as_str(),
            long_id.as_str(),
        ] {
            let outcome = authenticate(&store, presented).await;
            assert!(
                matches!(outcome, Err(TokenError::Malformed) | Err(TokenError::Invalid)),
                "{presented:?} was not rejected: {outcome:?}"
            );
        }
    }

    #[test]
    fn splitting_accepts_only_the_two_part_shape() {
        assert_eq!(split("7|secret"), Some(("7", "secret")));
        // The secret keeps everything after the first separator, so a stray
        // one cannot be used to truncate the comparison.
        assert_eq!(split("7|a|b"), Some(("7", "a|b")));

        assert_eq!(split(""), None);
        assert_eq!(split("|"), None);
        assert_eq!(split("a|"), None);
        assert_eq!(split("|b"), None);
        assert_eq!(split("no-pipe"), None);
        assert_eq!(split(&"a".repeat(MAX_PRESENTED_LENGTH + 1)), None);
    }

    #[tokio::test]
    async fn revoking_a_token_takes_effect_immediately() {
        let store = store();
        let issued = issue(&store, "41", "iPhone").await;

        assert!(authenticate(&store, issued.plain_text()).await.is_ok());

        store.delete(issued.token().id()).await.unwrap();

        let outcome = authenticate(&store, issued.plain_text()).await;
        assert!(matches!(outcome, Err(TokenError::Invalid)));
    }

    #[tokio::test]
    async fn revoking_every_token_for_a_user_leaves_the_others_alone() {
        let store = store();
        let phone = issue(&store, "41", "iPhone").await;
        let laptop = issue(&store, "41", "laptop").await;
        let someone_else = issue(&store, "99", "theirs").await;

        store.delete_for_identity(&Identity::new("41")).await.unwrap();

        for revoked in [&phone, &laptop] {
            assert!(matches!(
                authenticate(&store, revoked.plain_text()).await,
                Err(TokenError::Invalid)
            ));
        }
        assert!(authenticate(&store, someone_else.plain_text()).await.is_ok());
    }

    #[test]
    fn can_matches_a_scope_exactly_and_not_by_prefix() {
        let scopes = Scopes::new().with("posts:write").with("orders:read");

        assert!(scopes.can("posts:write"));
        assert!(scopes.can("orders:read"));

        assert!(!scopes.can("posts:writeextra"), "a longer scope must not be granted");
        assert!(!scopes.can("posts:writ"), "nor a shorter one");
        assert!(!scopes.can("posts:write:all"));
        assert!(!scopes.can("posts:read"), "read and write are not the same permission");
        assert!(!scopes.can("POSTS:WRITE"), "the comparison is case sensitive");
        assert!(!scopes.can(""));
    }

    #[test]
    fn the_wildcard_grants_everything_and_an_empty_list_grants_nothing() {
        let everything = Scopes::any();
        assert!(everything.can("posts:write"));
        assert!(everything.can("anything at all"));
        assert!(everything.can(WILDCARD));

        let nothing = Scopes::new();
        assert!(nothing.is_empty());
        assert!(!nothing.can("posts:read"));
        assert!(!nothing.can(WILDCARD), "the wildcard is not held, so it is not granted");
    }

    #[test]
    fn scopes_are_collected_deduplicated_and_kept_in_order() {
        let scopes: Scopes = ["posts:read", "posts:write", "posts:read"].into_iter().collect();

        assert_eq!(scopes.len(), 2);
        assert_eq!(scopes.as_slice(), ["posts:read", "posts:write"]);
        assert_eq!(scopes.to_string(), "posts:read posts:write");
        assert_eq!(scopes.iter().count(), 2);
    }

    #[tokio::test]
    async fn a_tokens_scopes_answer_can() {
        let store = store();
        let issued = NewToken::new(Identity::new("41"), "reader")
            .scope("posts:read")
            .issue(&store)
            .await
            .unwrap();

        assert!(issued.token().can("posts:read"));
        assert!(!issued.token().can("posts:write"));
        assert_eq!(issued.token().scopes().as_slice(), ["posts:read"]);
    }

    #[tokio::test]
    async fn a_new_token_grants_nothing_until_it_is_asked_to() {
        let store = store();
        let issued = NewToken::new(Identity::new("41"), "bare").issue(&store).await.unwrap();

        assert!(issued.token().scopes().is_empty());
        assert!(!issued.token().can("posts:read"));
        assert!(!issued.token().can(WILDCARD));
    }

    #[test]
    fn a_rejection_says_nothing_about_the_token_that_was_presented() {
        assert_eq!(TokenError::Missing.message(), "Unauthenticated.");
        assert_eq!(
            TokenError::Malformed.message(),
            TokenError::Invalid.message(),
            "an unknown id and a wrong secret must be indistinguishable"
        );

        assert_eq!(TokenError::Missing.challenge(), "Bearer");
        assert!(TokenError::Invalid.challenge().starts_with("Bearer"));
        assert!(TokenError::Invalid.challenge().contains("invalid_token"));

        assert_eq!(TokenError::Invalid.status(), Status::UNAUTHORIZED);
        assert_eq!(TokenError::Unavailable(Error::msg("down")).status(), Status(500));
    }

    #[test]
    fn digests_differ_for_different_secrets_and_are_stable_for_the_same_one() {
        assert_eq!(digest("secret"), digest("secret"));
        assert_ne!(digest("secret"), digest("secreu"));
        assert_eq!(digest("").len(), 32);
    }

    #[test]
    fn a_token_rebuilt_from_a_row_keeps_every_field() {
        let token = Token::new("7", Identity::new("41"), "CI", digest("s"), Scopes::any())
            .with_created_at(1_000)
            .with_expires_at(Some(2_000))
            .with_last_used_at(Some(1_500));

        assert_eq!(token.id(), "7");
        assert_eq!(token.name(), "CI");
        assert_eq!(token.created_at(), 1_000);
        assert_eq!(token.expires_at(), Some(2_000));
        assert_eq!(token.last_used_at(), Some(1_500));

        assert!(!token.is_expired_at(1_999));
        assert!(token.is_expired_at(2_000), "the expiry second is already past");
        assert!(token.is_expired_at(2_001));

        assert!(token.matches_secret("s"));
        assert!(!token.matches_secret("t"));
    }
}
