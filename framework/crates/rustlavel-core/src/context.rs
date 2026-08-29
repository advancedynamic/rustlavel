//! The application context: configuration plus shared, typed state.
//!
//! This is what replaces Laravel's service container. Laravel resolves services
//! by name at runtime; here a handler asks for a type and the compiler proves it
//! was registered — `req.state::<Database>()` cannot typo its way into a
//! runtime failure.

use crate::config::Config;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

/// Shared state handed to every request, cheap to clone.
#[derive(Clone)]
pub struct Context {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    state: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl Context {
    pub fn builder() -> ContextBuilder {
        ContextBuilder { config: Config::with_defaults(), state: HashMap::new() }
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Fetch a registered service by type.
    pub fn state<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.inner.state.get(&TypeId::of::<T>()).and_then(|value| value.downcast_ref::<T>())
    }

    /// Fetch a registered service, panicking with an actionable message if the
    /// application forgot to register it. Intended for framework packages that
    /// cannot proceed without their own state.
    pub fn expect_state<T: Send + Sync + 'static>(&self) -> &T {
        self.state::<T>().unwrap_or_else(|| {
            panic!(
                "`{}` was never registered on the application. \
                 Add `.state(...)` for it in main.rs, or enable the package that provides it.",
                std::any::type_name::<T>()
            )
        })
    }
}

impl Default for Context {
    fn default() -> Self {
        Context::builder().build()
    }
}

pub struct ContextBuilder {
    config: Config,
    state: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl ContextBuilder {
    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// Register a service. One value per type; registering twice replaces.
    pub fn state<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.state.insert(TypeId::of::<T>(), Box::new(value));
        self
    }

    pub fn build(self) -> Context {
        Context { inner: Arc::new(Inner { config: self.config, state: self.state }) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Database(&'static str);
    struct Cache;

    #[test]
    fn resolves_registered_state_by_type() {
        let context = Context::builder().state(Database("postgres")).build();

        assert_eq!(context.state::<Database>().unwrap().0, "postgres");
        assert!(context.state::<Cache>().is_none());
    }

    #[test]
    fn carries_configuration() {
        let config = Config::new();
        config.set("app.name", "Rustlavel");
        let context = Context::builder().config(config).build();

        assert_eq!(context.config().string("app.name", ""), "Rustlavel");
    }
}
