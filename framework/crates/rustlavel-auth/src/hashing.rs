//! Password hashing with argon2id.
//!
//! `hash_password` produces a PHC string — `$argon2id$v=19$m=...,t=...,p=...$salt$hash` —
//! which carries the algorithm, the version, the cost and the salt alongside
//! the digest. That is what lets [`verify_password`] keep working after the
//! cost is raised, and what lets [`needs_rehash`] notice that an old hash is
//! now under-provisioned.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use rustlavel_core::{Error, Result};

/// How much work one password hash costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    /// Memory used per hash, in kibibytes.
    pub memory_kib: u32,
    /// Passes over that memory.
    pub iterations: u32,
    /// Lanes hashed in parallel.
    pub parallelism: u32,
}

impl Cost {
    /// The default: 19 MiB of memory, two passes, one lane.
    ///
    /// This is OWASP's first recommended argon2id configuration. Memory is the
    /// parameter that actually defends against attackers: a GPU or ASIC can
    /// pipeline iterations cheaply but has to buy 19 MiB of fast memory per
    /// guess, which is what collapses their parallelism advantage. Two passes
    /// is the minimum argon2id allows at this memory size, and one lane keeps
    /// the cost predictable on a busy web server that is already running one
    /// request per core.
    ///
    /// On a modern server this lands around 40 ms per hash — slow enough to
    /// make offline cracking expensive, fast enough that a login request does
    /// not feel it.
    pub const DEFAULT: Cost = Cost { memory_kib: 19_456, iterations: 2, parallelism: 1 };

    /// The cheapest configuration argon2 accepts.
    ///
    /// Deliberately far too weak for real passwords. It exists because a test
    /// suite that hashes a dozen passwords at the real cost spends its whole
    /// runtime inside argon2, and a slow suite is a suite that stops being run.
    /// Never use this to store a password.
    pub const FAST: Cost = Cost { memory_kib: 8, iterations: 1, parallelism: 1 };

    fn params(self) -> Result<Params> {
        Params::new(self.memory_kib, self.iterations, self.parallelism, None)
            .map_err(|e| Error::msg(format!("invalid argon2 parameters: {e}")))
    }
}

impl Default for Cost {
    fn default() -> Self {
        Cost::DEFAULT
    }
}

/// Hash a password at the default cost.
pub fn hash_password(password: &str) -> Result<String> {
    hash_password_with(password, Cost::DEFAULT)
}

/// Hash a password at an explicit cost.
///
/// Every hash gets its own 16 bytes of salt from the OS CSPRNG, so two users
/// who chose the same password still get different digests and a precomputed
/// table buys an attacker nothing.
pub fn hash_password_with(password: &str, cost: Cost) -> Result<String> {
    let salt = SaltString::encode_b64(&crate::random::bytes(16))
        .map_err(|e| Error::msg(format!("could not encode a password salt: {e}")))?;

    let hasher = Argon2::new(Algorithm::Argon2id, Version::V0x13, cost.params()?);
    let hash = hasher
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| Error::msg(format!("could not hash the password: {e}")))?;

    Ok(hash.to_string())
}

/// Check a password against a stored hash.
///
/// Constant-time by construction: argon2 re-derives the digest using the
/// parameters recorded in `hash` and compares the two with a branch-free
/// equality check, so a wrong password takes the same time whether it differs
/// in the first byte or the last. A malformed or unrecognised hash is simply
/// `false` — an unparseable value in the database must never authenticate
/// anybody.
pub fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };

    Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
}

/// Whether a stored hash was produced below the cost we now want.
///
/// Call it after a successful login: the plaintext password is in hand exactly
/// once per session, which is the only moment an old hash can be upgraded.
pub fn needs_rehash(hash: &str, cost: Cost) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        // Unreadable, or hashed by some other algorithm entirely: rehash it.
        return true;
    };
    if parsed.algorithm != Algorithm::Argon2id.ident() {
        return true;
    }

    match Params::try_from(&parsed) {
        Ok(params) => {
            params.m_cost() < cost.memory_kib
                || params.t_cost() < cost.iterations
                || params.p_cost() != cost.parallelism
        }
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hashed_password_verifies_against_itself() {
        let hash = hash_password_with("correct horse battery staple", Cost::FAST).unwrap();

        assert!(verify_password("correct horse battery staple", &hash));
    }

    #[test]
    fn the_wrong_password_is_rejected() {
        let hash = hash_password_with("s3cret", Cost::FAST).unwrap();

        assert!(!verify_password("s3cre", &hash));
        assert!(!verify_password("s3cret ", &hash));
        assert!(!verify_password("", &hash));
        assert!(!verify_password("S3cret", &hash));
    }

    #[test]
    fn the_same_password_hashes_differently_every_time() {
        let first = hash_password_with("repeated", Cost::FAST).unwrap();
        let second = hash_password_with("repeated", Cost::FAST).unwrap();

        assert_ne!(first, second, "each hash must carry its own salt");
        assert!(verify_password("repeated", &first));
        assert!(verify_password("repeated", &second));
    }

    #[test]
    fn hashes_are_phc_strings_naming_argon2id() {
        let hash = hash_password_with("x", Cost::FAST).unwrap();

        assert!(hash.starts_with("$argon2id$v=19$"), "unexpected hash format: {hash}");
        assert!(hash.contains("m=8,t=1,p=1"), "cost should be recorded in the hash: {hash}");
    }

    #[test]
    fn a_corrupt_or_foreign_hash_never_authenticates() {
        assert!(!verify_password("anything", ""));
        assert!(!verify_password("anything", "not-a-hash"));
        assert!(!verify_password("anything", "$argon2id$v=19$m=8,t=1,p=1$c2FsdA$"));
        // A bcrypt hash from some other system must not be treated as a match.
        assert!(!verify_password("anything", "$2y$10$abcdefghijklmnopqrstuv"));
    }

    #[test]
    fn a_hash_verifies_even_though_the_default_cost_is_higher() {
        // Verification uses the parameters stored in the hash, not the current
        // defaults, so raising the cost does not lock existing users out.
        let cheap = hash_password_with("legacy", Cost::FAST).unwrap();

        assert!(verify_password("legacy", &cheap));
        assert!(needs_rehash(&cheap, Cost::DEFAULT));
        assert!(!needs_rehash(&cheap, Cost::FAST));
    }

    #[test]
    fn unreadable_hashes_are_reported_as_needing_a_rehash() {
        assert!(needs_rehash("", Cost::DEFAULT));
        assert!(needs_rehash("$2y$10$abcdefghijklmnopqrstuv", Cost::DEFAULT));
    }

    #[test]
    fn the_default_cost_produces_a_working_hash() {
        // The one test that pays the real cost, so the shipped parameters are
        // proven to be accepted by argon2 rather than merely plausible.
        let hash = hash_password("default cost").unwrap();

        assert!(verify_password("default cost", &hash));
        assert!(!verify_password("wrong", &hash));
        assert!(!needs_rehash(&hash, Cost::DEFAULT));
    }
}
