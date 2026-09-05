//! The session value: what is remembered about one visitor between requests.
//!
//! A `Session` is a plain map plus Laravel's flash lifecycle, and knows nothing
//! about HTTP or storage. That separation is what makes it testable without a
//! server and swappable between the in-memory and file stores.

use crate::random;
use rustlavel_core::Json;
use std::collections::{BTreeMap, BTreeSet};

/// How many bytes of entropy a session id carries.
///
/// A session id is a bearer token: whoever presents it *is* the user, so it has
/// to be far beyond guessing range even for an attacker making millions of
/// attempts. 32 bytes — 256 bits — is the size at which enumeration stops being
/// a threat model and starts being arithmetic.
pub const ID_BYTES: usize = 32;

/// Whether a string could have been produced by [`Session::new_id`].
///
/// Session ids arrive from a cookie, which is to say from the client, and the
/// file store turns one into a path. Checking the shape before anything else
/// touches it means a value like `../../.env` is rejected as a session id long
/// before it can be rejected as a filename.
pub fn is_valid_id(id: &str) -> bool {
    (ID_BYTES * 2..=128).contains(&id.len())
        && id.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// One visitor's session.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    id: String,
    data: BTreeMap<String, Json>,
    /// Flashed during this request; must still be readable in the next one.
    fresh_flash: BTreeSet<String>,
    /// Flashed during the previous request; readable now, dropped at its end.
    stale_flash: BTreeSet<String>,
    /// Unix seconds of the last write, for expiry.
    updated_at: u64,
    dirty: bool,
}

impl Session {
    /// Where the CSRF token lives, and the name of the form field that carries
    /// it back. Laravel's `_token`, so a template ported across looks familiar.
    pub const TOKEN_KEY: &'static str = "_token";

    /// Start a session with a brand new id.
    pub fn new() -> Self {
        Session::with_id(Session::new_id())
    }

    /// Rebuild a session around a known id.
    pub fn with_id(id: impl Into<String>) -> Self {
        Session {
            id: id.into(),
            data: BTreeMap::new(),
            fresh_flash: BTreeSet::new(),
            stale_flash: BTreeSet::new(),
            updated_at: crate::unix_now(),
            dirty: false,
        }
    }

    /// A fresh identifier: [`ID_BYTES`] of OS entropy, hex encoded.
    pub fn new_id() -> String {
        random::hex(ID_BYTES)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// When this session was last written, in unix seconds.
    pub fn updated_at(&self) -> u64 {
        self.updated_at
    }

    /// Whether anything changed since the session was loaded.
    ///
    /// The middleware uses this to avoid writing a session file for every
    /// anonymous request that never touched the session.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        self.data.get(key)
    }

    /// A value as a string, which is what most session data actually is.
    pub fn get_string(&self, key: &str) -> Option<String> {
        match self.data.get(key)? {
            Json::String(value) => Some(value.clone()),
            Json::Null => None,
            other => Some(other.to_string()),
        }
    }

    pub fn has(&self, key: &str) -> bool {
        self.data.contains_key(key)
    }

    pub fn put(&mut self, key: impl Into<String>, value: impl Into<Json>) {
        self.data.insert(key.into(), value.into());
        self.dirty = true;
    }

    /// Remove a key, handing back whatever was there.
    pub fn forget(&mut self, key: &str) -> Option<Json> {
        let removed = self.data.remove(key);
        self.fresh_flash.remove(key);
        self.stale_flash.remove(key);
        if removed.is_some() {
            self.dirty = true;
        }
        removed
    }

    /// Remove everything, keeping the id.
    ///
    /// On logout prefer [`Session::invalidate`], which also rotates the id.
    pub fn flush(&mut self) {
        self.data.clear();
        self.fresh_flash.clear();
        self.stale_flash.clear();
        self.dirty = true;
    }

    pub fn all(&self) -> &BTreeMap<String, Json> {
        &self.data
    }

    /// Store a value that survives exactly one further request.
    ///
    /// The redirect-with-a-message pattern: a POST handler flashes `status`,
    /// redirects, and the GET that follows renders it. By the request after
    /// that, it is gone — so a refresh does not show the message twice.
    pub fn flash(&mut self, key: impl Into<String>, value: impl Into<Json>) {
        let key = key.into();
        self.fresh_flash.insert(key.clone());
        self.put(key, value);
    }

    /// Keep one flashed value for another request.
    pub fn keep(&mut self, key: &str) {
        if self.stale_flash.remove(key) {
            self.fresh_flash.insert(key.to_string());
            self.dirty = true;
        }
    }

    /// Keep every flashed value for another request.
    pub fn reflash(&mut self) {
        let carried = std::mem::take(&mut self.stale_flash);
        if !carried.is_empty() {
            self.dirty = true;
        }
        self.fresh_flash.extend(carried);
    }

    /// Advance the flash lifecycle by one request.
    ///
    /// Called by the session middleware after the handler has run: what was
    /// flashed *last* request has now been read and is dropped, and what was
    /// flashed *this* request takes its place.
    pub fn age_flash(&mut self) {
        let expiring: Vec<String> =
            self.stale_flash.difference(&self.fresh_flash).cloned().collect();
        for key in expiring {
            self.data.remove(&key);
            self.dirty = true;
        }
        self.stale_flash = std::mem::take(&mut self.fresh_flash);
    }

