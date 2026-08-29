//! The `state` parameter: the only thing standing between a login button and
//! login CSRF.
//!
//! The attack, concretely. An attacker starts the flow with *their own* Google
//! account, stops at the point where Google hands back a `code`, and gets a
//! victim to load the callback URL carrying it. If the application accepts any
//! callback it is handed, the victim's browser is now signed in as the
//! attacker — and every note, upload and payment method the victim adds
//! afterwards lands in an account the attacker controls. It is quieter than
//! stealing a session and it is far easier to reach.
//!
//! The defence is that a callback must prove it answers a request *this*
//! browser made. Two ways to prove it are offered here, and neither is
//! optional: there is no constructor that skips the check.

use crate::error::{OAuthError, OAuthErrorCode};
use crate::pkce::Pkce;
use crate::url;
use rustlavel_auth::{AppKey, Encrypter, SessionHandle, base64, constant_time_eq, random, unix_now};
use rustlavel_core::Json;
use std::time::Duration;

/// Where the stateful mode keeps the pending flow, unless told otherwise.
pub const DEFAULT_SESSION_KEY: &str = "_oauth_state";

/// How long a pending authorisation stays valid.
///
/// Long enough for somebody to read a consent screen and find their password
/// manager; short enough that a state left in a browser tab overnight is not
/// still a live credential.
pub const DEFAULT_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// Why a callback was refused.
///
/// Three variants rather than one boolean because the words shown to a visitor
/// differ: an expired flow should offer the button again, a mismatched one
/// should not pretend anything is recoverable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateError {
    /// No `state` came back, or none was pending — including the case where one
    /// was pending and has already been spent.
    Missing,
    /// A `state` came back that this application did not issue, or that was
    /// edited on the way.
    Invalid,
    /// Ours, unedited, and too old.
    Expired,
}

impl StateError {
    pub fn message(self) -> &'static str {
        match self {
            StateError::Missing => {
                "no sign-in is in progress for this browser. Start again from the sign-in button."
            }
            StateError::Invalid => {
                "this sign-in did not start here. Start again from the sign-in button."
            }
            StateError::Expired => "this sign-in took too long. Start again from the sign-in button.",
        }
    }
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for StateError {}

/// Every state failure is `invalid_request`: the callback was malformed as a
/// request, whatever the reason, and saying which reason to the *provider*
/// would be describing our own defence to whoever tripped it.
impl From<StateError> for OAuthError {
    fn from(error: StateError) -> OAuthError {
        OAuthError::because(OAuthErrorCode::InvalidRequest, error.message())
    }
}

/// Issues the `state`, and checks it when the visitor comes back.
///
/// Both modes bind the PKCE verifier to the state, so recovering one recovers
/// the other and a caller cannot accidentally exchange a code against a
/// verifier from a different flow.
#[derive(Debug, Clone)]
pub enum StateGuard {
    /// The state is a random nonce; the nonce and the verifier live in the
    /// session. Prefer this: the entry is deleted when it is checked, so a
    /// state works exactly once.
    Session(SessionState),
    /// The state is sealed with a key derived from `APP_KEY` and carries the
    /// verifier and its own issue time. Nothing is stored server-side, which is
    /// what a fleet with no shared session store needs.
    Sealed(SealedState),
}

impl StateGuard {
    /// The stateful mode, on the request's session.
    pub fn session(session: &SessionHandle) -> StateGuard {
        StateGuard::Session(SessionState::new(session.clone()))
    }

    /// The stateless mode, keyed off the application key.
    pub fn sealed(key: &AppKey) -> StateGuard {
        StateGuard::Sealed(SealedState::new(key))
    }

    /// A fresh `state` committing to `pkce`.
    pub fn issue(&self, pkce: &Pkce) -> Result<String, OAuthError> {
        match self {
            StateGuard::Session(guard) => Ok(guard.issue(pkce)),
            StateGuard::Sealed(guard) => guard.issue(pkce),
        }
    }

    /// Check what came back, and recover the verifier it was issued with.
    ///
    /// `None` is the missing case rather than an error the caller has to spell
    /// out, because "the query string had no `state`" is the most common shape
    /// of the attack and must not be reachable by forgetting a check.
    pub fn verify(&self, presented: Option<&str>) -> Result<Pkce, StateError> {
        let presented = match presented {
            Some(state) if !state.is_empty() => state,
            _ => return Err(StateError::Missing),
        };

        match self {
            StateGuard::Session(guard) => guard.verify(presented),
            StateGuard::Sealed(guard) => guard.verify(presented),
        }
    }

