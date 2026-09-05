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
    /// Where the generated API document is served, when it is enabled.
    #[cfg(feature = "openapi")]
    openapi: Option<(rustlavel_openapi::Info, String)>,
    /// The migration list the CLI generated, so `rustlavel migrate` can run it.
    #[cfg(feature = "db")]
    migrations: Vec<&'static dyn rustlavel_db::Migration>,
    #[cfg(feature = "db")]
    seeders: Vec<&'static dyn rustlavel_db::Seeder>,
    #[cfg(feature = "queue")]
    queue: Option<std::sync::Arc<dyn rustlavel_queue::Queue>>,
    #[cfg(feature = "queue")]
    jobs: Option<std::sync::Arc<rustlavel_queue::JobRegistry>>,
    #[cfg(feature = "queue")]
    scheduler: Option<rustlavel_queue::Scheduler>,
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
            #[cfg(feature = "openapi")]
            openapi: None,
            #[cfg(feature = "db")]
            migrations: Vec::new(),
            #[cfg(feature = "db")]
            seeders: Vec::new(),
            #[cfg(feature = "queue")]
            queue: None,
            #[cfg(feature = "queue")]
            jobs: None,
            #[cfg(feature = "queue")]
            scheduler: None,
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
            #[cfg(feature = "openapi")]
            openapi: None,
            #[cfg(feature = "db")]
            migrations: Vec::new(),
            #[cfg(feature = "db")]
            seeders: Vec::new(),
            #[cfg(feature = "queue")]
            queue: None,
            #[cfg(feature = "queue")]
            jobs: None,
            #[cfg(feature = "queue")]
            scheduler: None,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    #[cfg(feature = "db")]
    pub(crate) fn registered_migrations(&self) -> Vec<&'static dyn rustlavel_db::Migration> {
        self.migrations.clone()
    }

    #[cfg(feature = "db")]
    pub(crate) fn registered_seeders(&self) -> Vec<&'static dyn rustlavel_db::Seeder> {
        self.seeders.clone()
    }

    #[cfg(feature = "queue")]
    pub(crate) fn registered_queue(&self) -> Option<std::sync::Arc<dyn rustlavel_queue::Queue>> {
        self.queue.clone()
    }

    #[cfg(feature = "queue")]
    pub(crate) fn registered_jobs(&self) -> Option<std::sync::Arc<rustlavel_queue::JobRegistry>> {
        self.jobs.clone()
    }

    /// Takes the scheduler, because running it consumes it — and nothing else
    /// wants it afterwards.
    #[cfg(feature = "queue")]
    pub(crate) fn take_scheduler(&mut self) -> Option<rustlavel_queue::Scheduler> {
        self.scheduler.take()
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
    pub fn plugin(self, plugin: impl Plugin) -> Self {
        self.plugin_boxed(Box::new(plugin))
    }

    /// The same, for a plugin chosen at runtime.
    ///
    /// `plugin` takes a concrete type, which is right when `main.rs` names each
    /// one. An application that keeps its features in a list — a module per
    /// feature, registered by walking the list — has `Box<dyn Plugin>` instead,
    /// and had no way in.
    pub fn plugin_boxed(mut self, plugin: Box<dyn Plugin>) -> Self {
        let name = plugin.name();
        let mut setup =
            Setup { router: &mut self.router, config: &self.config, context: &mut self.context };
        plugin.register(&mut setup);
        rustlavel_core::debug!("plugin registered: {name}");
        self
    }

    /// Register the template engine, so handlers can call `req.view(...)`.
    #[cfg(feature = "view")]
    pub fn views(self, engine: rustlavel_view::Engine) -> Self {
        self.state(engine)
    }

    /// Serve a generated OpenAPI document, and a page that reads it.
    ///
    /// The document is built after every route is registered, which is why
    /// this records the intent rather than mounting immediately: a document
    /// generated now would describe an empty API.
    #[cfg(feature = "openapi")]
    pub fn openapi(mut self, info: rustlavel_openapi::Info) -> Self {
        self.openapi = Some((info, "/openapi.json".to_string()));
        self
    }

    /// Register the migrations `rustlavel migrate` should run.
    ///
    /// A compiled program cannot list a directory into types, so the CLI keeps
    /// this list generated beside the migration files: `database::migrations::all()`.
    #[cfg(feature = "db")]
    pub fn migrations(mut self, migrations: Vec<&'static dyn rustlavel_db::Migration>) -> Self {
        self.migrations = migrations;
        self
    }

    #[cfg(feature = "db")]
    pub fn seeders(mut self, seeders: Vec<&'static dyn rustlavel_db::Seeder>) -> Self {
        self.seeders = seeders;
        self
    }

    /// The queue `rustlavel queue:work` should drain.
    #[cfg(feature = "queue")]
    pub fn queue(mut self, queue: impl rustlavel_queue::Queue + 'static) -> Self {
        self.queue = Some(std::sync::Arc::new(queue));
        self
    }

    /// The registry that turns a job name back into something runnable.
    #[cfg(feature = "queue")]
    pub fn jobs(mut self, registry: rustlavel_queue::JobRegistry) -> Self {
        self.jobs = Some(std::sync::Arc::new(registry));
        self
    }

    #[cfg(feature = "queue")]
    pub fn schedule(mut self, scheduler: rustlavel_queue::Scheduler) -> Self {
        self.scheduler = Some(scheduler);
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

        // Generated last but one, so the document sees the application's
        // routes and the health endpoint, but not the static-file fallback.
        #[cfg(feature = "openapi")]
        if let Some((info, path)) = self.openapi.take() {
            rustlavel_openapi::mount(&mut self.router, &info, &path);
        }

        // Static files answer only what no route claimed.
        if let Some(dir) = self.public.clone()
            && self.router.routes().iter().all(|route| route.pattern != "/{path:*}") {
                self.router.fallback(Files::new(dir));
            }

        // A `resources/views` directory is enough to turn views on, the way a
        // `public` directory turns on static files — **unless the application
        // built its own engine**. It used to register one either way, which
        // meant `.views(...)` silently did nothing for any application that had
        // a views directory, which is every application that has views. An
        // engine carrying a translator was replaced by one that could not
        // translate, and `@lang` rendered its keys.
        #[cfg(feature = "view")]
        if self.root.join(self.config.string("view.root", rustlavel_view::DEFAULT_ROOT)).is_dir()
            && !self
                .context
                .as_ref()
                .expect("context builder")
                .has_state::<rustlavel_view::Engine>()
        {
            let engine = crate::view::engine_from_config(&self.config, &self.root);
            self.context = Some(self.context.take().expect("context builder").state(engine));
        }

        // The named routes, for `@route` in a template and for a handler that
        // wants to send somebody to a route by name rather than by path.
        //
        // Registered here because this is the first moment both exist: the
        // router is complete, and the context has not been sealed. The engine
        // is handed the same table through a cell rather than a field, because
        // an application may have built its own engine with `.views(...)` long
        // before a single route was declared.
        let named = self.router.named_routes();
        self.context =
            Some(self.context.take().expect("context builder").state(named.clone()));

        let context = self.context.take().expect("context builder").build();

        #[cfg(feature = "view")]
        if let Some(engine) = context.state::<rustlavel_view::Engine>() {
            let _ = engine
                .routes_cell()
                .set(std::sync::Arc::new(crate::view::RouteTable::new(named)));
        }

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
                print_route_table(&router, &args[1..]);
                Ok(())
            }
            // Everything else the CLI forwards is answered by the console,
            // which is where the commands needing the application itself live.
            Some(other) => {
                let rest: Vec<String> = args.iter().skip(1).cloned().collect();
                crate::console::Console::dispatch(self, other, &rest).await
            }
        }
    }

    /// Bind and serve until Ctrl-C.
    pub async fn serve(self) -> Result<()> {
        let host = self.config.string("server.host", "127.0.0.1");
        let port = self.config.int("server.port", 8000);
        let name = self.config.string("app.name", "Rustlavel");
        let environment = self.config.environment();

        rustlavel_core::info!("{name} [{environment}]");

        // Query bindings are what make a slow-query log useful, and also where
        // a password ends up. They stay out of the event stream in production.
        #[cfg(feature = "db")]
        rustlavel_db::set_log_bindings(!self.config.is_production());

        let (router, context) = self.finish();
        Server::new(router, context).listen(format!("{host}:{port}")).await
    }

    /// The finished router and context, for serving on a listener of the
    /// caller's own. Tests that need a real socket use this.
    pub fn take_parts(self) -> (Router, Context) {
        self.finish()
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
/// `route:list`, and the filters that make it readable past fifty routes.
///
/// An application of any size lists more than fits a screen, and the answer to
/// "where is the route for X" should not be piping this through `grep` — which
/// works, and loses the header row that says what the columns are.
///
/// ```text
/// route:list --path admin      only paths containing `admin`
/// route:list --method post     only POSTs (any case)
/// route:list --name auth       only named routes matching `auth`
/// ```
fn matching_routes<'a>(
    router: &'a Router,
    filters: &[String],
) -> Vec<&'a rustlavel_http::router::Route> {
    let value_of = |flag: &str| -> Option<String> {
        filters
            .iter()
            .position(|argument| argument == flag)
            .and_then(|at| filters.get(at + 1))
            .map(|value| value.to_lowercase())
    };

    let path = value_of("--path");
    let method = value_of("--method");
    let name = value_of("--name");

    router
        .routes()
        .iter()
        .filter(|route| {
            path.as_ref().is_none_or(|want| route.pattern.to_lowercase().contains(want))
                && method
                    .as_ref()
                    .is_none_or(|want| route.method.as_str().eq_ignore_ascii_case(want))
                && name.as_ref().is_none_or(|want| {
                    route.name.as_deref().is_some_and(|had| had.to_lowercase().contains(want))
                })
        })
        .collect()
}

