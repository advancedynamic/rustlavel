//! Credentials that change while the process is running.
//!
//! A password in `.env` is read once and never changes. A *dynamic* credential
//! — an account a secret store creates for this process and deletes when its
//! lease ends — does change, and a long-lived process has to cope with that
//! without restarting. This is the piece that lets it.
//!
//! # What actually breaks, and what does not
//!
//! Measured against PostgreSQL 16 rather than assumed, because the answer
//! decides the whole design:
//!
//! - A connection that is **already open** keeps working after its account is
//!   dropped. Authentication happens once, at connect time, and is not checked
//!   again per query. No query fails mid-flight, and no transaction is torn in
//!   half.
//! - A **new** connection with the old credentials is refused outright
//!   (`28P01 password authentication failed`).
//!
//! So rotation is not the frightening thing it sounds like. Nothing has to be
//! interrupted; the pool only has to stop *reusing* connections that belong to
//! a superseded credential, and open new ones with the current one. That is
//! what the generation counter here is for, and it is the same shape as
//! HikariCP's `softEvictConnections` — idle connections go now, busy ones go
//! when their borrower is finished with them.
//!
//! Retiring the old connections matters even though they still work: the point
//! of a short-lived credential is that access ends when the lease does, and a
//! pool quietly holding a session opened with a revoked account keeps that
//! access alive for as long as the process runs.
//!
//! # Wiring it up
//!
//! Deliberately not automatic, and deliberately not aware of any secret store —
//! this crate does not depend on `rustlavel-vault`, and an application may get
//! its credentials from somewhere this framework has never heard of:
//!
//! ```ignore
//! let credentials = Credentials::new("v-token-app-abc", "…");
//! let mut config = DatabaseConfig::from_url(&url)?;
//! config.credentials = Some(credentials.clone());
//! let db = Database::with_config(config).await?;
//!
//! // Wherever the new credential comes from, when the lease can no longer be
//! // renewed:
//! let fresh = vault.database().credentials("app").await?;
//! credentials.rotate(fresh.username, fresh.password);
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// A username and password that may be replaced while the process runs.
///
/// Cheap to clone — every clone shares one set of credentials, which is the
/// point: the task that fetches a new one and the pool that opens connections
/// with it are looking at the same value.
#[derive(Clone)]
pub struct Credentials {
    inner: Arc<Inner>,
}

struct Inner {
    current: RwLock<(String, String)>,
    /// Bumped on every rotation. A connection remembers the generation it was
    /// opened under, which is how the pool tells a usable connection from one
    /// belonging to a credential that has been replaced.
    generation: AtomicU64,
}

impl Credentials {
    pub fn new(user: impl Into<String>, password: impl Into<String>) -> Credentials {
        Credentials {
            inner: Arc::new(Inner {
                current: RwLock::new((user.into(), password.into())),
                generation: AtomicU64::new(1),
            }),
        }
    }

    /// The credentials to open the next connection with.
    pub fn current(&self) -> (String, String) {
        match self.inner.current.read() {
            Ok(current) => current.clone(),
            // A poisoned lock means a writer panicked mid-rotation. The value
            // is still a complete pair — the write is one assignment — and
            // refusing to connect over it would turn a panic somewhere else
            // into an outage here.
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn user(&self) -> String {
        self.current().0
    }

    /// Which generation the current credentials belong to.
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    /// Replace them, and retire every connection opened with the old ones.
    ///
    /// Returns the new generation. Rotating to the *same* values still counts
    /// as a rotation: a caller doing that is saying the old connections are no
    /// longer wanted, and second-guessing it here would silently keep them.
    pub fn rotate(&self, user: impl Into<String>, password: impl Into<String>) -> u64 {
        let pair = (user.into(), password.into());

        match self.inner.current.write() {
            Ok(mut current) => *current = pair,
            Err(poisoned) => *poisoned.into_inner() = pair,
        }

        // Released after the write, so nothing can read the new generation and
        // then the old credentials.
        self.inner.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Whether a connection opened under `generation` is still current.
    pub fn is_current(&self, generation: u64) -> bool {
        generation == self.generation()
    }
}

/// Names the user, never the password.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("user", &self.user())
            .field("password", &"<redacted>")
            .field("generation", &self.generation())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hands_out_what_it_was_given() {
        let credentials = Credentials::new("app", "s3cr3t");

        assert_eq!(credentials.current(), ("app".to_string(), "s3cr3t".to_string()));
        assert_eq!(credentials.user(), "app");
    }

    #[test]
    fn rotating_replaces_the_pair_and_moves_the_generation_on() {
        let credentials = Credentials::new("old-user", "old-pass");
        let first = credentials.generation();

        let second = credentials.rotate("new-user", "new-pass");

        assert_eq!(credentials.current(), ("new-user".to_string(), "new-pass".to_string()));
        assert_eq!(second, first + 1);
        assert!(!credentials.is_current(first), "connections from before must be retired");
        assert!(credentials.is_current(second));
    }

    #[test]
    fn every_clone_sees_the_rotation() {
        // The property the whole design rests on: the task fetching a new
        // credential and the pool opening connections hold the same value.
        let credentials = Credentials::new("old", "old");
        let held_elsewhere = credentials.clone();

        credentials.rotate("new", "new");

        assert_eq!(held_elsewhere.user(), "new");
        assert_eq!(held_elsewhere.generation(), credentials.generation());
    }

    #[test]
    fn rotating_to_the_same_values_still_retires_the_old_connections() {
        // Vault can hand out the same username twice in principle, and a caller
        // that rotates is saying "stop using what you have" either way.
        let credentials = Credentials::new("app", "same");
        let before = credentials.generation();

        credentials.rotate("app", "same");

        assert!(!credentials.is_current(before));
    }

    #[test]
    fn generations_keep_climbing_across_many_rotations() {
        let credentials = Credentials::new("a", "a");
        let start = credentials.generation();

        for round in 1..=100 {
            assert_eq!(credentials.rotate("a", "a"), start + round);
        }
    }

    #[test]
    fn debug_prints_the_user_but_never_the_password() {
        // The user is genuinely useful in a log — it is how you tell which
        // dynamic account a connection belongs to. The password is not.
        let printed = format!("{:?}", Credentials::new("v-token-app-abc", "s3cr3t"));

        assert!(printed.contains("v-token-app-abc"));
        assert!(!printed.contains("s3cr3t"), "the password reached a log: {printed}");
    }

    #[test]
    fn a_poisoned_lock_still_yields_credentials() {
        // A panic in unrelated code must not take the database down with it.
        let credentials = Credentials::new("app", "s3cr3t");
        let clone = credentials.clone();

        let _ = std::thread::spawn(move || {
            let _guard = clone.inner.current.write().unwrap();
            panic!("poisoning the lock");
        })
        .join();

        assert_eq!(credentials.user(), "app");
        assert_eq!(credentials.rotate("next", "next"), 2);
        assert_eq!(credentials.user(), "next");
    }
}