    /// How long an issued state stays valid.
    pub fn lifetime(self, lifetime: Duration) -> StateGuard {
        match self {
            StateGuard::Session(guard) => StateGuard::Session(guard.lifetime(lifetime)),
            StateGuard::Sealed(guard) => StateGuard::Sealed(guard.lifetime(lifetime)),
        }
    }
}

/// The stateful mode: a nonce in the session, compared on return.
#[derive(Debug, Clone)]
pub struct SessionState {
    session: SessionHandle,
    key: String,
    lifetime: Duration,
}

impl SessionState {
    pub fn new(session: SessionHandle) -> SessionState {
        SessionState {
            session,
            key: DEFAULT_SESSION_KEY.to_string(),
            lifetime: DEFAULT_LIFETIME,
        }
    }

    /// Store the pending flow somewhere other than `_oauth_state`.
    ///
    /// Worth doing when an application offers two providers side by side: with
    /// one key, opening Google and GitHub in two tabs leaves only the second
    /// flow completable.
    pub fn keyed(mut self, key: impl Into<String>) -> SessionState {
        self.key = key.into();
        self
    }

    pub fn lifetime(mut self, lifetime: Duration) -> SessionState {
        self.lifetime = lifetime;
        self
    }

    fn issue(&self, pkce: &Pkce) -> String {
        // 128 bits from the OS CSPRNG. The state is an anti-forgery token, so
        // it has to be unguessable, not merely unique.
        let nonce = base64::encode_url(&random::bytes(16));

        self.session.put(
            self.key.clone(),
            Json::object([
                ("state", Json::String(nonce.clone())),
                ("verifier", Json::String(pkce.verifier().to_string())),
                ("issued_at", Json::Number(unix_now() as f64)),
            ]),
        );

        nonce
    }

    fn verify(&self, presented: &str) -> Result<Pkce, StateError> {
        // Taken, not read: a state that has been checked once must not check
        // out again, or a code replayed against the callback still gets past
        // the door. This is the whole reason the stateful mode is the default.
        let pending = self.session.forget(&self.key).ok_or(StateError::Missing)?;

        let expected = pending.get("state").and_then(Json::as_str).ok_or(StateError::Invalid)?;
        if !constant_time_eq(expected.as_bytes(), presented.as_bytes()) {
            return Err(StateError::Invalid);
        }

        let issued_at = pending.get("issued_at").and_then(Json::as_f64).unwrap_or(0.0) as u64;
        if unix_now().saturating_sub(issued_at) > self.lifetime.as_secs() {
            return Err(StateError::Expired);
        }

        let verifier = pending.get("verifier").and_then(Json::as_str).ok_or(StateError::Invalid)?;
        Pkce::from_verifier(verifier).map_err(|_| StateError::Invalid)
    }
}

/// The stateless mode: the state is its own storage.
///
/// The payload carries the verifier, the issue time and a nonce, sealed with
/// AES-256-GCM under a key derived from `APP_KEY`. Authenticated encryption is
/// the point: the issue time is inside the sealed blob, so a holder cannot
/// extend their own state's life, and a forged state fails the tag check
/// before a single field is read.
///
/// What it cannot do is burn a state on use — that would need somewhere to
/// write. Within its lifetime the same state verifies twice. It is not the hole
/// it looks like: the state's job is to prove the callback answers a request
/// this browser made, and the *code* it arrives with is single-use at the
/// provider, so a replayed callback fails at the token endpoint with
/// `invalid_grant`. Still, the lifetime here is short for a reason, and an
/// application that can reach a session store should use [`SessionState`].
#[derive(Clone)]
pub struct SealedState {
    encrypter: Encrypter,
    lifetime: Duration,
}

impl SealedState {
    pub fn new(key: &AppKey) -> SealedState {
        SealedState {
            // A sub-key, not the application key: a state sealed here must
            // never decrypt as an encrypted cookie, and an attacker who can
            // make the application encrypt something they chose must not
            // thereby be able to mint a state.
            encrypter: Encrypter::new(AppKey::from_bytes(key.derive("oauth-state"))),
            // Shorter than the stateful default because this one cannot be
            // spent: every second it stays valid is a second it can be reused.
            lifetime: Duration::from_secs(5 * 60),
        }
    }

    pub fn lifetime(mut self, lifetime: Duration) -> SealedState {
        self.lifetime = lifetime;
        self
    }

