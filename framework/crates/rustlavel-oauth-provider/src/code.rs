//! Authorisation codes: single-use, short-lived, and bound to everything.
//!
//! A code is the only part of the flow that travels through the user's browser,
//! which is the least trustworthy leg there is — it lands in the address bar,
//! in browser history, in a `Referer` header, and in whatever proxy the network
//! runs. So a code is worth as little as it can be made worth:
//!
//! * **Short-lived.** Sixty seconds, which is what RFC 6749 §4.1.2 recommends
//!   and far longer than the redirect it has to survive.
//! * **Single-use.** Enforced atomically, so two simultaneous redemptions
//!   cannot both win.
//! * **Bound** to the client it was issued to, the redirect URI it was issued
//!   for, the user it represents, and the PKCE challenge that was committed to.
//!   Every one of those is checked again at the token endpoint.
//! * **Hashed at rest**, like every other credential here.
//!
//! # Replay is a signal, not just an error
//!
//! RFC 6749 §4.1.2 and §10.5 both say the same thing: if a code is presented
//! twice, the server MUST refuse the second attempt and SHOULD revoke the
//! tokens issued from the first. The reasoning is that a code presented twice
//! is a code that leaked — the legitimate client redeems it exactly once, so a
//! second presentation means somebody else has it, and whichever of the two
//! redemptions was the attacker's, tokens now exist that should not. Refusing
//! the replay alone leaves the attacker holding whatever the first exchange
//! produced.
//!
//! Detecting that requires keeping the record after the code is spent, so a
//! consumed code stays in the store for [`DEFAULT_RETENTION`] before it is
//! purged. A store that deleted the row on use would answer "unknown code" to
//! the replay and revoke nothing.

use crate::store::{StoreFuture, digest};
use rustlavel_oauth::{ChallengeMethod, Scopes};
use std::collections::HashMap;
use std::sync::Mutex;

/// How long a code is valid. RFC 6749 §4.1.2: "a maximum authorization code
/// lifetime of 10 minutes is RECOMMENDED"; a minute is all a redirect needs.
pub const DEFAULT_TTL: u64 = 60;

/// How long a spent code is kept so a replay can still be recognised.
pub const DEFAULT_RETENTION: u64 = 600;

/// One issued authorisation code.
///
/// The code itself is not here — only its digest. The plaintext exists once, in
/// the redirect that carries it.
#[derive(Clone)]
pub struct AuthorizationCode {
    hash: String,
    pub client_id: String,
    /// The exact redirect URI this code was issued for. The token endpoint
    /// requires the same string again, which is what stops a code obtained
    /// through one registered URI being redeemed as though it came from
    /// another.
    pub redirect_uri: String,
    pub user_id: String,
    pub scopes: Scopes,
    pub challenge: String,
    pub challenge_method: ChallengeMethod,
    pub issued_at: u64,
    pub expires_at: u64,
    /// Set when the code is spent: the token family the exchange produced, so a
    /// later replay knows what to revoke.
    pub family: Option<String>,
}

impl AuthorizationCode {
    /// Build a record for a code that is about to be handed out.
    pub fn issued(code: &str, now: u64, ttl: u64) -> AuthorizationCode {
        AuthorizationCode {
            hash: digest(code),
            client_id: String::new(),
            redirect_uri: String::new(),
            user_id: String::new(),
            scopes: Scopes::new(),
            challenge: String::new(),
            challenge_method: ChallengeMethod::S256,
            issued_at: now,
            expires_at: now.saturating_add(ttl),
            family: None,
        }
    }

    pub fn for_client(mut self, client_id: impl Into<String>) -> AuthorizationCode {
        self.client_id = client_id.into();
        self
    }

    pub fn for_user(mut self, user_id: impl Into<String>) -> AuthorizationCode {
        self.user_id = user_id.into();
        self
    }

    pub fn redirecting_to(mut self, uri: impl Into<String>) -> AuthorizationCode {
        self.redirect_uri = uri.into();
        self
    }

    pub fn granting(mut self, scopes: Scopes) -> AuthorizationCode {
        self.scopes = scopes;
        self
    }

    pub fn challenged(
        mut self,
        challenge: impl Into<String>,
        method: ChallengeMethod,
    ) -> AuthorizationCode {
        self.challenge = challenge.into();
        self.challenge_method = method;
        self
    }

    /// The digest this record is stored under.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Whether the code was spent, whatever the outcome of that exchange.
    pub fn is_consumed(&self) -> bool {
        self.family.is_some()
    }

    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at
    }
}

