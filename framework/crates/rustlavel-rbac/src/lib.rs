//! Roles and permissions — the authorization half of the auth starter kit.
//!
//! Modelled on `laravel-permission`, which is the package Laravel developers
//! actually reach for, and shaped so the same mental model transfers: roles are
//! named bundles of permissions, users hold roles, and a user can also be given
//! — or refused — a single permission on their own.
//!
//! ```ignore
//! // main.rs
//! App::new()
//!     .plugin(Rbac::from_config(db.clone(), &config)?)
//!     .routes(|r| {
//!         r.group("/admin", |r| {
//!             r.middleware(Authenticate::from_config(&config));
//!             r.middleware(Can::permission("admin.access"));
//!             r.get("", dashboard);
//!         });
//!     });
//! ```
//!
//! # The model
//!
//! Five tables. `roles` and `permissions` are lists of names; `role_permission`
//! attaches a permission to a role; `user_role` gives a user a role; and
//! `user_permission` gives — or refuses — a permission to one user directly,
//! which is what its `granted` boolean is for.
//!
//! Two ways to hold a permission, then: through a role, or in your own name.
//!
//! # Precedence
//!
//! The question this package exists to answer is "why can this user still do
//! X?", so the answer is one short list, applied in order:
//!
//! 1. **An explicit direct deny beats everything**, including a super role.
//! 2. A **super role** passes, with no permissions attached.
//! 3. A **grant** passes — from a role or made directly; the two rank equally,
//!    so a direct grant beats the *absence* of a role grant.
//! 4. Otherwise, no. Silence is never permission.
//!
//! The one that surprises people is the first: a deny outranks `super-admin`.
//! It has to. A deny is the only entry an administrator writes in order to say
//! "no, not this one", and a rule that some role can quietly overrule is not a
//! rule. The trade is that a super role is not unstoppable — which is a
//! property worth having, not a hole.
//!
//! See [`Grants::allows`] for the implementation and the tests that pin it.
//!
//! # Wildcards
//!
//! A stored permission may end in `.*`, and `*` on its own matches everything.
//! So a role holding `users.*` satisfies a check for `users.create`. The
//! matching is a segment-wise prefix comparison and nothing more — no regular
//! expressions, no glob syntax, no `*` in the middle. An authorization rule
//! whose meaning cannot be worked out by reading it is worse than one that
//! cannot express every case. [`permission_matches`] has the details.
//!
//! # The super role
//!
//! A role named `super-admin` (configure it with `rbac.super_role`) passes
//! every check without holding a single permission. It is what makes the first
//! user of a fresh application able to do anything, and it should be treated
//! with suspicion after that.
//!
//! **The risk, plainly: a super role cannot be audited by listing its
//! permissions, because it has none.** Reading `role_permissions("super-admin")`
//! returns an empty list, and that empty list is not what the role can do — it
//! is the opposite. Nothing shows up in `permissions_for(user)` either. If your
//! compliance story is "here is what each role may do", a super role is a hole
//! in it, and the fix is to give the role real permissions (`*` is one) and
//! turn the escape hatch off with `.super_role("")`.
//!
//! # Caching
//!
//! An authorization check happens on every guarded request, so a check that
//! queries the database every time is the slowest thing in the application. A
//! user's resolved grants are cached in memory for 30 seconds
//! (`rbac.cache_ttl_ms`), and every method that changes what a user is allowed
//! invalidates it immediately — an administrator removing a role sees it take
//! effect on the next request, not in half a minute. The TTL is only the
//! backstop for a change made by *another* process; it is not the mechanism.
//!
//! The cache lives in the [`Permissions`] handle, and cloning one shares it.
//! Register it once, in application state.

pub mod grants;
pub mod guard;
pub mod plugin;
pub mod store;
pub mod tables;

pub use grants::{Grants, permission_matches};
pub use guard::{Can, RbacExt};
pub use plugin::Rbac;
pub use store::{DEFAULT_CACHE_TTL, DEFAULT_SUPER_ROLE, Named, Permissions};
pub use tables::{CreateRbacTables, TableNames, create_tables, drop_tables, migrations};

pub use rustlavel_core::{Error, Result};

/// Everything an application usually wants: `use rustlavel_rbac::prelude::*;`
pub mod prelude {
    pub use crate::{Can, Grants, Permissions, Rbac, RbacExt, TableNames, migrations};
}
