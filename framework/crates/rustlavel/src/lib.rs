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
mod console;
#[cfg(feature = "view")]
mod view;

pub use app::App;
pub use console::Console;

#[cfg(feature = "ai")]
pub use rustlavel_ai as ai;
#[cfg(feature = "auth")]
pub use rustlavel_auth as auth;
#[cfg(feature = "cache")]
pub use rustlavel_cache as cache;
#[cfg(feature = "client")]
pub use rustlavel_client as client;
#[cfg(feature = "db")]
pub use rustlavel_db as db;
#[cfg(feature = "debugbar")]
pub use rustlavel_debugbar as debugbar;
#[cfg(feature = "i18n")]
pub use rustlavel_i18n as i18n;
#[cfg(feature = "vault")]
pub use rustlavel_vault as vault;
#[cfg(feature = "ldap")]
pub use rustlavel_ldap as ldap;
#[cfg(feature = "oauth")]
pub use rustlavel_oauth as oauth;
#[cfg(feature = "oauth-provider")]
pub use rustlavel_oauth_provider as oauth_provider;
#[cfg(feature = "mail")]
pub use rustlavel_mail as mail;
#[cfg(feature = "mail")]
pub use rustlavel_mail::Mail;
#[cfg(feature = "mcp")]
pub use rustlavel_mcp as mcp;
#[cfg(feature = "metrics")]
pub use rustlavel_metrics as metrics;
#[cfg(feature = "metrics")]
pub use rustlavel_metrics::Metrics;
#[cfg(feature = "otel")]
pub use rustlavel_otel as otel;
#[cfg(feature = "search")]
pub use rustlavel_search as search;
#[cfg(feature = "webauthn")]
pub use rustlavel_webauthn as webauthn;
#[cfg(feature = "openapi")]
pub use rustlavel_openapi as openapi;
#[cfg(feature = "queue")]
pub use rustlavel_queue as queue;
#[cfg(feature = "storage")]
pub use rustlavel_storage as storage;
#[cfg(feature = "telescope")]
pub use rustlavel_telescope as telescope;
#[cfg(feature = "telescope")]
pub use rustlavel_telescope::Telescope;
#[cfg(feature = "validation")]
pub use rustlavel_validation as validation;
#[cfg(feature = "view")]
pub use rustlavel_view as views;
#[cfg(feature = "ws")]
pub use rustlavel_ws as ws;
#[cfg(feature = "view")]
pub use view::{Views, engine_from_config};

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

    #[cfg(feature = "ai")]
    pub use rustlavel_ai::Ai;
    #[cfg(feature = "auth")]
    pub use rustlavel_auth::prelude::*;
    #[cfg(feature = "cache")]
    pub use rustlavel_cache::prelude::*;
    #[cfg(feature = "client")]
    pub use rustlavel_client::Client;
    #[cfg(feature = "db")]
    pub use rustlavel_db::prelude::*;
    #[cfg(feature = "i18n")]
    pub use rustlavel_i18n::Translator;
    #[cfg(feature = "queue")]
    pub use rustlavel_queue::prelude::*;
    #[cfg(feature = "storage")]
    pub use rustlavel_storage::{Storage, Visibility};
    #[cfg(feature = "validation")]
    pub use rustlavel_validation::{Errors, Rule, Validated, validate};
    #[cfg(feature = "view")]
    pub use crate::Views;
    #[cfg(feature = "view")]
    pub use rustlavel_view::{Context as ViewContext, Engine};
    #[cfg(feature = "ws")]
    pub use rustlavel_ws::{Broadcaster, Message as WsMessage, WebSocket, websocket};
}

/// Helpers for writing application tests.
pub mod test_prelude {
    pub use crate::prelude::*;
    pub use rustlavel_http::{TestClient, TestResponse};
}
