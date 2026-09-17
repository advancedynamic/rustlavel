//! The chain envelope and the progress table, against PostgreSQL.
//!
//! Two claims only a database can check: a chain survives the trip through
//! the `payload` column of a `jobs` table created without knowing about
//! chains, and progress written by one connection is read by another — which
//! is the worker process and the web process.

use std::sync::Arc;

use rustlavel_core::{Json, Result};
use rustlavel_db::{Database, Schema};
use rustlavel_queue::database::define_job_progress_table;
use rustlavel_queue::{
    Chain, DatabaseProgress, DatabaseQueue, Job, JobContext, JobRegistry, Progress, ProgressStore,
    Queue, QueueExt, Worker,
};

async fn database() -> Option<Database> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(Database::connect(&url).await.expect("the test database is reachable"))
}

macro_rules! db_or_skip {
    () => {
        match database().await {
            Some(db) => db,
            None => {
                eprintln!("skipped: set DATABASE_URL to run the PostgreSQL chain and progress tests");
                return;
            }
        }
    };
}

struct Mark(String);

impl Job for Mark {
    const NAME: &'static str = "mark";
    fn payload(&self) -> Json {
        Json::object([("name", Json::from(self.0.as_str()))])
    }
    fn from_payload(payload: &Json) -> Result<Self> {
        Ok(Mark(payload.get("name").and_then(Json::as_str).unwrap_or("?").to_string()))
    }
    async fn handle(&self) -> Result<()> {
        Ok(())
    }
    async fn handle_with(&self, ctx: &JobContext) -> Result<()> {
        ctx.progress(50, format!("running {}", self.0)).await;
        Ok(())
    }
}

/// The chain is stored in a `jobs` table that was created by `migrate()`
/// exactly as 0.7 created it — no new columns — and comes back whole.
#[tokio::test]
async fn a_chain_survives_the_payload_column() {
    let db = db_or_skip!();
    let name = format!("chain_{}", std::process::id());
    let queue = DatabaseQueue::with_tables(db.clone(), &format!("q_{name}"), &format!("q_{name}_failed"))
        .expect("a queue over two tables");
    queue.migrate().await.expect("tables");
    let queue = Arc::new(queue);

    let progress_table = format!("p_{name}");
    let schema = Schema::new(&db);
    let _ = schema.drop(&progress_table).await;
    schema.create(&progress_table, define_job_progress_table).await.expect("progress table");
    let progress: Arc<dyn ProgressStore> = Arc::new(DatabaseProgress::new(db.clone()).with_table(&progress_table));

    queue
        .dispatch_chain(Chain::new(Mark("a".into())).then(Mark("b".into())).then(Mark("c".into())).named("pg-chain"))
        .await
        .unwrap();

    let mut registry = JobRegistry::new();
    registry.register::<Mark>();
    let worker = Worker::new(Arc::clone(&queue) as Arc<dyn Queue>, Arc::new(registry))
        .reporting_to(Arc::clone(&progress));

    let mut ran = 0;
    while worker.run_once().await.unwrap().is_some() {
        ran += 1;
        assert!(ran <= 3, "the chain ran more steps than it has");
    }
    assert_eq!(ran, 3, "not every step came back out of the payload column");

    // Progress written by the worker's connection, read by a fresh one — the
    // web process's view.
    let other = Database::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
    let reader = DatabaseProgress::new(other).with_table(&progress_table);
    let whole = reader.get("chain:pg-chain").await.unwrap().expect("the chain's progress is in the table");
    assert!(whole.finished);
    assert_eq!(whole.percent, 100);

    let _ = schema.drop(&progress_table).await;
    let _ = schema.drop(&format!("q_{name}")).await;
    let _ = schema.drop(&format!("q_{name}_failed")).await;
}

/// Set twice from two connections, and the second write wins — the upsert
/// with no `ON CONFLICT` still converges.
#[tokio::test]
async fn progress_is_upserted_across_connections() {
    let db = db_or_skip!();
    let table = format!("p_upsert_{}", std::process::id());
    let schema = Schema::new(&db);
    let _ = schema.drop(&table).await;
    schema.create(&table, define_job_progress_table).await.expect("progress table");

    let a = DatabaseProgress::new(db.clone()).with_table(&table);
    let b = DatabaseProgress::new(
        Database::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap(),
    )
    .with_table(&table);

    let at = |percent, stage: &str| Progress {
        percent,
        stage: stage.into(),
        updated_at: 1,
        finished: false,
        failed: None,
    };
    a.set("job-1", at(10, "first")).await.unwrap();
    b.set("job-1", at(70, "second")).await.unwrap();

    let seen = a.get("job-1").await.unwrap().expect("recorded");
    assert_eq!(seen.percent, 70);
    assert_eq!(seen.stage, "second");
    assert_eq!(a.get("nothing").await.unwrap(), None);

    let _ = schema.drop(&table).await;
}