    /// Give this session a new id, keeping its contents.
    ///
    /// This is the defence against session fixation. An attacker who can plant
    /// a cookie — through a subdomain, an open redirect, or a shared machine —
    /// hands the victim a session id he already knows, waits for the victim to
    /// log in, and then presents the same id to arrive inside the authenticated
    /// session. Rotating the id at the moment privileges change makes the id he
    /// planted worthless: it is no longer the id the session answers to.
    ///
    /// [`crate::Guard::login`] calls this for exactly that reason.
    pub fn regenerate(&mut self) -> &str {
        self.id = Session::new_id();
        // The CSRF token goes with the id, which it did not use to.
        //
        // Rotating the id stops an attacker who planted a session cookie from
        // riding the session afterwards — but the token he planted with it was
        // left in place, and it is the token the *authenticated* session then
        // answers to. He could drive state-changing requests from a page of his
        // own for the life of that session, which is most of what rotating the
        // id was meant to prevent. Laravel's `Store::regenerate` calls
        // `regenerateToken` for this reason.
        //
        // Forgotten rather than replaced: a session that never needed a token
        // should not be given one, and `token()` mints one when something
        // actually asks.
        self.forget(Session::TOKEN_KEY);
        self.dirty = true;
        &self.id
    }

    /// Throw the contents away *and* rotate the id — what logout wants.
    pub fn invalidate(&mut self) -> &str {
        self.flush();
        self.regenerate()
    }

    /// The CSRF token for this session, generated on first use.
    ///
    /// Living in the session is what makes it available to templates: a view
    /// renders `session.get("_token")` into a hidden field, and the `csrf`
    /// middleware compares what comes back against this same value.
    pub fn token(&mut self) -> String {
        if let Some(token) = self.get_string(Session::TOKEN_KEY) {
            return token;
        }
        let token = random::hex(ID_BYTES);
        self.put(Session::TOKEN_KEY, token.clone());
        token
    }

    /// Serialize for a store. The id is the key, so it is not in the payload.
    pub fn to_json(&self) -> Json {
        Json::object([
            ("data", Json::Object(self.data.clone())),
            (
                "flash",
                Json::Array(self.stale_flash.iter().cloned().map(Json::String).collect()),
            ),
            ("updated_at", Json::Number(self.updated_at as f64)),
        ])
    }

    /// Rebuild from a store's payload.
    ///
    /// A payload that is not an object at all yields an empty session rather
    /// than an error: a corrupt session file should log the visitor out, not
    /// take the site down.
    pub fn from_json(id: impl Into<String>, payload: &Json) -> Self {
        let mut session = Session::with_id(id);

        if let Some(Json::Object(data)) = payload.get("data") {
            session.data = data.clone();
        }
        if let Some(Json::Array(flash)) = payload.get("flash") {
            session.stale_flash =
                flash.iter().filter_map(|key| key.as_str().map(str::to_string)).collect();
        }
        if let Some(updated_at) = payload.get("updated_at").and_then(Json::as_f64) {
            session.updated_at = updated_at as u64;
        }

        session.dirty = false;
        session
    }

    /// Stamp the session as written now. Stores call this before persisting so
    /// an active visitor's session keeps sliding forward instead of expiring
    /// a fixed time after login.
    pub fn touch(&mut self) {
        self.updated_at = crate::unix_now();
    }
}

impl Default for Session {
    fn default() -> Self {
        Session::new()
    }
}

#[cfg(test)]
mod tests {

    /// Session fixation, the half that was missing.
    ///
    /// An attacker who can plant a cookie — a subdomain, an HTTP hop, a shared
    /// machine — seeds the victim with a session he made, and therefore with a
    /// `_token` he knows. Rotating the id on sign-in stopped him riding the
    /// session. It did not stop the token he knows from being the token the
    /// authenticated session answers to, which let him drive state-changing
    /// requests from a page of his own for the life of that session.
    #[test]
    fn rotating_the_id_rotates_the_csrf_token() {
        let mut session = Session::new();
        let planted = session.token();
        let before = session.id().to_string();

        session.regenerate();

        assert_ne!(session.id(), before, "the id did not rotate");
        assert_ne!(
            session.token(),
            planted,
            "the token survived the rotation, so a planted one still works after sign-in"
        );
    }

    /// And a session nobody asked a token of does not acquire one just by
    /// rotating: a token is minted when something needs it, not before.
    #[test]
    fn rotating_does_not_mint_a_token_nobody_asked_for() {
        let mut session = Session::new();
        session.regenerate();
        assert!(session.get_string(Session::TOKEN_KEY).is_none());
    }
    use super::*;

    #[test]
    fn new_sessions_get_a_long_random_hexadecimal_id() {
        let first = Session::new();
        let second = Session::new();

        assert_eq!(first.id().len(), ID_BYTES * 2);
        assert!(is_valid_id(first.id()));
        assert_ne!(first.id(), second.id());
    }