/// `Debug` prints no digest: it is the key a stolen log would need in order to
/// recognise a code it also captured.
impl std::fmt::Debug for AuthorizationCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizationCode")
            .field("hash", &"<redacted>")
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri)
            .field("user_id", &self.user_id)
            .field("scopes", &self.scopes)
            .field("challenge_method", &self.challenge_method)
            .field("expires_at", &self.expires_at)
            .field("family", &self.family)
            .finish()
    }
}

/// What happened when a code was presented.
#[derive(Debug, Clone)]
pub enum Consumption {
    /// The code was live and is now spent. Carry on with the exchange.
    Fresh(Box<AuthorizationCode>),
    /// Already spent. The caller must revoke `family` before refusing.
    Replayed { family: Option<String> },
    /// Still unspent, but past its lifetime.
    Expired,
    /// No such code.
    Unknown,
}

/// Where live and recently-spent authorisation codes are kept.
pub trait CodeStore: Send + Sync + 'static {
    fn store<'a>(&'a self, code: AuthorizationCode) -> StoreFuture<'a, ()>;

    /// Spend a code, exactly once.
    ///
    /// Implementations **must** make the check-and-mark atomic: with two
    /// requests carrying the same code in flight, exactly one may receive
    /// [`Consumption::Fresh`]. A read followed by a separate write leaves a
    /// window in which both do, which is the whole attack this is here to stop.
    ///
    /// `family` is the token family the caller is about to issue into, recorded
    /// on the code so a replay knows what to revoke.
    fn consume<'a>(
        &'a self,
        hash: &'a str,
        family: &'a str,
        now: u64,
    ) -> StoreFuture<'a, Consumption>;

    /// Drop records whose retention window has passed, returning how many.
    ///
    /// Housekeeping rather than security — [`CodeStore::consume`] already
    /// refuses an expired code — but a busy server accumulates one dead record
    /// per login, forever, without it.
    fn purge<'a>(&'a self, now: u64) -> StoreFuture<'a, usize>;
}

/// Codes held in this process's memory.
pub struct MemoryCodeStore {
    codes: Mutex<HashMap<String, AuthorizationCode>>,
    retention: u64,
}

impl MemoryCodeStore {
    pub fn new() -> MemoryCodeStore {
        MemoryCodeStore::with_retention(DEFAULT_RETENTION)
    }

    /// How long a spent code is kept for replay detection. Shortening this
    /// shortens the window in which a stolen code's replay is noticed.
    pub fn with_retention(retention: u64) -> MemoryCodeStore {
        MemoryCodeStore { codes: Mutex::new(HashMap::new()), retention }
    }

