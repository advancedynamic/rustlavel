//! Audit logging: who did what, to which record, from where, and when.
//!
//! Different from the application log on purpose. A log line is for whoever is
//! debugging; an audit entry is a record somebody may be asked about a year
//! later — "who deleted that account?" — and it therefore lives in the
//! database, beside the data it describes, rather than in a file that rotates.
//!
//! ```ignore
//! App::new().plugin(Audit::new(db.clone()))
//! // and in the registry the CLI generates:
//! registry.extend(rustlavel_audit::migrations());
//! ```
//!
//! Then, from anywhere with a request:
//!
//! ```ignore
//! use rustlavel_audit::AuditExt;
//!
//! req.audit("users.deleted")
//!     .on("User", user.id)
//!     .describe(format!("{} deleted {}", actor, user.name))
//!     .with("email", Json::from(user.email.as_str()))
//!     .save()
//!     .await?;
//! ```
//!
//! **What the builder does not let you do is forget the context.** The actor,
//! the address and the user agent are read off the request rather than passed
//! in, because an entry that says "someone updated the settings" answers
//! nothing, and a caller in a hurry is exactly who writes that entry.

pub mod entry;
pub mod plugin;
pub mod store;
pub mod tables;

pub use entry::{Builder, Entry};
pub use plugin::{Audit, AuditExt};
pub use store::{Filter, Page, Trail};
pub use tables::{TABLE, migrations};

/// What an application file usually needs from this package.
///
/// `Page`, `Entry`, `Builder` and `Filter` are deliberately not here: they are
/// ordinary enough names that a glob import of several preludes would make one
/// of them ambiguous — `rustlavel_db` already exports a `Page` — and a name
/// that silently means something else is worse than one extra `use`. Spell
/// them `rustlavel::audit::Page` when you need them.
pub mod prelude {
    pub use crate::plugin::{Audit, AuditExt};
    pub use crate::store::Trail;
}