    #[test]
    fn id_validation_rejects_anything_that_is_not_our_own_shape() {
        assert!(is_valid_id(&Session::new_id()));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("short"));
        assert!(!is_valid_id("../../../etc/passwd"));
        // The right length, the wrong alphabet.
        assert!(!is_valid_id(&"g".repeat(64)));
        assert!(!is_valid_id(&"A".repeat(64)));
        assert!(!is_valid_id(&"a".repeat(129)));
    }

    #[test]
    fn values_are_put_read_and_forgotten() {
        let mut session = Session::new();
        assert!(!session.is_dirty());

        session.put("name", "Ada");
        session.put("visits", 3);

        assert_eq!(session.get_string("name").as_deref(), Some("Ada"));
        assert_eq!(session.get("visits").and_then(Json::as_i64), Some(3));
        assert!(session.has("name"));
        assert!(session.is_dirty());

        assert_eq!(session.forget("name"), Some(Json::from("Ada")));
        assert!(!session.has("name"));
        assert_eq!(session.forget("name"), None);
    }

    #[test]
    fn flush_empties_the_session_but_keeps_the_id() {
        let mut session = Session::new();
        let id = session.id().to_string();
        session.put("a", 1);
        session.flash("b", 2);

        session.flush();

        assert!(session.all().is_empty());
        assert_eq!(session.id(), id);
    }

    #[test]
    fn flash_data_survives_exactly_one_request() {
        // Request one flashes a message; it is readable straight away.
        let mut session = Session::new();
        session.flash("status", "Profile saved");
        assert_eq!(session.get_string("status").as_deref(), Some("Profile saved"));
        session.age_flash();

        // Request two — the redirect target — still sees it.
        assert_eq!(session.get_string("status").as_deref(), Some("Profile saved"));
        session.age_flash();

        // Request three does not.
        assert_eq!(session.get_string("status"), None);
    }

    #[test]
    fn ordinary_values_are_not_swept_away_with_the_flash() {
        let mut session = Session::new();
        session.put("user_id", 7);
        session.flash("status", "hi");

        session.age_flash();
        session.age_flash();

        assert_eq!(session.get("user_id").and_then(Json::as_i64), Some(7));
        assert_eq!(session.get_string("status"), None);
    }

    #[test]
    fn re_flashing_a_key_gives_it_another_request() {
        let mut session = Session::new();
        session.flash("status", "first");
        session.age_flash();

        // The next request flashes the same key again; the value must not be
        // swept away by the ageing that happens after it.
        session.flash("status", "second");
        session.age_flash();

        assert_eq!(session.get_string("status").as_deref(), Some("second"));
    }

    #[test]
    fn keep_and_reflash_extend_flashed_values() {
        let mut session = Session::new();
        session.flash("a", 1);
        session.flash("b", 2);
        session.age_flash();

        session.keep("a");
        session.age_flash();

        assert!(session.has("a"));
        assert!(!session.has("b"));

        session.flash("c", 3);
        session.age_flash();
        session.reflash();
        session.age_flash();

        assert!(session.has("c"));
    }

    #[test]
    fn regenerate_rotates_the_id_and_keeps_the_data() {
        let mut session = Session::new();
        session.put("user_id", 7);
        let before = session.id().to_string();

        let after = session.regenerate().to_string();

        assert_ne!(before, after);
        assert!(is_valid_id(&after));
        assert_eq!(session.get("user_id").and_then(Json::as_i64), Some(7));
    }

    #[test]
    fn invalidate_rotates_the_id_and_drops_the_data() {
        let mut session = Session::new();
        session.put("user_id", 7);
        let before = session.id().to_string();

        session.invalidate();

        assert_ne!(session.id(), before);
        assert!(session.all().is_empty());
    }

    #[test]
    fn the_csrf_token_is_generated_once_and_then_reused() {
        let mut session = Session::new();
        let first = session.token();

        assert_eq!(first.len(), ID_BYTES * 2);
        assert_eq!(session.token(), first);
        assert_eq!(session.get_string(Session::TOKEN_KEY), Some(first));
    }

    #[test]
    fn serialization_round_trips_data_and_pending_flash() {
        let mut session = Session::new();
        session.put("user_id", 7);
        session.flash("status", "saved");
        session.age_flash();

        let restored = Session::from_json(session.id(), &session.to_json());

        assert_eq!(restored.get("user_id").and_then(Json::as_i64), Some(7));
        assert_eq!(restored.get_string("status").as_deref(), Some("saved"));
        assert!(!restored.is_dirty(), "a freshly loaded session has not been changed yet");

        // And the restored flash still expires on schedule.
        let mut restored = restored;
        restored.age_flash();
        assert_eq!(restored.get_string("status"), None);
    }

    #[test]
    fn a_corrupt_payload_yields_an_empty_session_rather_than_an_error() {
        let session = Session::from_json(Session::new_id(), &Json::String("garbage".into()));

        assert!(session.all().is_empty());
        assert!(is_valid_id(session.id()));
    }
}
