//! The commands the CLI forwards to the application binary.
//!
//! `rustlavel migrate` cannot run migrations by itself: in a compiled language
//! the migrations are types inside the application, and only the application
//! can name them. So the CLI re-invokes the built binary with the command as an
//! argument, and this is what answers.

use crate::App;
use rustlavel_core::{Error, Result};

/// Everything the application knows how to do besides serve.
///
/// Kept in one place so `App::run` stays a dispatch table rather than a
/// hundred-line match.
pub struct Console;

impl Console {
    /// Handle a forwarded command, or say it is not one.
    /// `args` is only read by commands that take options, so it is unused when
    /// those packages are not enabled.
    pub async fn dispatch(app: App, command: &str, _args: &[String]) -> Result<()> {
        // `schedule:run` consumes the application; the rest only borrow it.
        match command {
            #[cfg(feature = "db")]
            "migrate" => migrate(&app).await,
            #[cfg(feature = "db")]
            "migrate:rollback" => rollback(&app).await,
            #[cfg(feature = "db")]
            "migrate:fresh" => fresh(&app).await,
            #[cfg(feature = "db")]
            "migrate:status" => status(&app).await,
            #[cfg(feature = "db")]
            "db:seed" => seed(&app).await,
            #[cfg(feature = "queue")]
            "queue:work" => work(&app, _args).await,
            #[cfg(feature = "queue")]
            "queue:failed" => failed(&app).await,
            #[cfg(feature = "queue")]
            "schedule:run" => schedule(app).await,
            other => Err(Error::msg(format!(
                "`{other}` is not a command this application answers. \
                 Run `rustlavel help` to see what is available."
            ))),
        }
    }

    /// Whether a command needs the application rather than the CLI.
    pub fn handles(command: &str) -> bool {
        matches!(
            command,
            "migrate"
                | "migrate:rollback"
                | "migrate:fresh"
                | "migrate:status"
                | "db:seed"
                | "queue:work"
                | "queue:failed"
                | "schedule:run"
        )
    }
}

#[cfg(feature = "db")]
async fn connect(app: &App) -> Result<rustlavel_db::Database> {
    use rustlavel_db::{Database, DatabaseConfig};

    let settings = DatabaseConfig::from_app_config(app.config())?;
    Database::with_config(settings).await
}

/// The registered migrations, or an error saying how to register them.
///
/// Checked before connecting: forgetting to register them is a programming
/// mistake, and hearing about it should not require a reachable database.
#[cfg(feature = "db")]
fn registered(app: &App) -> Result<Vec<&'static dyn rustlavel_db::Migration>> {
    let migrations = app.registered_migrations();
    if migrations.is_empty() {
        return Err(Error::msg(
            "no migrations are registered. Add `.migrations(database::migrations::all())` to the \
             App in main.rs — the CLI generates that list for you."
                .to_string(),
        ));
    }
    Ok(migrations)
}

#[cfg(feature = "db")]
async fn migrate(app: &App) -> Result<()> {
    let migrations = registered(app)?;
    let db = connect(app).await?;
    let report = rustlavel_db::Migrator::new(&db, migrations).run().await?;

    if report.applied.is_empty() {
        println!("\n  Nothing to migrate ({} already applied).\n", report.skipped);
    } else {
        println!("\n  Applied {} migration(s):", report.applied.len());
        for name in &report.applied {
            println!("    {name}");
        }
        println!();
    }
    Ok(())
}

#[cfg(feature = "db")]
async fn rollback(app: &App) -> Result<()> {
    let migrations = registered(app)?;
    let db = connect(app).await?;
    let report = rustlavel_db::Migrator::new(&db, migrations).rollback().await?;

    if report.rolled_back.is_empty() {
        println!("\n  Nothing to roll back.\n");
    } else {
        println!("\n  Rolled back {} migration(s):", report.rolled_back.len());
        for name in &report.rolled_back {
            println!("    {name}");
        }
        println!();
    }
    Ok(())
}

#[cfg(feature = "db")]
async fn fresh(app: &App) -> Result<()> {
    let migrations = registered(app)?;
    let environment = app.config().environment();
    let db = connect(app).await?;
    let report = rustlavel_db::Migrator::new(&db, migrations).fresh(&environment).await?;

    println!("\n  Dropped everything and applied {} migration(s).\n", report.applied.len());
    Ok(())
}

