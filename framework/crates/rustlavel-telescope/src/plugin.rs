//! Turning Telescope on, and refusing to turn it on where it does not belong.
//!
//! ```ignore
//! App::new()?
//!     .routes(routes::web::routes)
//!     .plugin(Telescope::default())
//!     .serve()
//!     .await
//! ```
//!
//! One line, no auto-discovery, no service provider: the application says it
//! wants Telescope and the plugin does the rest — subscribe to the bus, mount
//! the dashboard, and register the store so the application's own handlers can
//! read it back.

use crate::dashboard::PageOptions;
use crate::journal::Journal;
use crate::recorder::Recorder;
use crate::routes;
use crate::store::{DEFAULT_CAPACITY, Store};
use rustlavel_core::{Config, events};
use rustlavel_http::plugin::{Plugin, Setup};
use rustlavel_http::Router;
use std::path::PathBuf;

/// Where the dashboard lives when nothing says otherwise.
pub const DEFAULT_MOUNT: &str = "/telescope";
/// Entries at or above this many milliseconds are highlighted.
pub const DEFAULT_SLOW_MS: f64 = 100.0;

/// The Telescope plugin.
///
/// Every setting can come from configuration instead, so a deployment can tune
/// it without recompiling. An explicit builder call always wins over
/// configuration, because a value written in `main.rs` is a decision and a
/// value in a config file is a default:
///
/// | Config key           | Meaning                                             |
/// |----------------------|-----------------------------------------------------|
/// | `telescope.enabled`  | Mount at all. Defaults to false in production.      |
/// | `telescope.route`    | Where the dashboard is mounted (`/telescope`).      |
/// | `telescope.limit`    | Ring buffer size (500 entries).                     |
/// | `telescope.slow_ms`  | Duration at which an entry is marked slow (100).    |
/// | `telescope.path`     | JSON-lines file to persist entries to. Off by default. |
#[derive(Default)]
pub struct Telescope {
    mount: Option<String>,
    capacity: Option<usize>,
    slow_ms: Option<f64>,
    file: Option<PathBuf>,
    force: bool,
    ignored: Vec<String>,
}

impl Telescope {
    pub fn new() -> Self {
        Telescope::default()
    }

    /// Mount the dashboard somewhere else: `.at("/_debug")`.
    pub fn at(mut self, mount: impl Into<String>) -> Self {
        self.mount = Some(mount.into());
        self
    }

    /// How many entries to keep. Memory is the only cost, and an entry is small.
    pub fn capacity(mut self, entries: usize) -> Self {
        self.capacity = Some(entries);
        self
    }

    /// The threshold above which an entry is highlighted as slow.
    pub fn slow_after_ms(mut self, millis: f64) -> Self {
        self.slow_ms = Some(millis);
        self
    }

    /// Also append entries to a JSON-lines file, and load them back on boot, so
    /// a restart does not lose the request you were about to look at.
    pub fn persist_to(mut self, path: impl Into<PathBuf>) -> Self {
        self.file = Some(path.into());
        self
    }

    /// Do not record this kind of event.
    pub fn ignore(mut self, kind: impl Into<String>) -> Self {
        self.ignored.push(kind.into());
        self
    }

    /// Mount even in production.
    ///
    /// Spelled out in full because of what it means: the dashboard shows SQL,
    /// log messages, and every field any package chose to emit. Anyone who can
    /// reach the URL can read them. Put it behind authentication middleware
    /// before you call this.
    pub fn even_in_production(mut self) -> Self {
        self.force = true;
        self
    }

    /// Whether Telescope may mount in this environment.
    ///
    /// Outside production it is on unless configuration says otherwise; in
    /// production it is off unless someone explicitly said yes. The asymmetry
    /// is the point — the failure mode of a forgotten debug tool is a data
    /// leak, so the default has to be the safe one.
    fn allowed(&self, config: &Config) -> bool {
        self.force || config.bool("telescope.enabled", !config.is_production())
    }

