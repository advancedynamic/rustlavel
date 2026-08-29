//! Rustlavel — a Laravel-inspired web framework for Rust.
//!
//! ```ignore
//! use rustlavel::prelude::*;
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     App::new()?.routes(routes::web).serve().await
//! }
//! ```
//!
//! Optional packages are enabled through feature flags; what is not enabled is
//! never compiled.

mod app;

pub use app::App;

pub use rustlavel_core::{Config, Context, Error, Event, Json, Result, config, env, events, json, log};
pub use rustlavel_http::{
    Cookie, Files, Handler, Headers, Method, Middleware, Next, Plugin, Request, Resource, Response,
    Router, SameSite, Setup, Status, TestClient, TestResponse, cookie, error_page, middleware,
    request, response, router, server, status, testing, url,
};

/// Everything an application file usually needs.
pub mod prelude {
    pub use crate::App;
    pub use rustlavel_core::{Config, Context, Error, Json, Result};
    pub use rustlavel_http::{
        Cookie, IntoResponse, Method, Middleware, Next, Request, Response, Router, Status,
    };
    pub use rustlavel_core::{debug, error, info, warn};
}

/// Helpers for writing application tests.
pub mod test_prelude {
    pub use crate::prelude::*;
    pub use rustlavel_http::{TestClient, TestResponse};
}
