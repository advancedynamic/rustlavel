//! The application builder.

use rustlavel_core::{Config, Context, ContextBuilder, Result};
use rustlavel_http::plugin::{Plugin, Setup};
use rustlavel_http::{Files, Handler, Middleware, Request, Response, Router, Server, TestClient};
use std::path::PathBuf;

/// The entry point of every rustlavel application.
///
/// ```ignore
/// App::new()?
///     .routes(routes::web::routes)
///     .plugin(Telescope::default())
///     .serve()
///     .await
/// ```
pub struct App {
    router: Router,
    config: Config,
    context: Option<ContextBuilder>,
    root: PathBuf,
    public: Option<PathBuf>,
}

impl App {
    /// Boot from the current directory: load `.env`, then `config/*.json`.
    pub fn new() -> Result<Self> {
        App::from_root(std::env::current_dir()?)
    }

    /// Boot from an explicit project root. Tests use this to point at a fixture.
    pub fn from_root(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let config = rustlavel_core::boot(&root)?;
        let public = root.join("public");

        Ok(App {
            router: Router::new(),
            context: Some(Context::builder().config(config.clone())),
            config,
            public: public.is_dir().then_some(public),
            root,
        })
    }

    /// Build an application with no filesystem behind it, for unit tests.
    pub fn bare() -> Self {
        let config = Config::with_defaults();
        App {
            router: Router::new(),
            context: Some(Context::builder().config(config.clone())),
            config,
            root: PathBuf::from("."),
            public: None,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Register routes — this is what loads `routes/web.rs`.
    pub fn routes(mut self, define: impl FnOnce(&mut Router)) -> Self {
        define(&mut self.router);
        self
    }

    /// Add middleware that runs for every request.
    pub fn middleware(mut self, middleware: impl Middleware) -> Self {
        self.router.middleware(middleware);
        self
    }

    /// Register a service that handlers resolve with `req.state::<T>()`.
    pub fn state<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.context = Some(self.context.take().expect("context builder").state(value));
        self
    }

    /// Enable an optional package.
    pub fn plugin(mut self, plugin: impl Plugin) -> Self {
        let name = plugin.name();
        let mut setup =
            Setup { router: &mut self.router, config: &self.config, context: &mut self.context };
        Box::new(plugin).register(&mut setup);
        rustlavel_core::debug!("plugin registered: {name}");
        self
    }

    /// Replace the default 404 handler.
    pub fn fallback(mut self, handler: impl Handler) -> Self {
        self.router.fallback(handler);
        self
    }

    /// Serve static files from a directory. `public/` is used automatically
    /// when it exists, so this is only needed for a different location.
    pub fn public(mut self, dir: impl Into<PathBuf>) -> Self {
        self.public = Some(dir.into());
        self
    }

    fn finish(mut self) -> (Router, Context) {
        // A health endpoint every deployment can rely on, unless the app
        // defined its own.
        if !self.router.routes().iter().any(|route| route.pattern == "/up") {
            self.router.get("/up", |_req: Request| async {
                Response::json(rustlavel_core::Json::object([("status", "ok".into())]))
            });
        }

        // Static files answer only what no route claimed.
        if let Some(dir) = self.public.clone()
            && self.router.routes().iter().all(|route| route.pattern != "/{path:*}") {
                self.router.fallback(Files::new(dir));
            }

        let context = self.context.take().expect("context builder").build();
        (self.router, context)
    }

    /// The application's entry point: serve, or run the command the CLI
    /// forwarded to this binary.
    ///
    /// `rustlavel route:list` cannot answer on its own — in a compiled language
    /// only the built application knows its routes — so the CLI re-invokes this
    /// binary with the command as an argument.
    pub async fn run(self) -> Result<()> {
        let args: Vec<String> = std::env::args().skip(1).collect();

        match args.first().map(String::as_str) {
            None | Some("serve") => self.serve().await,
            Some("route:list") => {
                // Finish first, so the listing includes the routes the
                // framework and any plugin add.
                let (router, _) = self.finish();
                print_route_table(&router);
                Ok(())
            }
            Some(other) => Err(rustlavel_core::Error::msg(format!(
                "unknown command `{other}`. Run `rustlavel help` to see what is available."
            ))),
        }
    }

    /// Bind and serve until Ctrl-C.
    pub async fn serve(self) -> Result<()> {
        let host = self.config.string("server.host", "127.0.0.1");
        let port = self.config.int("server.port", 8000);
        let name = self.config.string("app.name", "Rustlavel");
        let environment = self.config.environment();

        rustlavel_core::info!("{name} [{environment}]");

        let (router, context) = self.finish();
        Server::new(router, context).listen(format!("{host}:{port}")).await
    }

    /// A test client for this application, without binding a port.
    pub fn test_client(self) -> TestClient {
        let (router, context) = self.finish();
        TestClient::new(router).with_context(context)
    }

    /// The routes as `rustlavel route:list` prints them.
    pub fn route_table(&self) -> Vec<(String, String, String)> {
        self.router
            .routes()
            .iter()
            .map(|route| {
                (
                    route.method.to_string(),
                    route.pattern.clone(),
                    route.name.clone().unwrap_or_default(),
                )
            })
            .collect()
    }
}

/// Print the route table, the way `php artisan route:list` does.
fn print_route_table(router: &Router) {
    let width = router.routes().iter().map(|r| r.pattern.len()).max().unwrap_or(4).max(4);

    println!("\n  {:<8}{:<width$}  NAME", "METHOD", "URI");
    for route in router.routes() {
        println!(
            "  {:<8}{:<width$}  {}",
            route.method.as_str(),
            route.pattern,
            route.name.as_deref().unwrap_or("")
        );
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_core::Json;

    #[tokio::test]
    async fn serves_registered_routes() {
        let client = App::bare()
            .routes(|r| {
                r.get("/", |_req: Request| async { "home" });
            })
            .test_client();

        client.get("/").await.assert_ok().assert_see("home");
    }

    #[tokio::test]
    async fn adds_a_health_endpoint_by_default() {
        let client = App::bare().test_client();

        client.get("/up").await.assert_ok().assert_json("status", "ok");
    }

    #[tokio::test]
    async fn an_application_route_wins_over_the_default_health_endpoint() {
        let client = App::bare()
            .routes(|r| {
                r.get("/up", |_req: Request| async { "custom" });
            })
            .test_client();

        client.get("/up").await.assert_see("custom");
    }

    #[tokio::test]
    async fn state_is_resolvable_from_a_handler() {
        struct Greeting(&'static str);

        let client = App::bare()
            .state(Greeting("hello from state"))
            .routes(|r| {
                r.get("/", |req: Request| async move {
                    req.state::<Greeting>().unwrap().0.to_string()
                });
            })
            .test_client();

        client.get("/").await.assert_see("hello from state");
    }

    #[tokio::test]
    async fn a_plugin_can_add_routes_and_state() {
        struct Counter(usize);
        struct CounterPlugin;

        impl Plugin for CounterPlugin {
            fn name(&self) -> &'static str {
                "counter"
            }

            fn register(self: Box<Self>, setup: &mut Setup<'_>) {
                setup.state(Counter(7));
                setup.router.get("/counter", |req: Request| async move {
                    Json::object([("count", (req.state::<Counter>().unwrap().0 as i64).into())])
                });
            }
        }

        let client = App::bare().plugin(CounterPlugin).test_client();

        client.get("/counter").await.assert_ok().assert_json("count", 7);
    }

    #[test]
    fn route_table_lists_names() {
        let app = App::bare().routes(|r| {
            r.get("/users/{id}", |_req: Request| async { "u" }).name("users.show");
        });

        let table = app.route_table();
        assert!(table.contains(&("GET".into(), "/users/{id}".into(), "users.show".into())));
    }
}