    /// Mount onto a router directly.
    ///
    /// Returns the [`Store`] when Telescope mounted, and `None` when the
    /// production guard refused. Applications go through [`Plugin`]; this is
    /// for tests and for an application that builds its own router.
    pub fn install(self, router: &mut Router, config: &Config) -> Option<Store> {
        if !self.allowed(config) {
            rustlavel_core::warn!(
                "telescope is not mounted: it exposes queries and log messages, and this is a \
                 production environment. Set `telescope.enabled` to true (and put the route \
                 behind authentication) if that is what you want."
            );
            return None;
        }

        let mount = normalise_mount(
            self.mount.clone().unwrap_or_else(|| config.string("telescope.route", DEFAULT_MOUNT)),
        );
        let capacity = self
            .capacity
            .unwrap_or_else(|| config.int("telescope.limit", DEFAULT_CAPACITY as i64).max(1) as usize);
        let slow_ms =
            self.slow_ms.unwrap_or_else(|| config.int("telescope.slow_ms", DEFAULT_SLOW_MS as i64) as f64);
        let file = self.file.clone().or_else(|| {
            let configured = config.string("telescope.path", "");
            (!configured.is_empty()).then(|| PathBuf::from(configured))
        });

        let store = Store::with_capacity(capacity);

        // Recover the previous run before anything new is recorded, so restored
        // ids stay below the ones this process will hand out.
        let journal = file.map(|path| {
            store.restore(Journal::load(&path, capacity));
            Journal::open(path)
        });

        routes::register(
            router,
            store.clone(),
            PageOptions {
                mount: mount.clone(),
                app: config.string("app.name", "Rustlavel"),
                slow_ms,
                capacity,
            },
        );

        // Announced before subscribing, so Telescope's own boot message is not
        // the first thing in its buffer.
        rustlavel_core::info!("telescope is recording — dashboard at {mount}");

        events::subscribe(
            Recorder::new(store.clone())
                .with_journal(journal)
                .ignoring(self.ignored.clone())
                .skipping_path(mount),
        );

        Some(store)
    }
}

impl Plugin for Telescope {
    fn name(&self) -> &'static str {
        "telescope"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        if let Some(store) = (*self).install(setup.router, setup.config) {
            // Registered as state so an application handler can read its own
            // recorded history — a health page, or a test asserting on queries.
            setup.state(store);
        }
    }
}

