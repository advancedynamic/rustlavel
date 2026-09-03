//! `crate::support::audit::of(&req, ...)` with the actor's name on it.
//!
//! The framework's `Identity` holds an id and nothing else, deliberately — it
//! is what the session can be trusted for. But an audit page that lists "User
//! #4" is a page somebody has to go and look four things up to read, so the
//! name belongs on the entry, recorded as it was at the time.
//!
//! It is put in the session when somebody signs in, which is the one moment
//! the name is already loaded. Reading it back costs nothing; looking the
//! account up on every recorded action would be a query per entry.

use rustlavel::prelude::*;
use rustlavel::audit::{AuditExt, Builder};

/// The session key holding the signed-in person's name.
pub const NAME_KEY: &str = "_name";

/// Begin an audit entry for this request, actor and all.
///
/// `None` when the audit plugin is not registered, the same as `req.audit`.
pub fn of(req: &Request, event: &str) -> Option<Builder> {
    let builder = req.audit(event)?;

    // The two are filled in independently, and that is not fussiness. On the
    // request that signs somebody in, the name is in the session but
    // `req.identity()` is still empty — the auth middleware read the session
    // before the login was written to it — so a version that needed both
    // would leave every "logged in" entry attributed to nobody. Which is
    // exactly the bug this replaced.
    let builder = match req.try_session().and_then(|session| session.get_string(NAME_KEY)) {
        Some(name) => builder.named(name),
        None => builder,
    };
    Some(builder)
}

/// Remember the name for later entries. Called once, on the way in.
pub fn remember(req: &Request, name: &str) {
    if let Some(session) = req.try_session() {
        session.put(NAME_KEY, Json::from(name));
    }
}

/// A timestamp as the audit page shows it.
///
/// PostgreSQL hands back `2026-09-03 04:13:27+00` and the other two do not.
/// The trailing offset is noise on every row — the whole trail is UTC — and
/// worse, it makes the same instant look different depending on the database.
pub fn stamp(at: &str) -> String {
    at.chars().take(19).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timezone_suffix_postgresql_adds_is_trimmed() {
        assert_eq!(stamp("2026-09-03 04:13:27+00"), "2026-09-03 04:13:27");
        assert_eq!(stamp("2026-09-03 04:13:27"), "2026-09-03 04:13:27");
        assert_eq!(stamp("2026-09-03 04:13:27.123456+00"), "2026-09-03 04:13:27");
        // Nothing sensible to trim, and nothing that panics either.
        assert_eq!(stamp(""), "");
        assert_eq!(stamp("2026"), "2026");
    }
}
