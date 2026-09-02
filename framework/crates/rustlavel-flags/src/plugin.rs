//! Attaching the package to an application, the way every optional package
//! attaches itself:
//!
//! ```ignore
//! App::new()
//!     .plugin(FeatureFlags::from_config(flags, &config))
//! ```
//!
//! One explicit line in `main.rs`, no auto-discovery. It puts a [`Flags`] into
//! the application context, which is where [`WhenActive`](crate::WhenActive)
//! and `req.flag(...)` look for it — and the only reason those two work without
//! being handed a registry.

use crate::flags::Flags;
use rustlavel_core::Config;
use rustlavel_http::plugin::{Plugin, Setup};

/// Registers the [`Flags`] registry into application state.
///
/// Adds no routes. There is no first-party screen for switching flags on and
/// off, deliberately: a page that changes what every user of the application
/// sees is one an application has to decide who may open, and shipping one that
/// turns itself on with the plugin would be shipping that decision already
/// made — wrongly.
pub struct FeatureFlags {
    flags: Flags,
}

impl FeatureFlags {
    /// Register a registry the application built.
    pub fn new(flags: Flags) -> Self {
        FeatureFlags { flags }
    }

    /// Register it, applying `flags.on` and `flags.off` from configuration
    /// first.
    ///
    /// In `.env` that is `FLAGS_OFF=new-checkout,beta-search`, through whatever
    /// mapping the application's configuration uses — which is the point of it:
    /// a feature can be switched off by an environment variable and a restart,
    /// with no deploy and no working database. See [`Flags`] for where the two
    /// lists sit in the precedence chain, and why `flags.off` sits at the top.
    pub fn from_config(flags: Flags, config: &Config) -> Self {
        FeatureFlags { flags: flags.configured(config) }
    }

    /// The registry, for wiring it into anything else that needs one — a
    /// console command, a seeder, a test.
    pub fn flags(&self) -> &Flags {
        &self.flags
    }
}

impl Plugin for FeatureFlags {
    fn name(&self) -> &'static str {
        "flags"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        // Cloning shares the store and the definitions: every request gets a
        // handle to the same registry, so an override written from an admin
        // route is seen by the next request rather than by the next process.
        setup.state(self.flags.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::FlagsExt;
    use crate::scope::Scope;
    use rustlavel_core::{Context, Error};
    use rustlavel_http::router::Router;
    use rustlavel_http::{Request, TestClient};

    fn flags() -> Flags {
        Flags::new().define("new-checkout", |scope: Scope| async move { scope.id().ends_with('7') })
    }

    /// Register the plugin the way an application would.
    fn registered(plugin: FeatureFlags) -> TestClient {
        let mut router = Router::new();
        let config = Config::with_defaults();
        let mut context = Some(Context::builder());

        router.get("/checkout", |request: Request| async move {
            let on = request.flag("new-checkout").await?;
            Ok::<_, Error>(format!("new-checkout={on}"))
        });

        let mut setup = Setup { router: &mut router, config: &config, context: &mut context };
        Box::new(plugin).register(&mut setup);

        TestClient::new(router).with_context(context.expect("the builder survives setup").build())
    }

    #[tokio::test]
    async fn the_plugin_puts_a_registry_where_handlers_can_find_it() {
        registered(FeatureFlags::new(flags()))
            .get("/checkout")
            .await
            .assert_ok()
            .assert_see("new-checkout=false");
    }

    #[tokio::test]
    async fn an_override_written_through_the_plugins_registry_is_seen_by_a_handler() {
        // The clone the plugin registers shares the store, which is what makes
        // an admin route able to change what the next request sees.
        let plugin = FeatureFlags::new(flags());
        plugin.flags().activate("new-checkout").await.unwrap();

        registered(plugin).get("/checkout").await.assert_ok().assert_see("new-checkout=true");
    }

    #[tokio::test]
    async fn without_the_plugin_a_handler_gets_the_configuration_error() {
        let mut router = Router::new();
        router.get("/checkout", |request: Request| async move {
            Ok::<_, Error>(format!("{}", request.flag("new-checkout").await?))
        });

        TestClient::new(router).get("/checkout").await.assert_status(500);
    }

    #[tokio::test]
    async fn configuration_reaches_the_registry_through_the_plugin() {
        let config = Config::new();
        config.set("flags.off", "new-checkout");

        let plugin = FeatureFlags::from_config(flags(), &config);
        // Forced off, and not even an override placed afterwards gets past it.
        plugin.flags().activate("new-checkout").await.unwrap();

        registered(plugin).get("/checkout").await.assert_ok().assert_see("new-checkout=false");
    }

    #[test]
    fn the_plugin_is_named_for_route_list() {
        assert_eq!(FeatureFlags::new(flags()).name(), "flags");
    }
}
