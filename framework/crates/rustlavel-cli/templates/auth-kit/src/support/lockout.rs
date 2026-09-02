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

/// Wrong passwords before an account locks.
pub const MAX_ATTEMPTS: i64 = 5;

/// How long it stays locked. Long enough to make guessing pointless, short
/// enough that a person who fat-fingered their password five times is not
/// filing a support ticket.
pub const LOCK_FOR: Duration = Duration::from_secs(15 * 60);

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
pub async fn record_failure(db: &Database, user: &mut User, now: &str) -> Result<bool> {
    user.failed_attempts += 1;

    if user.failed_attempts >= MAX_ATTEMPTS {
        user.locked_until = Some(crate::support::tokens::in_future(now, LOCK_FOR));
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
