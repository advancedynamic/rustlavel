//! rustlavel-core: the foundation every rustlavel package builds on.
//!
//! Configuration and `.env` loading, the JSON value model, the typed
//! application context that replaces Laravel's service container, structured
//! logging, and the instrumentation bus that Telescope and tracing listen on.

pub mod config;
pub mod context;
pub mod env;
pub mod error;
pub mod events;
pub mod json;
pub mod log;

pub use config::Config;
pub use context::{Context, ContextBuilder};
pub use error::{Error, Result};
pub use events::Event;
pub use json::Json;

/// Load `.env` and build the default configuration tree.
///
/// Called once during boot, before anything reads configuration.
pub fn boot(root: impl AsRef<std::path::Path>) -> Result<Config> {
    let root = root.as_ref();
    env::load(root.join(".env"))?;

    let config = Config::with_defaults();
    config.load_dir(root.join("config"))?;

    if let Some(level) = std::env::var("LOG_LEVEL").ok().and_then(|v| log::Level::parse(&v)) {
        log::set_level(level);
    } else if config.is_production() {
        log::set_level(log::Level::Info);
    } else {
        log::set_level(log::Level::Debug);
    }

    // Production wants machine-readable logs; local development wants to read them.
    log::set_json(config.is_production());

    Ok(config)
}
