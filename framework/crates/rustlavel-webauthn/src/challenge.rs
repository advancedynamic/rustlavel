//! The value that makes a signature mean "now", and only once.
//!
//! An assertion is a signature over bytes the server chose. Take away any one
//! of the three properties below and the signature stops proving anything
//! about this login:
//!
//! - **Unpredictable.** A challenge an attacker can guess can be signed in
//!   advance, on a page the user visits for some other reason, and presented
//!   later. Thirty-two bytes from the operating system's CSPRNG.
//! - **Single-use.** A challenge that can be answered twice is a password:
//!   whoever captures the answer once can send it again. Answering one takes
//!   it out of the store, so the second attempt finds nothing.
//! - **Short-lived.** A challenge that never expires is one an attacker has
//!   unlimited time to work on, and one the store has to keep forever. Five
//!   minutes is longer than any honest ceremony and shorter than any patient
//!   attack.
//!
//! The store is a trait for the same reason `rustlavel-auth`'s token store is:
//! a memory implementation is right for a test and for `cargo run`, and wrong
//! the moment a second process has to finish a ceremony the first one started.

use crate::ceremony::MINIMUM_CHALLENGE_BYTES;
use rustlavel_auth::{base64, random, unix_now};
use rustlavel_core::{Error, Result};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// How many random bytes a challenge is made of.
pub const CHALLENGE_BYTES: usize = 32;

/// How long a ceremony has to finish, in seconds.
pub const DEFAULT_LIFETIME: u64 = 300;

/// Which ceremony a challenge was issued for.
///
/// Recorded rather than assumed. Registration and authentication both consume
/// a challenge from the same store, so without this a challenge handed out for
/// one could be spent on the other — and the type inside `clientDataJSON`,
/// which is the only other thing that would catch it, is written by the same
/// browser the attacker is driving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ceremony {
    Registration,
    Authentication,
}

impl Ceremony {
    /// The `type` the browser writes into `clientDataJSON` for this ceremony.
    pub fn client_data_type(self) -> &'static str {
        match self {
            Ceremony::Registration => "webauthn.create",
            Ceremony::Authentication => "webauthn.get",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Ceremony::Registration => "registration",
            Ceremony::Authentication => "authentication",
        }
    }
}

/// One issued challenge, waiting to be spent.
#[derive(Clone)]
pub struct Challenge {
    bytes: Vec<u8>,
    ceremony: Ceremony,
    expires_at: u64,
    user_handle: Option<Vec<u8>>,
}

impl Challenge {
    /// Issue a challenge for `ceremony`, good for [`DEFAULT_LIFETIME`].
    pub fn issue(ceremony: Ceremony) -> Challenge {
        Challenge::issue_lasting(ceremony, DEFAULT_LIFETIME)
    }

    /// Issue one with a lifetime of your own choosing, in seconds.
    pub fn issue_lasting(ceremony: Ceremony, lifetime: u64) -> Challenge {
        Challenge {
            bytes: random::bytes(CHALLENGE_BYTES),
            ceremony,
            expires_at: unix_now().saturating_add(lifetime),
            user_handle: None,
        }
    }

    /// Tie this challenge to one user handle.
    ///
    /// A registration ceremony is started by a server that already knows whose
    /// account it is for. Recording that here means the response cannot be
    /// finished against a different account: the challenge and the user have
    /// to agree, and only the server ever knew both.
    pub fn bound_to(mut self, user_handle: impl Into<Vec<u8>>) -> Challenge {
        self.user_handle = Some(user_handle.into());
        self
    }

