//! Locking an account that is being guessed at.
//!
//! Two limits, because they answer different questions and either alone leaves
//! a hole.
//!
//! The **per-account** lock is the one people mean: after enough wrong
//! passwords the account stops accepting any, for a while. It is stored on the
//! user row, so it survives a restart and applies wherever the attempt comes
//! from — an attacker rotating through a botnet is still hitting one account.
//!
//! The **per-address** limit is the other half. Without it, somebody trying one
//! common password against ten thousand accounts never trips a single account's
//! counter, and that is the attack that actually works. It is counted in the
//! cache, keyed by address, and it is why this file needs both.
//!
//! Neither is a substitute for a second factor. A lock buys time; it does not
//! make a guessed password wrong.

use rustlavel::prelude::*;
use std::time::Duration;

use crate::models::user::User;
use crate::support::settings::Settings;

/// Wrong passwords before an account locks.
pub const MAX_ATTEMPTS: i64 = 5;

/// How long it stays locked. Long enough to make guessing pointless, short
/// enough that a person who fat-fingered their password five times is not
/// filing a support ticket.
pub const LOCK_FOR: Duration = Duration::from_secs(15 * 60);

/// The two per-account limits, as Settings → Security has them.
///
/// The constants above are the fallback rather than the rule: a request with no
/// [`Settings`] in state — a test client, or a project that has not run the
/// settings migration yet — still locks accounts, it just locks them on the
/// numbers compiled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Wrong passwords before the account locks. **Zero means no limit**, and
    /// the Settings page says so: it is offered because somebody will ask for
    /// it, not because it is a good idea.
    pub attempts: i64,
    pub lock_for: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { attempts: MAX_ATTEMPTS, lock_for: LOCK_FOR }
    }
}

impl Limits {
    /// What this request should lock on.
    pub async fn current(req: &Request) -> Limits {
        let Some(settings) = req.state::<Settings>() else { return Limits::default() };

        // Clamped rather than trusted. The values come from a `<select>` built
        // from the catalogue, but a setting is a database row and a database
        // row is not a promise — a negative count would read as "no limit" and
        // quietly turn the lock off.
        let attempts = settings.int("auth.lockout.attempts", MAX_ATTEMPTS).await.max(0);
        let minutes = settings
            .int("auth.lockout.minutes", LOCK_FOR.as_secs() as i64 / 60)
            .await
            .clamp(1, 7 * 24 * 60);

        Limits { attempts, lock_for: Duration::from_secs(minutes as u64 * 60) }
    }

    /// Whether `failures` wrong passwords is enough to lock the account.
    ///
    /// With no limit set, nothing is ever enough. The per-address counter below
    /// is then the only thing standing between an account and an unbounded
    /// guessing run, which is exactly why the choice is labelled "not advised".
    pub fn locks_at(&self, failures: i64) -> bool {
        self.attempts > 0 && failures >= self.attempts
    }
}

/// Failed attempts allowed from one address in a window, across all accounts.
pub const MAX_PER_ADDRESS: u64 = 20;
pub const ADDRESS_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Whether this address has been failing too often, whoever it is trying.
pub async fn address_is_blocked(req: &Request) -> bool {
    let Some(cache) = req.state::<CacheStore>() else { return false };
    let key = format!("login-failures:{}", req.ip().unwrap_or_else(|| "unknown".into()));
    // A cache that is down must not lock everybody out, so a failure here
    // fails open — the per-account lock still applies, and it is the one
    // stored durably for exactly this reason.
    matches!(
        cache.driver_handle().get(&key).await,
        Ok(Some(Json::Number(count))) if count as u64 >= MAX_PER_ADDRESS
    )
}

/// Count one failure against the address.
pub async fn record_address_failure(req: &Request) {
    let Some(cache) = req.state::<CacheStore>() else { return };
    let key = format!("login-failures:{}", req.ip().unwrap_or_else(|| "unknown".into()));
    // `increment_within` gives the key its lifetime only when it creates it, so
    // the twentieth failure does not restart the clock the first one started.
    let _ = cache.driver_handle().increment_within(&key, 1, ADDRESS_WINDOW).await;
}