    pub fn len(&self) -> usize {
        self.codes.lock().expect("code store lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for MemoryCodeStore {
    fn default() -> MemoryCodeStore {
        MemoryCodeStore::new()
    }
}

impl CodeStore for MemoryCodeStore {
    fn store<'a>(&'a self, code: AuthorizationCode) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.codes
                .lock()
                .expect("code store lock poisoned")
                .insert(code.hash.clone(), code);
            Ok(())
        })
    }

    fn consume<'a>(
        &'a self,
        hash: &'a str,
        family: &'a str,
        now: u64,
    ) -> StoreFuture<'a, Consumption> {
        Box::pin(async move {
            // One lock for the read and the write: this is the atomicity the
            // trait requires, and the reason `consume` is not `find` + `mark`.
            let mut codes = self.codes.lock().expect("code store lock poisoned");
            let Some(code) = codes.get_mut(hash) else { return Ok(Consumption::Unknown) };

            // Spent is checked before expired, deliberately. A code replayed
            // after its lifetime is still a code that leaked, and reporting it
            // as merely expired would skip the revocation.
            if code.is_consumed() {
                return Ok(Consumption::Replayed { family: code.family.clone() });
            }
            if code.is_expired(now) {
                return Ok(Consumption::Expired);
            }

            code.family = Some(family.to_string());
            Ok(Consumption::Fresh(Box::new(code.clone())))
        })
    }

    fn purge<'a>(&'a self, now: u64) -> StoreFuture<'a, usize> {
        Box::pin(async move {
            let mut codes = self.codes.lock().expect("code store lock poisoned");
            let before = codes.len();
            codes.retain(|_, code| now < code.expires_at.saturating_add(self.retention));
            Ok(before - codes.len())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn issue(code: &str) -> AuthorizationCode {
        AuthorizationCode::issued(code, NOW, DEFAULT_TTL)
            .for_client("web")
            .for_user("7")
            .redirecting_to("https://a.test/cb")
            .granting(Scopes::of(["read"]))
            .challenged("challenge", ChallengeMethod::S256)
    }

    async fn stored(code: &str) -> MemoryCodeStore {
        let store = MemoryCodeStore::new();
        store.store(issue(code)).await.expect("stored");
        store
    }

    #[test]
    fn the_code_itself_is_never_kept() {
        let record = issue("the-code");

        assert_ne!(record.hash(), "the-code");
        assert_eq!(record.hash(), digest("the-code"));
        assert!(!format!("{record:?}").contains(record.hash()));
    }

    #[tokio::test]
    async fn a_fresh_code_is_spent_once_and_carries_its_bindings() {
        let store = stored("the-code").await;

        let Consumption::Fresh(code) = store
            .consume(&digest("the-code"), "family-1", NOW)
            .await
            .expect("consumed")
        else {
            panic!("the first use must succeed");
        };

        assert_eq!(code.client_id, "web");
        assert_eq!(code.user_id, "7");
        assert_eq!(code.redirect_uri, "https://a.test/cb");
        assert_eq!(code.challenge, "challenge");
        assert_eq!(code.family.as_deref(), Some("family-1"));
    }

    #[tokio::test]
    async fn replaying_a_code_reports_the_family_to_revoke() {
        let store = stored("the-code").await;
        store.consume(&digest("the-code"), "family-1", NOW).await.expect("first use");

        let second = store
            .consume(&digest("the-code"), "family-2", NOW)
            .await
            .expect("second use");

        // Not merely refused: the caller is handed the family the first
        // exchange produced, because those tokens are now in play too.
        match second {
            Consumption::Replayed { family } => assert_eq!(family.as_deref(), Some("family-1")),
            other => panic!("expected a replay, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_replay_after_the_lifetime_is_still_a_replay() {
        // If expiry were checked first, a code stolen and replayed a minute
        // late would look like an ordinary timeout and revoke nothing.
        let store = stored("the-code").await;
        store.consume(&digest("the-code"), "family-1", NOW).await.expect("first use");

        let late = store
            .consume(&digest("the-code"), "family-2", NOW + DEFAULT_TTL + 1)
            .await
            .expect("late use");

        assert!(matches!(late, Consumption::Replayed { .. }), "got {late:?}");
    }

    #[tokio::test]
    async fn an_expired_code_is_refused_without_being_spent() {
        let store = stored("the-code").await;

        let outcome = store
            .consume(&digest("the-code"), "family-1", NOW + DEFAULT_TTL)
            .await
            .expect("consumed");

        assert!(matches!(outcome, Consumption::Expired), "got {outcome:?}");
        // And it did not become "replayed" on the next attempt, which would
        // have revoked a family that never existed.
        let again = store
            .consume(&digest("the-code"), "family-1", NOW + DEFAULT_TTL)
            .await
            .expect("consumed");
        assert!(matches!(again, Consumption::Expired), "got {again:?}");
    }

    #[tokio::test]
    async fn a_code_expires_the_second_its_lifetime_is_up() {
        let store = stored("the-code").await;

        let inside = store.consume(&digest("the-code"), "f", NOW + DEFAULT_TTL - 1).await;
        assert!(matches!(inside.expect("consumed"), Consumption::Fresh(_)));
    }

    #[tokio::test]
    async fn an_unknown_code_is_unknown() {
        let store = stored("the-code").await;

        let outcome = store.consume(&digest("other"), "family-1", NOW).await.expect("consumed");
        assert!(matches!(outcome, Consumption::Unknown), "got {outcome:?}");
    }

    #[tokio::test]
    async fn the_plaintext_code_is_not_a_lookup_key() {
        // Guards against a store that was handed the code rather than its
        // digest, which would make the database a set of live codes.
        let store = stored("the-code").await;

        let outcome = store.consume("the-code", "family-1", NOW).await.expect("consumed");
        assert!(matches!(outcome, Consumption::Unknown), "got {outcome:?}");
    }

    #[tokio::test]
    async fn purging_keeps_a_spent_code_for_its_retention_window() {
        let store = MemoryCodeStore::with_retention(600);
        store.store(issue("the-code")).await.expect("stored");
        store.consume(&digest("the-code"), "family-1", NOW).await.expect("first use");

        assert_eq!(store.purge(NOW + DEFAULT_TTL + 599).await.expect("purged"), 0);
        // Still detectable as a replay right up to the edge.
        let replay = store.consume(&digest("the-code"), "f", NOW + 599).await.expect("replay");
        assert!(matches!(replay, Consumption::Replayed { .. }));

        assert_eq!(store.purge(NOW + DEFAULT_TTL + 600).await.expect("purged"), 1);
        assert!(store.is_empty());
    }
}