/// A mount point is a path prefix: leading slash, no trailing one.
fn normalise_mount(mount: String) -> String {
    let trimmed = mount.trim().trim_matches('/');
    if trimmed.is_empty() { DEFAULT_MOUNT.to_string() } else { format!("/{trimmed}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_core::Event;
    use rustlavel_http::TestClient;

    fn config_for(environment: &str) -> Config {
        let config = Config::new();
        config.set("app.env", environment);
        config.set("app.name", "Test App");
        config
    }

    fn mounted(telescope: Telescope, config: &Config) -> (Option<Store>, Router) {
        let mut router = Router::new();
        let store = telescope.install(&mut router, config);
        (store, router)
    }

    fn patterns(router: &Router) -> Vec<String> {
        router.routes().iter().map(|route| route.pattern.clone()).collect()
    }

    #[test]
    fn the_plugin_registers_the_dashboard_and_its_api() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let (store, router) = mounted(Telescope::new(), &config_for("local"));

        assert!(store.is_some());
        let patterns = patterns(&router);
        assert!(patterns.contains(&"/telescope".to_string()));
        assert!(patterns.contains(&"/telescope/api/entries".to_string()));
        assert!(patterns.contains(&"/telescope/api/entries/{id}".to_string()));
        // The listing pattern carries both a GET and a DELETE route.
        assert_eq!(patterns.iter().filter(|p| *p == "/telescope/api/entries").count(), 2);

        events::clear_subscribers();
    }

    #[test]
    fn production_refuses_to_mount() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let (store, router) = mounted(Telescope::new(), &config_for("production"));

        assert!(store.is_none());
        assert!(patterns(&router).is_empty());
        // And nothing is listening on the bus, so nothing is recorded either.
        assert!(!events::has_subscribers());

        events::clear_subscribers();
    }

    #[test]
    fn production_mounts_when_configuration_explicitly_enables_it() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let config = config_for("production");
        config.set("telescope.enabled", true);
        let (store, router) = mounted(Telescope::new(), &config);

        assert!(store.is_some());
        assert!(patterns(&router).contains(&"/telescope".to_string()));

        events::clear_subscribers();
    }

    #[test]
    fn production_mounts_when_the_application_forces_it_in_code() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let (store, _) = mounted(Telescope::new().even_in_production(), &config_for("production"));
        assert!(store.is_some());

        events::clear_subscribers();
    }

    #[test]
    fn configuration_can_switch_it_off_outside_production_too() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let config = config_for("local");
        config.set("telescope.enabled", false);
        let (store, _) = mounted(Telescope::new(), &config);

        assert!(store.is_none());

        events::clear_subscribers();
    }

    #[test]
    fn a_custom_mount_point_moves_every_route() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let (_, router) = mounted(Telescope::new().at("_debug/"), &config_for("local"));

        let patterns = patterns(&router);
        assert!(patterns.contains(&"/_debug".to_string()));
        assert!(patterns.contains(&"/_debug/api/entries".to_string()));

        events::clear_subscribers();
    }

    #[test]
    fn configuration_supplies_the_mount_point_capacity_and_threshold() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let config = config_for("local");
        config.set("telescope.route", "/insight");
        config.set("telescope.limit", 7);
        config.set("telescope.slow_ms", 25);
        let (store, router) = mounted(Telescope::new(), &config);

        assert_eq!(store.expect("mounted").capacity(), 7);
        assert!(patterns(&router).contains(&"/insight".to_string()));

        events::clear_subscribers();
    }

    #[test]
    fn a_builder_call_wins_over_configuration() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let config = config_for("local");
        config.set("telescope.limit", 7);
        let (store, _) = mounted(Telescope::new().capacity(11), &config);

        assert_eq!(store.expect("mounted").capacity(), 11);

        events::clear_subscribers();
    }

    #[tokio::test]
    async fn a_mounted_telescope_records_and_serves_what_it_recorded() {
        // The bus lock is released before the HTTP assertions: the router's
        // handlers hold the store directly, so they no longer need the
        // subscriber registry, and holding a blocking lock across an `await`
        // would be wrong even in a test.
        let router = {
            let _guard = crate::test_support::exclusive();
            events::clear_subscribers();

            let mut router = Router::new();
            Telescope::new().install(&mut router, &config_for("local")).expect("mounted");
            Event::new("http.request")
                .with("method", "GET")
                .with("path", "/orders")
                .with("status", 200)
                .dispatch();

            events::clear_subscribers();
            router
        };

        let client = TestClient::new(router);
        client.get("/telescope").await.assert_ok().assert_see("GET /orders");
        client
            .get("/telescope/api/entries")
            .await
            .assert_ok()
            .assert_json("entries.0.summary", "GET /orders");
    }

    #[test]
    fn the_dashboard_does_not_record_its_own_requests() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let mut router = Router::new();
        let store = Telescope::new().install(&mut router, &config_for("local")).expect("mounted");

        // What the server dispatches after serving the dashboard itself.
        Event::new("http.request").with("path", "/telescope/api/entries").dispatch();

        assert!(store.is_empty());

        events::clear_subscribers();
    }

    #[test]
    fn a_persisted_run_is_loaded_back_on_boot() {
        let _guard = crate::test_support::exclusive();
        events::clear_subscribers();

        let path = std::env::temp_dir()
            .join("rustlavel-telescope-tests")
            .join("plugin-persistence.jsonl");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("temp dir");
        let _ = std::fs::remove_file(&path);

        let mut first = Router::new();
        let store =
            Telescope::new().persist_to(&path).install(&mut first, &config_for("local")).expect("mounted");
        Event::new("log").with("message", "from the previous run").dispatch();
        Event::new("http.request")
            .with("method", "GET")
            .with("path", "/orders")
            .took(std::time::Duration::from_secs(5))
            .dispatch();
        assert_eq!(store.len(), 2);
        events::clear_subscribers();

        // Wait for the writer thread: three lines, because the log entry is
        // written once on its own and again once the request has claimed it.
        for _ in 0..200 {
            if std::fs::read_to_string(&path).is_ok_and(|c| c.lines().count() >= 3) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let mut second = Router::new();
        let restored =
            Telescope::new().persist_to(&path).install(&mut second, &config_for("local")).expect("mounted");

        assert_eq!(restored.len(), 2);
        let entries = restored.entries(&Default::default());
        assert_eq!(entries[0].summary(), "GET /orders");
        assert_eq!(entries[1].summary(), "from the previous run");
        // And the log line is still attached to the request that caused it.
        assert_eq!(restored.related(entries[0].id).len(), 1);

        events::clear_subscribers();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_mount_point_is_normalised_into_a_path_prefix() {
        assert_eq!(normalise_mount("telescope".into()), "/telescope");
        assert_eq!(normalise_mount("/debug/".into()), "/debug");
        assert_eq!(normalise_mount("  ".into()), DEFAULT_MOUNT);
    }
}