/// Count one failure against the account, locking it at the threshold.
///
/// Returns whether this failure was the one that locked it.
///
/// The request is here for the limits, which come from the Settings page: the
/// same failure locks an account after three attempts on one deployment and
/// after ten on another, and neither of them recompiles to say so.
pub async fn record_failure(
    db: &Database,
    user: &mut User,
    req: &Request,
    now: &str,
) -> Result<bool> {
    let limits = Limits::current(req).await;
    user.failed_attempts += 1;

    // The count is kept even with no limit set, because turning the limit back
    // on should not start everybody from zero — and the sign-in log reads it.
    if limits.locks_at(user.failed_attempts) {
        user.locked_until = Some(crate::support::tokens::in_future(now, limits.lock_for));
        user.update(db).await?;
        return Ok(true);
    }
    user.update(db).await?;
    Ok(false)
}

/// Clear the counters after a successful sign-in.
///
/// Both of them: leaving the count behind means a person who mistyped their
/// password four times last week is one typo from being locked out today.
pub async fn record_success(db: &Database, user: &mut User, req: &Request, now: &str) -> Result<()> {
    user.failed_attempts = 0;
    user.locked_until = None;
    user.last_login_at = Some(now.to_string());
    user.last_login_ip = req.ip();
    user.update(db).await?;

    if let Some(cache) = req.state::<CacheStore>() {
        let key = format!("login-failures:{}", req.ip().unwrap_or_else(|| "unknown".into()));
        let _ = cache.driver_handle().forget(&key).await;
    }
    Ok(())
}

/// How much longer a lock has to run, in words.
pub fn remaining(locked_until: &str, now: &str) -> String {
    let minutes = crate::support::tokens::minutes_between(now, locked_until);
    match minutes {
        0 => "less than a minute".to_string(),
        1 => "1 minute".to_string(),
        n => format!("{n} minutes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::tokens;

    /// A `Limits` as [`Limits::current`] would build it from two settings,
    /// without the request and the database a real one needs.
    fn configured(attempts: i64, minutes: u64) -> Limits {
        Limits { attempts, lock_for: Duration::from_secs(minutes * 60) }
    }

    #[test]
    fn the_threshold_is_the_configured_count() {
        let limits = configured(3, 15);
        assert!(!limits.locks_at(2));
        assert!(limits.locks_at(3));
        assert!(limits.locks_at(4), "a count past the threshold is still locked");

        // And it is genuinely the setting, not the constant underneath it.
        assert_ne!(configured(10, 15).attempts, MAX_ATTEMPTS);
        assert!(!configured(10, 15).locks_at(MAX_ATTEMPTS));
        assert!(configured(10, 15).locks_at(10));
    }

    #[test]
    fn no_limit_never_locks() {
        let limits = configured(0, 15);
        for failures in [1, 5, 20, 5_000] {
            assert!(!limits.locks_at(failures), "{failures} failures locked an account with no limit");
        }
    }

    #[test]
    fn the_fallback_is_the_compiled_in_pair() {
        // What a request with no `Settings` in state gets.
        let limits = Limits::default();
        assert_eq!(limits.attempts, MAX_ATTEMPTS);
        assert_eq!(limits.lock_for, LOCK_FOR);
        assert!(limits.locks_at(MAX_ATTEMPTS));
    }

    #[test]
    fn a_locked_account_reports_the_configured_duration() {
        let now = "2026-09-03 09:00:00";

        for (minutes, expected) in [(5, "5 minutes"), (15, "15 minutes"), (60, "60 minutes")] {
            let limits = configured(5, minutes);
            let until = tokens::in_future(now, limits.lock_for);
            assert_eq!(remaining(&until, now), expected);
        }
    }

    #[test]
    fn a_lock_that_has_nearly_run_out_says_so() {
        let now = "2026-09-03 09:00:00";
        assert_eq!(remaining("2026-09-03 09:00:30", now), "less than a minute");
        assert_eq!(remaining("2026-09-03 09:01:00", now), "1 minute");

        // An expired lock does not count backwards.
        assert_eq!(remaining("2026-09-03 08:00:00", now), "less than a minute");
    }
}
