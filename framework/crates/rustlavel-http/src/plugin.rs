//! How an optional package attaches itself to an application.
//!
//! Laravel discovers packages at runtime and boots them by reflection. Here a
//! package is enabled by one explicit line in `main.rs`:
//!
//! ```ignore
//! App::new().plugin(Telescope::default())
//! ```
//!
//! The trait lives in the HTTP crate rather than the meta-crate so a package
//! can implement it without depending on `rustlavel` itself — which would
//! otherwise be a dependency cycle.

use crate::router::Router;
use rustlavel_core::{Config, ContextBuilder};

/// What a plugin is handed when it registers.
pub struct Setup<'a> {
    pub router: &'a mut Router,
    pub config: &'a Config,
    pub context: &'a mut Option<ContextBuilder>,
}

impl Setup<'_> {
    /// Register a service the plugin's own handlers will resolve later.
    pub fn state<T: Send + Sync + 'static>(&mut self, value: T) {
        let builder = self.context.take().expect("context builder is available during setup");
        *self.context = Some(builder.state(value));
    }
}

pub trait Plugin: Send + 'static {
    /// Shown by `rustlavel route:list` and in boot logs.
    fn name(&self) -> &'static str;

    /// Add routes, middleware, and state.
    fn register(self: Box<Self>, setup: &mut Setup<'_>);
}
