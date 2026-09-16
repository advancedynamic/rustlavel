//! rustlavel-gateway: one door in front of several services.
//!
//! Nothing else in this framework forwards a request. The client can call out,
//! the cache can rate-limit, `rustlavel-otel` can trace — and no code takes an
//! inbound request, sends it on and streams the answer back. That is this
//! crate.
//!
//! ```
//! use rustlavel_gateway::{Gateway, Upstream};
//!
//! let gateway = Gateway::new()
//!     .route("/api/*", Upstream::at("http://127.0.0.1:9002"))
//!     .route("/oauth/*", Upstream::at("http://127.0.0.1:9001"))
//!     // An upstream is anywhere, not only a service you deployed.
//!     .route("/maps/*", Upstream::at("https://api.example.com").strip("/maps"))
//!     // Refuse a request with no credential before it costs a hop. The
//!     // services behind still check for themselves — see below.
//!     .require_token();
//!
//! assert_eq!(gateway.routes().find("/api/orders").map(Upstream::base), Some("http://127.0.0.1:9002"));
//! ```
//!
//! Hand it to an application with `.plugin(gateway)`: it registers as the
//! router's fallback, so the application's own routes — health, metrics —
//! are matched first and only what nothing claimed reaches the table. A
//! gateway that swallowed every path would hide exactly the endpoints
//! somebody needs when the services behind it are the problem.
//!
//!
//! **A gateway is not the only door.** Anything on the network can reach a
//! service directly, so the services behind this still check tokens for
//! themselves. What the gateway saves is load, not the check: a request with no
//! token is refused here rather than after three network hops.

pub mod forward;
pub mod health;
pub mod gateway;
pub mod route;

pub use gateway::Gateway;
pub use route::{Locate, Routes, Upstream};
