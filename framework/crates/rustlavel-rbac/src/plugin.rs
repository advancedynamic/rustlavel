//! Attaching the package to an application, the way every optional package
//! attaches itself:
//!
//! ```ignore
//! App::new()
//!     .plugin(Rbac::from_config(db.clone(), &config)?)
//! ```
//!
//! One explicit line in `main.rs`, no auto-discovery. It puts a [`Permissions`]
//! into the application context, which is where [`Can`](crate::Can) and
//! `req.can(...)` look for it — and the only reason those two work without
//! being handed a store.
//!
//! The migrations are *not* registered from here, because migrations are not
//! runtime state: they belong in the registry the CLI generates, next to the
//! application's own. Add them with `registry.extend(rustlavel_rbac::migrations())`.

use crate::store::Permissions;
use rustlavel_core::{Config, Result};
use rustlavel_db::Database;
use rustlavel_http::plugin::{Plugin, Setup};
use std::time::Duration;

/// Registers the [`Permissions`] store into application state.
///
/// Adds no routes. There is no first-party admin UI, deliberately: a screen
/// that hands out permissions is an application's own decision — about who may
/// open it above all — and shipping one that is enabled by adding a plugin
/// would be shipping a privilege-escalation route with an on switch.
pub struct Rbac {
    store: Permissions,
}

impl Rbac {
    /// The defaults: the conventional tables, `super-admin`, a 30-second cache.
    pub fn new(db: Database) -> Self {
        Rbac { store: Permissions::new(db) }
    }

    /// Read `rbac.super_role` (or `rbac.super_roles`) and `rbac.cache_ttl_ms`.
    ///
    /// In `.env` that is `RBAC_SUPER_ROLE=owner` and `RBAC_CACHE_TTL_MS=5000`,
    /// through whatever mapping the application's configuration uses.
    pub fn from_config(db: Database, config: &Config) -> Result<Self> {
        Ok(Rbac { store: Permissions::from_config(db, config)? })
    }

    /// Use a store you built yourself — with different tables, say.
    pub fn with_store(store: Permissions) -> Self {
        Rbac { store }
    }

    pub fn super_role(mut self, name: impl Into<String>) -> Self {
        self.store = self.store.super_role(name);
        self
    }

    pub fn cache_ttl(mut self, ttl: Duration) -> Self {
        self.store = self.store.cache_ttl(ttl);
        self
    }

    /// The store, for wiring it into anything else that needs one.
    pub fn store(&self) -> &Permissions {
        &self.store
    }
}

impl Plugin for Rbac {
    fn name(&self) -> &'static str {
        "rbac"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        // Cloning shares the cache: every request gets a handle to the same
        // map, which is the whole point of caching at all.
        setup.state(self.store.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::Grants;
    use crate::guard::RbacExt;
    use crate::store::DEFAULT_SUPER_ROLE;
    use rustlavel_core::Context;
    use rustlavel_db::DatabaseConfig;
    use rustlavel_http::router::Router;
    use rustlavel_http::{Request, TestClient};

    fn offline() -> Database {
        Database::lazy(
            DatabaseConfig::from_url("postgres://nobody:nothing@127.0.0.1:1/none").unwrap(),
        )
        .unwrap()
    }

    /// Register the plugin the way an application would.
    fn registered(plugin: Rbac) -> TestClient {
        let mut router = Router::new();
        let config = Config::with_defaults();
        let mut context = Some(Context::builder());

        router.get("/who", |request: Request| async move {
            let store = request.permissions()?;
            let list = store.permissions_for(41).await?;
            Ok::<_, rustlavel_core::Error>(list.join(","))
        });

        let mut setup = Setup { router: &mut router, config: &config, context: &mut context };
        Box::new(plugin).register(&mut setup);

        TestClient::new(router).with_context(context.expect("the builder survives setup").build())
    }

    #[tokio::test]
    async fn the_plugin_puts_a_store_where_handlers_can_find_it() {
        let plugin = Rbac::new(offline());
        // Prime through the plugin's own store: the clone the plugin registers
        // shares the cache, so this is visible from inside the handler.
        plugin.store().prime(
            41,
            Grants {
                granted: ["posts.publish".to_string()].into_iter().collect(),
                ..Grants::default()
            },
        );

        registered(plugin).get("/who").await.assert_ok().assert_see("posts.publish");
    }

    #[tokio::test]
    async fn without_the_plugin_a_handler_gets_the_configuration_error() {
        let mut router = Router::new();
        router.get("/who", |request: Request| async move {
            let store = request.permissions()?;
            Ok::<_, rustlavel_core::Error>(store.permissions_for(41).await?.join(","))
        });

        TestClient::new(router).get("/who").await.assert_status(500);
    }

    #[test]
    fn the_plugin_is_named_for_route_list() {
        assert_eq!(Rbac::new(offline()).name(), "rbac");
    }

    #[test]
    fn configuration_reaches_the_store_through_the_plugin() {
        let config = Config::new();
        config.set("rbac.super_role", "owner");
        config.set("rbac.cache_ttl_ms", 1_500);

        let plugin = Rbac::from_config(offline(), &config).unwrap();

        assert!(plugin.store().super_role_names().contains("owner"));
        assert!(!plugin.store().super_role_names().contains(DEFAULT_SUPER_ROLE));
    }

    #[test]
    fn the_builders_override_the_defaults() {
        let plugin = Rbac::new(offline()).super_role("root").cache_ttl(Duration::from_secs(1));

        assert_eq!(plugin.store().super_role_names().iter().collect::<Vec<_>>(), ["root"]);
    }
}