    fn issue(&self, pkce: &Pkce) -> Result<String, OAuthError> {
        let issued_at = unix_now().to_string();
        // The nonce makes two states issued in the same second differ, so a
        // state is not a stable identifier for the flow it belongs to.
        let nonce = base64::encode_url(&random::bytes(8));
        let payload = url::form_encode(&[
            ("v", pkce.verifier()),
            ("t", &issued_at),
            ("n", &nonce),
        ]);

        self.encrypter.encrypt(&payload).map_err(|error| {
            OAuthError::server_error(format!("could not seal the OAuth state: {error}"))
        })
    }

    fn verify(&self, presented: &str) -> Result<Pkce, StateError> {
        // Every decryption failure looks the same from here — wrong key, edited
        // ciphertext, truncated base64 — which is deliberate: telling the
        // holder which one it was is an oracle.
        let payload = self.encrypter.decrypt(presented).map_err(|_| StateError::Invalid)?;
        let fields = url::form_decode(&payload);
        let field = |name: &str| {
            fields.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
        };

        let issued_at: u64 = field("t").and_then(|t| t.parse().ok()).ok_or(StateError::Invalid)?;
        if unix_now().saturating_sub(issued_at) > self.lifetime.as_secs() {
            return Err(StateError::Expired);
        }

        Pkce::from_verifier(field("v").ok_or(StateError::Invalid)?)
            .map_err(|_| StateError::Invalid)
    }
}

