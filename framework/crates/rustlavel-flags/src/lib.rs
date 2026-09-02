//! Feature flags — deciding at run time what a given request sees.
//!
//! This is Laravel Pennant's job, done for this framework: a named switch, a
//! bit of code that decides it for one user, somewhere to write down an
//! operator's override, and a route guard.
//!
//! # These are not Cargo features
//!
//! The two are easy to confuse and they solve opposite problems.
//!
//! A **Cargo feature** is compile time. `rustlavel = { features = ["flags"] }`
//! decides what is *in the binary*; code behind a feature that is off does not
//! exist in the artifact, cannot be called, and costs nothing. Changing one
//! means a rebuild and a deploy, and the answer is the same for every request
//! the binary ever serves.
//!
//! A **flag in this crate** is run time. It decides what *this request* sees,
//! out of code that is already compiled in and shipped. Both branches are in
//! the binary — that is the price — and in exchange the answer can differ per
//! user, change while the process is running, and be turned off by somebody who
//! is not waiting for CI.
//!
//! Use a Cargo feature to leave a whole package out. Use one of these to ship
//! a half-finished checkout to nobody, then to your own account, then to a
//! quarter of your users, then to everybody, without four deploys.
//!
//! # The shape of it
//!
//! ```ignore
//! // main.rs
//! let flags = Flags::new()
//!     .define("new-checkout", |scope| async move { scope.id().ends_with('7') })
//!     .define("beta-search", |_| async { false })
//!     .rollout("dark-mode", 25);
//!
//! App::new()
//!     .plugin(FeatureFlags::from_config(flags, &config))
//!     .routes(|r| {
//!         r.group("/checkout", |r| {
//!             r.middleware(WhenActive::new("new-checkout"));
//!             r.post("", place_order);
//!         });
//!
//!         r.get("/search", |req: Request| async move {
//!             let beta = req.flag("beta-search").await?;
//!             Ok(Response::html(render(beta)))
//!         });
//!     });
//! ```
//!
//! # The four pieces
//!
//! * A [`Scope`] is who a flag is being checked for: a user, a tenant, or
//!   [`Scope::none`] for the whole installation.
//! * [`Flags`] holds the definitions and answers the question. A per-request
//!   [`ScopedFlags`] view remembers what it has resolved, so a flag backed by a
//!   slow lookup costs its slowness once a request rather than once a check.
//! * A [`FlagStore`] holds **overrides** — an operator's decision, which beats
//!   whatever the resolver computes. [`MemoryStore`] is the one that ships.
//! * [`WhenActive`] hides a route behind a flag, and [`FlagsExt`] lets a
//!   handler ask.
//!
//! # Precedence
//!
//! The question a flag system exists to answer is "why is this on for this
//! user?", so the answer is one short list. It is written out in full, with the
//! reasoning, on [`Flags`]; the summary is that **`flags.off` in configuration
//! beats everything**, then a stored override — with off beating on among them
//! — then `flags.on`, then the resolver, then off.
//!
//! # Rollouts
//!
//! [`Flags::rollout`] is a percentage, and it is *stable*: the bucket a scope
//! lands in comes from hashing the flag name with the scope, not from a die
//! roll, so it is the same on the next request and in the next process. See
//! [`rollout`] for why that matters more than it sounds like it should.

pub mod flags;
pub mod guard;
pub mod plugin;
pub mod rollout;
pub mod scope;
pub mod store;

pub use flags::{FlagAnswer, Flags, ScopedFlags};
pub use guard::{FlagsExt, WhenActive};
pub use plugin::FeatureFlags;
pub use rollout::{bucket, in_rollout};
pub use scope::Scope;
pub use store::{FlagStore, MemoryStore};

pub use rustlavel_core::{Error, Result};

/// Everything an application usually wants: `use rustlavel_flags::prelude::*;`
pub mod prelude {
    pub use crate::{FeatureFlags, FlagStore, Flags, FlagsExt, Scope, ScopedFlags, WhenActive};
}
