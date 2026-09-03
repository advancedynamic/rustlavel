//! Attaching the package to an application, and reaching it from a handler.
//!
//! ```ignore
//! App::new().plugin(Audit::new(db.clone()))
//! ```
//!
//! The migrations are not registered from here: they belong in the registry
//! the CLI generates, next to the application's own. Add them with
//! `registry.extend(rustlavel_audit::migrations());`

use crate::entry::{Builder, Entry};
use crate::store::Trail;
use rustlavel_auth::guard::AuthExt;
use rustlavel_db::Database;
use rustlavel_http::Request;
use rustlavel_http::plugin::{Plugin, Setup};

/// Registers the [`Trail`] into application state.
pub struct Audit {
    trail: Trail,
}

impl Audit {
    pub fn new(db: Database) -> Audit {
        Audit { trail: Trail::new(db) }
    }

    pub fn with_trail(trail: Trail) -> Audit {
        Audit { trail }
    }

    pub fn trail(&self) -> &Trail {
        &self.trail
    }
}

impl Plugin for Audit {
    fn name(&self) -> &'static str {
        "audit"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        setup.state(self.trail.clone());
    }
}

/// `req.audit(...)` — an entry with the request's context already on it.
pub trait AuditExt {
    /// The trail, or `None` when the plugin is not registered.
    fn trail(&self) -> Option<&Trail>;

    /// Begin an entry, with the actor's id, the address and the user agent
    /// filled in from this request.
    ///
    /// The actor's *name* is not, and cannot be: the framework's `Identity`
    /// holds an id and nothing else, on purpose. The caller adds it with
    /// [`Builder::by`], from whatever it is already holding.
    ///
    /// Returns `None` rather than a silent no-op builder when there is no
    /// trail: a caller that meant to record something should find out that it
    /// did not, and `let Some(audit) = req.audit(..)` is where they find out.
    fn audit(&self, event: &str) -> Option<Builder>;
}

impl AuditExt for Request {
    fn trail(&self) -> Option<&Trail> {
        self.state::<Trail>()
    }

    fn audit(&self, event: &str) -> Option<Builder> {
        let trail = self.state::<Trail>()?;

        let mut entry = Entry::new(event);
        // Read off the request rather than passed in. An entry that says
        // "someone updated the settings" answers nothing, and a caller in a
        // hurry is exactly who writes that entry.
        if let Some(identity) = self.identity() {
            entry.user_id = identity.id_as::<i64>();
        }
        entry.ip_address = self.ip();
        entry.user_agent = self.header("user-agent").map(|agent| agent.to_string());

        Some(Builder { trail: trail.clone(), entry })
    }
}