impl std::fmt::Debug for SealedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedState").field("lifetime", &self.lifetime).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_auth::Session;

    fn session_guard() -> StateGuard {
        StateGuard::session(&SessionHandle::new(Session::new()))
    }

    fn sealed_guard() -> StateGuard {
        StateGuard::sealed(&AppKey::from_base64(&AppKey::generate()).unwrap())
    }

    fn both() -> Vec<(&'static str, StateGuard)> {
        vec![("session", session_guard()), ("sealed", sealed_guard())]
    }

    #[test]
    fn a_state_this_application_issued_verifies_and_gives_back_its_verifier() {
        for (mode, guard) in both() {
            let pkce = Pkce::generate();
            let state = guard.issue(&pkce).unwrap();

            let recovered = guard.verify(Some(&state)).expect(mode);
            assert_eq!(recovered.verifier(), pkce.verifier(), "{mode}");
        }
    }

    #[test]
    fn a_callback_with_no_state_at_all_is_rejected_because_that_is_login_csrf() {
        // The attack in one line: an attacker's authorisation code, delivered to
        // a victim's browser as a bare callback URL. Accepting it signs the
        // victim into the attacker's account.
        for (mode, guard) in both() {
            guard.issue(&Pkce::generate()).unwrap();

            assert_eq!(guard.verify(None).err(), Some(StateError::Missing), "{mode}");
            assert_eq!(guard.verify(Some("")).err(), Some(StateError::Missing), "{mode}");
        }
    }

    #[test]
    fn a_callback_carrying_a_state_this_application_never_issued_is_rejected() {
        for (mode, guard) in both() {
            guard.issue(&Pkce::generate()).unwrap();

            assert!(guard.verify(Some("not-a-state-we-made")).is_err(), "{mode}");
        }
    }

    #[test]
    fn a_state_from_a_different_browsers_flow_does_not_verify() {
        // Two visitors, two sessions. The attacker's own legitimately-issued
        // state must not unlock the victim's callback.
        let victim = session_guard();
        let attacker = session_guard();

        victim.issue(&Pkce::generate()).unwrap();
        let attackers_state = attacker.issue(&Pkce::generate()).unwrap();

        assert_eq!(victim.verify(Some(&attackers_state)).err(), Some(StateError::Invalid));
    }

    #[test]
    fn a_stateful_state_cannot_be_replayed_once_it_has_been_spent() {
        let guard = session_guard();
        let state = guard.issue(&Pkce::generate()).unwrap();

        assert!(guard.verify(Some(&state)).is_ok());
        assert_eq!(
            guard.verify(Some(&state)).err(),
            Some(StateError::Missing),
            "a state is spent when it is checked; a replayed callback must find nothing"
        );
    }

    #[test]
    fn a_sealed_state_cannot_be_burned_which_is_why_its_life_is_short() {
        // Stating the limitation rather than hiding it. The second callback is
        // stopped at the token endpoint instead, because the code is single-use
        // there — see the type's documentation.
        let guard = sealed_guard();
        let state = guard.issue(&Pkce::generate()).unwrap();

        assert!(guard.verify(Some(&state)).is_ok());
        assert!(guard.verify(Some(&state)).is_ok(), "documented, not accidental");

        match sealed_guard() {
            StateGuard::Sealed(sealed) => assert_eq!(sealed.lifetime, Duration::from_secs(300)),
            _ => unreachable!(),
        }
    }

    #[test]
    fn an_expired_state_is_rejected_even_though_it_really_was_ours() {
        let guards: Vec<(&str, StateGuard)> = both()
            .into_iter()
            .map(|(mode, guard)| (mode, guard.lifetime(Duration::from_secs(0))))
            .collect();
        let issued: Vec<String> =
            guards.iter().map(|(_, guard)| guard.issue(&Pkce::generate()).unwrap()).collect();

        // `unix_now` has one-second resolution, so a zero lifetime is only
        // reliably in the past once the clock has ticked. Slept once for both
        // guards rather than once each.
        std::thread::sleep(Duration::from_millis(1100));

        for ((mode, guard), state) in guards.iter().zip(&issued) {
            assert_eq!(guard.verify(Some(state)).err(), Some(StateError::Expired), "{mode}");
        }
    }

    #[test]
    fn a_sealed_state_cannot_be_edited_to_extend_its_own_life() {
        // The issue time is inside the authenticated blob, so there is no byte
        // to edit: GCM authenticates the whole ciphertext, and a flipped bit
        // fails the tag rather than yielding a later timestamp.
        let guard = sealed_guard();
        let state = guard.issue(&Pkce::generate()).unwrap();

        let mut raw = base64::decode(&state).unwrap();
        // A byte well inside the ciphertext: past the version byte and the
        // 12-byte nonce, and nowhere near the trailing tag.
        raw[20] ^= 0x01;

        assert_eq!(guard.verify(Some(&base64::encode_url(&raw))).err(), Some(StateError::Invalid));
    }

    #[test]
    fn a_sealed_state_from_another_application_key_is_rejected() {
        let theirs = sealed_guard().issue(&Pkce::generate()).unwrap();

        assert_eq!(sealed_guard().verify(Some(&theirs)).err(), Some(StateError::Invalid));
    }

    #[test]
    fn the_oauth_state_key_is_not_the_key_that_encrypts_cookies() {
        // Otherwise an application that encrypts anything a visitor controls is
        // an application that lets them mint a state.
        let key = AppKey::from_base64(&AppKey::generate()).unwrap();
        let cookies = Encrypter::new(key.clone());
        let guard = StateGuard::sealed(&key);

        let forged = cookies.encrypt(&url::form_encode(&[
            ("v", &"a".repeat(43)),
            ("t", &unix_now().to_string()),
        ]))
        .unwrap();

        assert_eq!(guard.verify(Some(&forged)).err(), Some(StateError::Invalid));
    }

    #[test]
    fn two_flows_in_two_tabs_can_be_kept_apart() {
        let session = SessionHandle::new(Session::new());
        let google = SessionState::new(session.clone()).keyed("_oauth_google");
        let github = SessionState::new(session).keyed("_oauth_github");

        let google_state = google.issue(&Pkce::generate());
        let github_state = github.issue(&Pkce::generate());

        assert!(github.verify(&github_state).is_ok());
        assert!(google.verify(&google_state).is_ok(), "the second flow did not evict the first");
    }

    #[test]
    fn a_state_never_carries_the_verifier_in_the_clear() {
        // The state travels in a URL, through the provider, into browser
        // history and access logs. A verifier visible there would undo PKCE.
        let pkce = Pkce::generate();

        for (mode, guard) in both() {
            let state = guard.issue(&pkce).unwrap();
            assert!(!state.contains(pkce.verifier()), "{mode} leaked the verifier into the URL");
        }
    }

    #[test]
    fn two_states_are_never_the_same() {
        for (mode, guard) in both() {
            let pkce = Pkce::generate();
            assert_ne!(guard.issue(&pkce).unwrap(), guard.issue(&pkce).unwrap(), "{mode}");
        }
    }

    #[test]
    fn the_refusal_says_what_to_do_and_names_no_internals() {
        for error in [StateError::Missing, StateError::Invalid, StateError::Expired] {
            assert!(error.message().contains("sign-in button"), "{error:?}");

            let oauth: OAuthError = error.into();
            assert_eq!(oauth.code, OAuthErrorCode::InvalidRequest);
        }
    }
}