    /// Rebuild a challenge a store persisted.
    ///
    /// The length is checked on the way back in, because a store is a table
    /// somebody else wrote and a truncated column would otherwise reappear as
    /// a challenge nobody can guess wrong.
    pub fn restore(
        bytes: Vec<u8>,
        ceremony: Ceremony,
        expires_at: u64,
        user_handle: Option<Vec<u8>>,
    ) -> Result<Challenge> {
        if bytes.len() < MINIMUM_CHALLENGE_BYTES {
            return Err(Error::msg(format!(
                "a stored challenge is {} bytes, and anything below {MINIMUM_CHALLENGE_BYTES} \
                 is guessable rather than unpredictable",
                bytes.len()
            )));
        }

        Ok(Challenge { bytes, ceremony, expires_at, user_handle })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The challenge as it travels to the browser, and as a store keys it.
    ///
    /// Always the unpadded URL-safe alphabet, so the key a ceremony computes
    /// from the bytes the client echoed back is the key the store was given —
    /// whatever padding the browser chose to write.
    pub fn encoded(&self) -> String {
        base64::encode_url(&self.bytes)
    }

    pub fn ceremony(&self) -> Ceremony {
        self.ceremony
    }

    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    pub fn user_handle(&self) -> Option<&[u8]> {
        self.user_handle.as_deref()
    }

    pub fn has_expired(&self, now: u64) -> bool {
        now >= self.expires_at
    }

    /// Everything that must be true before a challenge may be spent.
    ///
    /// Called after the store has already removed it, never before: the
    /// removal is what makes it single-use, and a challenge rejected here has
    /// still been spent. Refusing an expired one and *leaving* it in the store
    /// would let an attacker keep a known-good challenge alive by failing on
    /// purpose.
    pub(crate) fn accept(&self, ceremony: Ceremony, now: u64) -> Result<()> {
        if self.ceremony != ceremony {
            return Err(Error::msg(format!(
                "that challenge was issued for {} and this is {}. A challenge is good for one \
                 ceremony, or a response collected by one endpoint can be spent at the other.",
                self.ceremony.name(),
                ceremony.name()
            )));
        }

        if self.has_expired(now) {
            return Err(Error::msg(format!(
                "that challenge expired {} seconds ago. A ceremony has {DEFAULT_LIFETIME} \
                 seconds by default, which is longer than a person needs and shorter than an \
                 attacker wants.",
                now - self.expires_at
            )));
        }

        Ok(())
    }
}

/// Ceremony and expiry, never the value.
///
/// A challenge is not a secret — it is sent to the browser in the clear — but
/// it is live until it is spent, and a log line holding one is a log line
/// worth relaying. There is nothing to gain by printing it.
impl std::fmt::Debug for Challenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Challenge")
            .field("ceremony", &self.ceremony)
            .field("expires_at", &self.expires_at)
            .field("bound", &self.user_handle.is_some())
            .field("challenge", &format_args!("<{} bytes>", self.bytes.len()))
            .finish()
    }
}

/// What a challenge store's operations return.
///
/// A boxed future rather than an `async fn` in the trait, matching
/// `rustlavel_auth::TokenStore`: a ceremony holds `dyn ChallengeStore` —
/// whichever driver the application picked — and cannot be generic over it
/// without a type parameter spreading through every handler.
pub type ChallengeFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Where challenges live between the two halves of a ceremony.
///
/// There is deliberately no database driver here, and this crate deliberately
/// does not depend on `rustlavel-db`. A challenge belongs wherever the
/// application already keeps short-lived per-user state — a session, a cache,
/// a table with a TTL — and everything needed to write a row is reachable from
/// [`Challenge`]: `encoded`, `ceremony`, `expires_at`, `user_handle`.
pub trait ChallengeStore: Send + Sync + 'static {
    /// Record a challenge that has just been handed to a browser.
    fn store(&self, challenge: Challenge) -> ChallengeFuture<'_, ()>;

    /// Remove a challenge and return it, or `None` if it is not there.
    ///
    /// Removing is the contract, not a detail of the implementation: this is
    /// the only thing that makes a challenge single-use, and an implementation
    /// that returns one without deleting it has turned every captured
    /// assertion into a reusable credential. It must be atomic — two requests
    /// arriving together must not both be handed the same challenge.
    ///
    /// The key is the challenge in unpadded base64url, as [`Challenge::encoded`]
    /// writes it.
    fn take<'a>(&'a self, encoded: &'a str) -> ChallengeFuture<'a, Option<Challenge>>;
}

/// A challenge store shared between the ceremony endpoints.
pub type SharedChallengeStore = Arc<dyn ChallengeStore>;

/// Challenges held in this process's memory.
///
/// Right for tests and for `cargo run`; wrong for a deployment where the
/// request that starts a ceremony and the request that finishes it may land on
/// different workers — there the second one finds no challenge and every login
/// fails, which at least fails safe.
pub struct MemoryChallengeStore {
    challenges: Mutex<HashMap<String, Challenge>>,
}

impl MemoryChallengeStore {
    pub fn new() -> MemoryChallengeStore {
        MemoryChallengeStore { challenges: Mutex::new(HashMap::new()) }
    }

    /// How many challenges are outstanding. For tests.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop everything already past its expiry.
    ///
    /// Run on every insert. Without it the map only ever grows: a ceremony
    /// abandoned in the browser leaves a challenge nobody will ever spend, and
    /// abandoning ceremonies is free for whoever is doing it.
    pub fn purge_expired(&self, now: u64) {
        self.lock().retain(|_, challenge| !challenge.has_expired(now));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Challenge>> {
        self.challenges.lock().expect("challenge store lock poisoned")
    }
}

impl Default for MemoryChallengeStore {
    fn default() -> Self {
        MemoryChallengeStore::new()
    }
}

/// A count, and nothing else.
impl std::fmt::Debug for MemoryChallengeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryChallengeStore").field("outstanding", &self.len()).finish()
    }
}

