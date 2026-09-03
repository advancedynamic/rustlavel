//! Refusing a password somebody has had before.
//!
//! Settings → Security → *Password Reuse Prevention* says how many to
//! remember. Zero — the default — keeps no history at all, which is the
//! honest default: reuse rules push people towards a counter on the end of one
//! password rather than towards a different one, and NIST SP 800-63B advises
//! against them. They are here because some organisations are required to have
//! them.

use rustlavel::prelude::*;

use crate::models::password_history::PasswordHistory;
use crate::support::settings::Settings;

/// How many past passwords to refuse. Zero means the feature is off.
pub async fn keep(req: &Request) -> i64 {
    let raw = match req.state::<Settings>() {
        Some(settings) => settings.get("auth.password.reuse").await,
        None => req.config().string("auth.password.reuse", "0"),
    };
    raw.parse::<i64>().unwrap_or(0).clamp(0, 24)
}

/// Whether this password is one of the last `keep` this person used.
///
/// The password on the account right now counts as one of them, and is checked
/// first: an account that predates this setting has no history rows at all, and
/// without this an administrator could switch reuse prevention on and still
/// have everybody re-set the password they already had.
///
/// Each comparison is a full argon2 verification, which is deliberately slow —
/// hence the cap on `keep` and the early return when the feature is off.
pub async fn was_used_before(db: &Database, user_id: i64, password: &str, keep: i64) -> Result<bool> {
    if keep == 0 {
        return Ok(false);
    }

    if let Some(current) = crate::models::user::User::find(db, user_id)
        .await?
        .and_then(|user| user.password_hash)
    {
        if rustlavel::auth::verify_password(password, &current) {
            return Ok(true);
        }
    }

    let history = PasswordHistory::get(db, PasswordHistory::for_user(user_id).limit(keep)).await?;
    Ok(history.iter().any(|row| rustlavel::auth::verify_password(password, &row.password_hash)))
}

/// Push the password this account is leaving behind into its history.
///
/// Called with the *outgoing* hash, before the new one is written. The new one
/// needs no row of its own: it becomes `users.password_hash`, which
/// [`was_used_before`] checks directly. Recording the incoming hash instead —
/// the obvious thing to write — leaves the password somebody actually just
/// stopped using as the one password they are still allowed to go back to.
///
/// `keep` counts the current password as one of them, so the history holds
/// `keep - 1` rows and the two together are the `keep` a person is refused.
pub async fn remember_previous(db: &Database, user_id: i64, outgoing: Option<&str>, keep: i64) -> Result<()> {
    let (Some(hash), true) = (outgoing, keep > 0) else {
        return Ok(());
    };
    PasswordHistory { user_id, password_hash: hash.to_string(), ..Default::default() }
        .insert(db)
        .await?;

    // Older rows are dropped rather than left to grow: they answer no question
    // once they are outside the window, and a password hash kept for no reason
    // is a password hash that can still leak.
    let window = (keep - 1).max(1);
    let kept = PasswordHistory::get(db, PasswordHistory::for_user(user_id).limit(window)).await?;
    if let Some(oldest) = kept.last() {
        PasswordHistory::query()
            .filter("user_id", user_id)
            .filter_op("id", "<", oldest.id)
            .delete(db)
            .await?;
    }
    Ok(())
}

/// The message a refused password gets. One sentence, and it says what to do.
pub fn reuse_message(keep: i64) -> String {
    format!("That is one of your last {keep} passwords. Choose one you have not used here before.")
}
