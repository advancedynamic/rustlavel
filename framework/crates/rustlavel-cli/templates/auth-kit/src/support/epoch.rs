//! Making "sign out everywhere" actually sign anybody out.
//!
//! Three places rotate `users.session_epoch` — a password reset, a password
//! change on the profile, and the security settings — and each one tells the
//! person that their other devices have been signed out. Until this middleware
//! existed that sentence was false: the column was written by three
//! controllers, the matching `_epoch` key was written into the session by two
//! of them, and **nothing anywhere read either one**. Every other session
//! stayed exactly as valid as it had been.
//!
//! That is the worst kind of dead code. A reset is what somebody does when they
//! believe their account has been taken, and the one thing they are trying to
//! achieve is throwing the other party out. Promising it and not doing it is
//! worse than not offering it.
//!
//! The check is one comparison against a value the session already carries.
//!
//! **Belongs after `Authenticate` in a group**, for the same reason
//! [`crate::support::idle::IdleTimeout`] does: it has nothing to say about a
//! request that was never signed in.

use rustlavel::prelude::*;

use crate::models::user::User;
use crate::support::page;

/// The session key holding the epoch this session was opened under.
pub const EPOCH_KEY: &str = "_epoch";

/// Ends a session whose epoch no longer matches the account's.
pub struct SessionEpoch;

impl Middleware for SessionEpoch {
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response> {
        Box::pin(async move {
            if stale(&request).await {
                if let Some(session) = request.try_session() {
                    Guard::new(session.clone()).logout();
                }
                page::flash(
                    &request,
                    "info",
                    "You were signed out because the password on this account was changed.",
                );
                return Response::see_other("/login");
            }
            next.run(request).await
        })
    }
}

/// Whether this session was opened before the account's current epoch.
///
/// An account that has never rotated carries no epoch, and every session for it
/// is fine — that is what keeps sessions alive across the deploy that adds this
/// middleware, rather than signing out everybody at once. Once an account *has*
/// rotated, a session without the matching value is one of the sessions the
/// rotation was meant to end.
async fn stale(request: &Request) -> bool {
    let Some(identity) = request.identity() else { return false };
    let Some(user_id) = identity.id_as::<i64>() else { return false };
    let Some(db) = request.state::<Database>() else { return false };

    let Ok(Some(user)) = User::find(db, user_id).await else { return false };
    let Some(current) = user.session_epoch else { return false };

    let held = request
        .try_session()
        .and_then(|session| session.get_string(EPOCH_KEY));

    held.as_deref() != Some(current.as_str())
}
