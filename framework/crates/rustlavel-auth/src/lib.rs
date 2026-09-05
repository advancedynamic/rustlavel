//! rustlavel-auth: sessions, API tokens, password hashing, CSRF, signed URLs.
//!
//! Everything an application needs to know who is talking to it and to trust
//! what they send. Like every rustlavel package it is opt-in: an application
//! that never adds it never compiles a line of this.
//!
//! ```ignore
//! use rustlavel_auth::prelude::*;
//!
//! let sessions = SessionManager::from_config(&config, FileStore::new("storage/sessions"))?;
//! router.middleware(sessions);
//! router.middleware(Csrf::new());
//!
//! router.group("/dashboard", |r| {
//!     r.middleware(Authenticate::from_config(&config));
//!     r.get("", |req: Request| async move {
//!         format!("hello, {}", req.identity().unwrap().id())
//!     });
//! });
//! ```
//!
//! # Why the crypto crates are here
//!
//! The framework builds from scratch on principle, and this crate follows that
//! everywhere except the primitives: argon2, AES-GCM, HMAC and SHA-256 come
//! from the RustCrypto crates. A hand-written cipher or KDF looks like it works
//! from the first test and is a vulnerability regardless — this is the one
//! place where writing less code is the safer engineering decision.

pub mod base64;
pub mod csrf;
pub mod encryption;
pub mod guard;
pub mod hashing;
pub mod impersonation;
pub mod key;
pub mod middleware;
pub mod qr;
pub mod random;
#[cfg(feature = "redis")]
pub mod redis_session;
pub mod session;
pub mod signed_url;
pub mod store;
pub mod tokens;
pub mod totp;

pub use csrf::Csrf;
pub use encryption::Encrypter;
pub use guard::{AuthExt, Authenticatable, Authenticate, Guard, Guest, Identity};
pub use hashing::{Cost, hash_password, hash_password_with, needs_rehash, verify_password};
pub use impersonation::{Impersonation, ImpersonationExt, Impersonator};
pub use key::AppKey;
pub use middleware::{SessionExt, SessionHandle, SessionManager};
pub use session::Session;
pub use signed_url::{SignatureError, UrlSigner};
pub use store::{FileStore, MemoryStore, SessionStore, SharedStore};
pub use tokens::{
    MemoryTokenStore, NewToken, PlainTextToken, RequireApiToken, RequireScope, Scopes,
    SharedTokenStore, Token, TokenError, TokenExt, TokenStore,
};

pub use rustlavel_core::{Error, Result};

/// What a `routes/web.rs` or a controller imports to use this package.
pub mod prelude {
    pub use crate::{
        AuthExt, Authenticatable, Authenticate, Cost, Csrf, Encrypter, Guard, Guest, Identity,
        Session, SessionExt, SessionHandle, SessionManager, SignatureError, UrlSigner,
        hash_password, verify_password,
    };
    pub use crate::{FileStore, MemoryStore, SessionStore};
    pub use crate::{
        MemoryTokenStore, NewToken, PlainTextToken, RequireApiToken, RequireScope, Scopes, Token,
        TokenError, TokenExt, TokenStore,
    };
}

/// Compare two byte strings without leaking where they first differ.
///
/// The obvious `a == b` returns as soon as it finds a mismatch, so how long it
/// took is a measurement of how much of the secret the caller guessed
/// correctly. Repeat that a few thousand times and a token can be recovered one
/// byte at a time. Every secret comparison in this crate — CSRF tokens, URL
/// signatures, session-cookie MACs — goes through here.
///
/// Unequal lengths are reported as unequal immediately: a length is not a
/// secret, and pretending otherwise would mean comparing past the end of a
/// buffer.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;

    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// The current time in unix seconds.
///
/// Clamped at the epoch rather than panicking: a machine whose clock is set
/// before 1970 should expire every signed URL, not take the process down.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_comparison_agrees_with_ordinary_equality() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"token", b"token"));

        assert!(!constant_time_eq(b"token", b"tokeN"));
        assert!(!constant_time_eq(b"token", b"Token"));
        assert!(!constant_time_eq(b"token", b"token "));
        assert!(!constant_time_eq(b"token", b""));
    }

    #[test]
    fn the_clock_is_past_the_epoch_and_moves_forward() {
        let now = unix_now();

        // Somewhere after 2020; a value of 0 would mean every signed URL is
        // already expired, which is a failure worth noticing here.
        assert!(now > 1_577_836_800, "the system clock reported {now}");
        assert!(unix_now() >= now);
    }
}