impl ChallengeStore for MemoryChallengeStore {
    fn store(&self, challenge: Challenge) -> ChallengeFuture<'_, ()> {
        Box::pin(async move {
            self.purge_expired(unix_now());
            self.lock().insert(challenge.encoded(), challenge);
            Ok(())
        })
    }

    fn take<'a>(&'a self, encoded: &'a str) -> ChallengeFuture<'a, Option<Challenge>> {
        Box::pin(async move { Ok(self.lock().remove(encoded)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_challenge_is_thirty_two_unpredictable_bytes() {
        let one = Challenge::issue(Ceremony::Registration);
        let two = Challenge::issue(Ceremony::Registration);

        assert_eq!(one.bytes().len(), CHALLENGE_BYTES);
        assert_ne!(one.bytes(), two.bytes());
        assert_eq!(base64::decode(&one.encoded()).unwrap(), one.bytes());
    }

    #[tokio::test]
    async fn answering_a_challenge_takes_it_out_of_the_store() {
        // The removal is what makes it single-use. Everything else about this
        // package assumes it.
        let store = MemoryChallengeStore::new();
        let challenge = Challenge::issue(Ceremony::Authentication);
        let key = challenge.encoded();

        store.store(challenge).await.unwrap();
        assert_eq!(store.len(), 1);

        assert!(store.take(&key).await.unwrap().is_some());
        assert!(store.take(&key).await.unwrap().is_none(), "a replay found the challenge again");
        assert!(store.is_empty());
    }

    #[test]
    fn a_challenge_from_the_other_ceremony_is_refused() {
        let challenge = Challenge::issue(Ceremony::Registration);

        let error =
            challenge.accept(Ceremony::Authentication, unix_now()).unwrap_err().to_string();
        assert!(error.contains("issued for registration"), "got {error}");
    }

    #[test]
    fn an_expired_challenge_is_refused() {
        let challenge = Challenge::issue_lasting(Ceremony::Registration, 0);

        assert!(challenge.has_expired(unix_now()));
        let error = challenge.accept(Ceremony::Registration, unix_now()).unwrap_err().to_string();
        assert!(error.contains("expired"), "got {error}");
    }

    #[tokio::test]
    async fn storing_purges_challenges_nobody_will_ever_spend() {
        // An abandoned ceremony costs whoever abandons it nothing; without
        // this the map only grows. The purge runs before the insert, so an
        // expired challenge is still found and still *refused* by name rather
        // than quietly missing — which is a better error to read.
        let store = MemoryChallengeStore::new();
        let expired = Challenge::issue_lasting(Ceremony::Registration, 0);
        let key = expired.encoded();
        store.store(expired).await.unwrap();
        assert_eq!(store.len(), 1);

        store.store(Challenge::issue(Ceremony::Registration)).await.unwrap();

        assert_eq!(store.len(), 1);
        assert!(store.take(&key).await.unwrap().is_none(), "the expired challenge was kept");
    }

    #[test]
    fn a_restored_challenge_must_still_be_long_enough_to_be_unguessable() {
        let error = Challenge::restore(vec![1, 2, 3], Ceremony::Registration, u64::MAX, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("guessable"), "got {error}");

        let restored =
            Challenge::restore(vec![7u8; 32], Ceremony::Authentication, 99, Some(vec![1])).unwrap();
        assert_eq!(restored.bytes(), &[7u8; 32]);
        assert_eq!(restored.expires_at(), 99);
        assert_eq!(restored.user_handle(), Some([1u8].as_slice()));
    }

    #[test]
    fn each_ceremony_names_the_client_data_type_the_browser_writes() {
        assert_eq!(Ceremony::Registration.client_data_type(), "webauthn.create");
        assert_eq!(Ceremony::Authentication.client_data_type(), "webauthn.get");
    }

    #[test]
    fn debug_output_does_not_carry_a_live_challenge() {
        let challenge = Challenge::issue(Ceremony::Registration).bound_to(vec![1, 2, 3]);
        let printed = format!("{challenge:?}");

        assert!(!printed.contains(&challenge.encoded()), "the challenge was printed: {printed}");
        assert!(printed.contains("<32 bytes>"), "got {printed}");
        assert!(printed.contains("Registration"));

        assert!(format!("{:?}", MemoryChallengeStore::new()).contains("outstanding"));
    }
}
