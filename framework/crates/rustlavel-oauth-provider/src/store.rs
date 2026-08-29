//! What the four stores share, and how a credential is kept at rest.
//!
//! An authorisation server persists four things: registered clients, live
//! authorisation codes, issued tokens, and the consent a user has already
//! given. Each is a trait in its own module with an in-memory implementation
//! beside it.
//!
//! # Why there is no database here
//!
//! This crate deliberately does **not** depend on `rustlavel-db`. An
//! application that is ready to be an OAuth provider already has migrations, a
//! connection pool, and an opinion about what its `oauth_clients` table should
//! look like — whether client ids are UUIDs or slugs, whether a deleted client
//! is a row or a flag, which tenant a client belongs to. Inventing a schema
//! here would mean every application either accepted those answers or fought
//! them. Implementing four small traits against tables you already own is less
//! work than either, and it leaves the crate usable from an application with no
//! database at all.
//!
//! The in-memory stores are the right answer for tests and a single-process
//! development server, and the wrong answer for anything that restarts or runs
//! more than one worker — a restart would silently un-revoke every token.

use rustlavel_core::Result;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::pin::Pin;

/// What a store's operations return.
///
/// A boxed future rather than an `async fn` because every store has to be
/// usable as `dyn ClientStore` and friends: the server holds whichever driver
/// the application picked, and cannot be generic over four of them without
/// putting four type parameters on every handler that touches it.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// The lowercase-hex SHA-256 of a credential, which is what gets stored.
///
/// Client secrets, authorisation codes, access tokens and refresh tokens are
/// all held this way. Nothing in the database is a usable credential, so a
/// leaked dump is not a leaked set of tokens, and a token can be looked up by
/// hashing what the client presented — no scan, no decryption key to protect.
///
/// # Why SHA-256 and not argon2
///
/// Argon2 is the right answer for a *password*, which is short, chosen by a
/// human, and therefore guessable. None of these values are: every one is 256
/// bits from the OS CSPRNG, so there is nothing to guess and no dictionary to
/// run. What argon2 would add is its deliberate cost — tens of milliseconds and
/// tens of megabytes — on the token endpoint, on the resource-server path, and
/// on every introspection call. That is not defence in depth, it is a
/// denial-of-service an attacker triggers by sending requests.
///
/// The rule this follows: hash passwords slowly, hash random secrets fast.
pub fn digest(secret: &str) -> String {
    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(secret.as_bytes()) {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// Compare a presented credential against a stored digest.
///
/// Hashing first is not enough on its own: comparing the two digests with `==`
/// returns as soon as they differ, and the timing of that is a measurement of
/// how many leading bytes matched. It is a weaker oracle than comparing the raw
/// secrets, but it is still an oracle, and there is no reason to leave it open.
pub fn digest_matches(presented: &str, stored: &str) -> bool {
    rustlavel_auth::constant_time_eq(digest(presented).as_bytes(), stored.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_a_known_sha256_vector() {
        // The empty string, so a wrong hash function is caught immediately
        // rather than only when two of our own values disagree.
        assert_eq!(
            digest(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(digest("abc").len(), 64);
    }

    #[test]
    fn the_digest_is_not_the_secret() {
        assert_ne!(digest("s3cret"), "s3cret");
        assert_ne!(digest("s3cret"), digest("s3creT"));
    }

    #[test]
    fn a_presented_secret_is_checked_against_its_stored_digest() {
        let stored = digest("s3cret");

        assert!(digest_matches("s3cret", &stored));
        assert!(!digest_matches("s3creT", &stored));
        assert!(!digest_matches("", &stored));
        // And the stored form is never accepted as the secret itself.
        assert!(!digest_matches(&stored, &stored));
    }
}