fn print_route_table(router: &Router, filters: &[String]) {
    let value_of = |flag: &str| -> Option<String> {
        filters
            .iter()
            .position(|argument| argument == flag)
            .and_then(|at| filters.get(at + 1))
            .map(|value| value.to_lowercase())
    };

    let path = value_of("--path");
    let method = value_of("--method");
    let name = value_of("--name");

    let matching = matching_routes(router, filters);
    let _ = (&path, &method, &name);

    let width = matching.iter().map(|r| r.pattern.len()).max().unwrap_or(4).max(4);

    println!("\n  {:<8}{:<width$}  NAME", "METHOD", "URI");
    for route in &matching {
        println!(
            "  {:<8}{:<width$}  {}",
            route.method.as_str(),
            route.pattern,
            route.name.as_deref().unwrap_or("")
        );
    }

    // The count, and — when a filter hid some — what it hid, so a search that
    // finds nothing says so rather than printing an empty table.
    let total = router.routes().len();
    match matching.len() {
        0 => println!("\n  no route matches, out of {total}"),
        shown if shown == total => println!("\n  {shown} routes"),
        shown => println!("\n  {shown} of {total} routes"),
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

    /// A hundred routes do not fit a screen, and `grep` loses the header.
    #[test]
    fn route_list_filters_by_path_method_and_name() {
        use rustlavel_http::router::Router;

        let mut router = Router::new();
        router.get("/admin/users", |_req: Request| async { "" }).name("admin.users");
        router.post("/admin/users", |_req: Request| async { "" });
        router.get("/login", |_req: Request| async { "" }).name("login");

        let matching = |filters: &[&str]| {
            let filters: Vec<String> = filters.iter().map(|f| f.to_string()).collect();
            super::matching_routes(&router, &filters)
                .iter()
                .map(|r| format!("{} {}", r.method.as_str(), r.pattern))
                .collect::<Vec<_>>()
        };

        assert_eq!(matching(&[]).len(), 3, "no filter lists everything");
        assert_eq!(matching(&["--path", "admin"]).len(), 2);
        assert_eq!(matching(&["--method", "POST"]), ["POST /admin/users"]);
        // Case does not matter: nobody remembers whether they typed `post`.
        assert_eq!(matching(&["--method", "post"]), ["POST /admin/users"]);
        assert_eq!(matching(&["--name", "login"]), ["GET /login"]);
        // Filters narrow together rather than replacing one another.
        assert_eq!(matching(&["--path", "admin", "--method", "get"]), ["GET /admin/users"]);
        assert!(matching(&["--path", "nowhere"]).is_empty());
    }
}