#[cfg(feature = "db")]
async fn status(app: &App) -> Result<()> {
    let migrations = registered(app)?;
    let db = connect(app).await?;
    let rows = rustlavel_db::Migrator::new(&db, migrations).status().await?;

    let width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(4);
    println!("\n  {:<width$}  STATUS", "MIGRATION");
    for (name, applied) in rows {
        println!("  {name:<width$}  {}", if applied { "applied" } else { "pending" });
    }
    println!();
    Ok(())
}

#[cfg(feature = "db")]
async fn seed(app: &App) -> Result<()> {
    let seeders = app.registered_seeders();

    if seeders.is_empty() {
        return Err(Error::msg(
            "no seeders are registered. Add `.seeders(database::seeders::all())` to the App in \
             main.rs."
                .to_string(),
        ));
    }

    let db = connect(app).await?;
    let ran = rustlavel_db::migration::seed(&db, &seeders).await?;
    println!("\n  Ran {} seeder(s).\n", ran.len());
    Ok(())
}

#[cfg(feature = "queue")]
async fn work(app: &App, args: &[String]) -> Result<()> {
    use rustlavel_queue::{Shutdown, Worker, run_pool};

    let queue = app.registered_queue().ok_or_else(|| {
        Error::msg(
            "no queue is registered. Add `.queue(...)` to the App in main.rs.".to_string(),
        )
    })?;
    let registry = app.registered_jobs().ok_or_else(|| {
        Error::msg(
            "no jobs are registered. Add `.jobs(...)` with a JobRegistry to the App in main.rs — \
             a compiled program cannot turn a job name back into a type without one."
                .to_string(),
        )
    })?;

    let workers = args
        .iter()
        .position(|arg| arg == "--workers")
        .and_then(|at| args.get(at + 1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(1usize);

    let shutdown = Shutdown::new();
    shutdown.on_ctrl_c();

    rustlavel_core::info!("Processing jobs with {workers} worker(s). Ctrl-C to stop.");
    let worker = Worker::new(queue, registry);
    run_pool(worker, workers, shutdown).await?;

    rustlavel_core::info!("Workers stopped.");
    Ok(())
}

#[cfg(feature = "queue")]
async fn failed(app: &App) -> Result<()> {
    let queue = app
        .registered_queue()
        .ok_or_else(|| Error::msg("no queue is registered.".to_string()))?;

    let failures = queue.failed_jobs().await?;
    if failures.is_empty() {
        println!("\n  No failed jobs.\n");
        return Ok(());
    }

    println!("\n  {} failed job(s):", failures.len());
    for job in &failures {
        println!("    {} — {}", job.name, job.error);
    }
    println!();
    Ok(())
}

#[cfg(feature = "queue")]
async fn schedule(mut app: App) -> Result<()> {
    use rustlavel_queue::Shutdown;

    let scheduler = app.take_scheduler().ok_or_else(|| {
        Error::msg("no schedule is registered. Add `.schedule(...)` to the App in main.rs.".to_string())
    })?;

    let shutdown = Shutdown::new();
    shutdown.on_ctrl_c();

    rustlavel_core::info!("Scheduler running. Ctrl-C to stop.");
    scheduler.run(shutdown).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_recognises_the_commands_the_cli_forwards() {
        assert!(Console::handles("migrate"));
        assert!(Console::handles("queue:work"));
        assert!(!Console::handles("serve"));
        assert!(!Console::handles("make:controller"));
    }

    #[tokio::test]
    async fn an_unknown_command_points_at_the_help() {
        let error = Console::dispatch(App::bare(), "nonsense", &[]).await.unwrap_err();
        assert!(error.to_string().contains("rustlavel help"));
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn migrating_without_a_registry_says_what_to_add_without_touching_a_database() {
        let error = Console::dispatch(App::bare(), "migrate", &[])
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains(".migrations(database::migrations::all())"), "{error}");
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn seeding_without_seeders_says_what_to_add() {
        let error = Console::dispatch(App::bare(), "db:seed", &[])
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains(".seeders(database::seeders::all())"), "{error}");
    }
}
