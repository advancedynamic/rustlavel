use async_trait::async_trait;
use loco_rs::{
    app::{AppContext, Hooks, Initializer},
    bgworker::Queue,
    boot::{create_app, BootResult, StartMode},
    config::Config,
    controller::AppRoutes,
    environment::Environment,
    task::Tasks,
    Result,
};
use migration::Migrator;
use std::path::Path;

use crate::{controllers, initializers};

pub struct App;
#[async_trait]
impl Hooks for App {
    fn app_name() -> &'static str {
        env!("CARGO_CRATE_NAME")
    }

    fn app_version() -> String {
        format!(
            "{} ({})",
            env!("CARGO_PKG_VERSION"),
            option_env!("BUILD_SHA")
                .or(option_env!("GITHUB_SHA"))
                .unwrap_or("dev")
        )
    }

    async fn boot(
        mode: StartMode,
        environment: &Environment,
        config: Config,
    ) -> Result<BootResult> {
        create_app::<Self, Migrator>(mode, environment, config).await
    }

    async fn initializers(_ctx: &AppContext) -> Result<Vec<Box<dyn Initializer>>> {
        Ok(vec![Box::new(initializers::view_engine::ViewEngineInitializer)])
    }

    fn routes(_ctx: &AppContext) -> AppRoutes {
        // `empty()` rather than `with_default_routes()`: the contract's eight
        // endpoints and nothing else. `_ping`/`_health`/`_readiness` are not
        // measured and would be extra routes in the matcher.
        AppRoutes::empty()
            .add_route(controllers::bench::routes())
            .add_route(controllers::bench::middleware_routes())
    }

    // No background work is exercised by the contract, and the `worker`
    // feature is compiled out; nothing to register.
    async fn connect_workers(_ctx: &AppContext, _queue: &Queue) -> Result<()> {
        Ok(())
    }

    #[allow(unused_variables)]
    fn register_tasks(tasks: &mut Tasks) {
        // tasks-inject (do not remove)
    }

    // The fixture is owned by `benchmarks/schema.sql`, not by the app. Both of
    // these are destructive and must stay no-ops.
    async fn truncate(_ctx: &AppContext) -> Result<()> {
        Ok(())
    }

    async fn seed(_ctx: &AppContext, _base: &Path) -> Result<()> {
        Ok(())
    }
}
