//! The database driver: a `jobs` table, a `failed_jobs` table, and a
//! reservation that is safe when several workers race for the same row.
//!
//! Timestamps are stored as epoch seconds in `bigint` columns rather than as
//! `timestamptz`. rustlavel-db deliberately leaves a timestamp column as text so
//! precision survives, and a queue compares times far more often than it shows
//! them: an integer sorts, compares and indexes without anything having to
//! agree on a text format.

use crate::job::{BoxFuture, FailedJob, QueuedJob, ReservedJob};
use crate::queue::{Queue, record_pushed};
use crate::time::unix_now;
use rustlavel_core::{Error, Json, Result};
use rustlavel_db::schema::{Schema, Table};
use rustlavel_db::{Database, Value, quote_identifier};
use std::time::Duration;

/// The conventional table names, matching Laravel's.
pub const JOBS_TABLE: &str = "jobs";
pub const FAILED_JOBS_TABLE: &str = "failed_jobs";

/// Create the queue's two tables.
///
/// Shared by the migration below and by [`DatabaseQueue::migrate`], so a test
/// that wants its own table names gets exactly the schema production has.
pub async fn create_tables(schema: &Schema<'_>, jobs: &str, failed: &str) -> Result<()> {
    schema.create(jobs, define_jobs_table).await?;
    schema.create(failed, define_failed_jobs_table).await?;
    Ok(())
}

/// The `jobs` table. A free function rather than a closure so a test can build
/// the statements without a database, and see the same schema production gets.
pub fn define_jobs_table(t: &mut Table) {
    t.id();
    t.string("queue");
    t.string("name");
    t.json("payload");
    // Counted up on every reservation, never reset: it is the number that
    // decides when a job has had enough.
    t.integer("attempts").default_int(0);
    t.integer("max_tries").default_int(crate::job::DEFAULT_TRIES as i64);
    t.integer("retry_after").default_int(60);
    t.big_integer("available_at");
    // Null means nobody holds it. Set when a worker reserves it, which is also
    // what lets a crashed worker's job be reclaimed.
    t.big_integer("reserved_at").nullable();
    t.big_integer("created_at");
    // The exact shape of the reservation query, so finding the next job is an
    // index scan rather than a table scan taken under a row lock.
    t.index(&["queue", "reserved_at", "available_at"]);
}

/// The dead-letter table.
pub fn define_failed_jobs_table(t: &mut Table) {
    t.id();
    t.string("queue");
    t.string("name");
    t.json("payload");
    t.integer("attempts");
    t.text("error");
    t.big_integer("failed_at");
}

rustlavel_db::migration!(
    CreateQueueTables,
    "2026_08_29_000001_create_queue_tables",
    up: |schema| { crate::database::create_tables(schema, JOBS_TABLE, FAILED_JOBS_TABLE).await },
    down: |schema| {
        schema.drop(FAILED_JOBS_TABLE).await?;
        schema.drop(JOBS_TABLE).await
    },
);

/// A queue backed by two database tables.
///
/// Survives a restart, and is shared by every process pointed at the same
/// database — which is the whole reason to prefer it over the memory driver.
#[derive(Clone)]
pub struct DatabaseQueue {
    db: Database,
    jobs: String,
    failed: String,
    /// Both names pre-quoted, so no statement builds an identifier at run time.
    jobs_sql: String,
    failed_sql: String,
}

impl DatabaseQueue {
    /// Use the conventional `jobs` and `failed_jobs` tables.
    pub fn new(db: Database) -> Self {
        DatabaseQueue::with_tables(db, JOBS_TABLE, FAILED_JOBS_TABLE)
            .expect("the built-in table names are valid identifiers")
    }

    /// Use table names of your own. Validated here, once, rather than on every
    /// statement.
    pub fn with_tables(db: Database, jobs: &str, failed: &str) -> Result<Self> {
        Ok(DatabaseQueue {
            jobs_sql: quote_identifier(jobs)?,
            failed_sql: quote_identifier(failed)?,
            jobs: jobs.to_string(),
            failed: failed.to_string(),
            db,
        })
    }

    pub fn database(&self) -> &Database {
        &self.db
    }

    /// Create the tables directly, for an application that would rather not
    /// register [`CreateQueueTables`] in its migration registry.
    pub async fn migrate(&self) -> Result<()> {
        let schema = Schema::new(&self.db);
        create_tables(&schema, &self.jobs, &self.failed).await
    }

    /// Drop the tables. Intended for tests and `queue:fresh`.
    pub async fn drop_tables(&self) -> Result<()> {
        let schema = Schema::new(&self.db);
        schema.drop(&self.failed).await?;
        schema.drop(&self.jobs).await
    }

    /// Take the next job, holding it against every other worker.
    ///
    /// `select ... for update skip locked` is the primitive this needs, and
    /// nothing weaker will do:
    ///
    /// * A plain `select ... limit 1` followed by an `update` is a read-then-
    ///   write race. Two workers read the same row, both update it, and the job
    ///   runs twice — the one failure mode a queue may not have.
    /// * `for update` alone fixes correctness by making the second worker block
    ///   on the row lock, but it also serialises the pool: with ten workers, nine
    ///   are queued behind the same row instead of taking the nine rows after it.
    /// * `for update skip locked` locks the row this transaction picked and
    ///   makes every other transaction *step over* it rather than wait. Ten
    ///   workers take ten different rows in parallel, and no row is ever handed
    ///   out twice, because the lock is held until this transaction commits the
    ///   `reserved_at` that closes the row to everyone else.
    ///
    /// The select and the update are in one transaction for exactly that
    /// reason: the lock only lasts as long as the transaction does, so the row
    /// must be marked reserved before it is released.
    async fn reserve(&self, queue: &str) -> Result<Option<ReservedJob>> {
        let now = unix_now();
        let mut tx = self.db.begin().await?;

        let row = tx
            .select_one(
                &format!(
                    "select id, name, payload, attempts, max_tries, retry_after \
                     from {} \
                     where queue = $1 and reserved_at is null and available_at <= $2 \
                     order by id \
                     for update skip locked \
                     limit 1",
                    self.jobs_sql
                ),
                &[Value::from(queue), Value::from(now)],
            )
            .await?;

        let Some(row) = row else {
            // Nothing to do, but the transaction still has to end — an open one
            // would be discarded by the pool rather than reused.
            tx.rollback().await?;
            return Ok(None);
        };

        let id = row.get::<i64>("id")?;
        let attempts = row.get::<i64>("attempts")? as u32 + 1;
        let retry_after = Duration::from_secs(row.get::<i64>("retry_after")?.max(0) as u64);

        tx.execute(
            &format!(
                "update {} set attempts = $1, reserved_at = $2 where id = $3",
                self.jobs_sql
            ),
            &[Value::from(attempts), Value::from(now), Value::from(id)],
        )
        .await?;

        tx.commit().await?;

        Ok(Some(ReservedJob {
            id: id.to_string(),
            job: QueuedJob {
                name: row.get::<String>("name")?,
                payload: row.get::<Json>("payload")?,
                queue: queue.to_string(),
                max_tries: row.get::<i64>("max_tries")?.max(1) as u32,
                retry_after,
                delay: Duration::ZERO,
            },
            attempts,
        }))
    }

    /// Release jobs whose worker never came back.
    ///
    /// A worker that is killed between reserving and finishing leaves
    /// `reserved_at` set forever. Once the job's own `retry_after` has passed
    /// with no result, the job is assumed orphaned and goes back on the queue.
    ///
    /// Called only when the queue looks empty, so the common path — a worker
    /// picking up one of many waiting jobs — pays nothing for it.
    pub async fn reclaim_expired(&self, queue: &str) -> Result<u64> {
        let now = unix_now();

        self.db
            .execute(
                &format!(
                    "update {} set reserved_at = null, available_at = $1 \
                     where queue = $2 and reserved_at is not null and reserved_at + retry_after < $3",
                    self.jobs_sql
                ),
                &[Value::from(now), Value::from(queue), Value::from(now)],
            )
            .await
    }

    fn row_to_failed(row: &rustlavel_db::Row) -> Result<FailedJob> {
        Ok(FailedJob {
            id: row.get::<i64>("id")?.to_string(),
            name: row.get::<String>("name")?,
            queue: row.get::<String>("queue")?,
            payload: row.get::<Json>("payload")?,
            attempts: row.get::<i64>("attempts")?.max(0) as u32,
            error: row.get::<String>("error")?,
            failed_at: row.get::<i64>("failed_at")?,
        })
    }

    fn job_id(job: &ReservedJob) -> Result<i64> {
        job.id.parse::<i64>().map_err(|_| {
            Error::msg(format!(
                "`{}` is not an id from the database queue. Was this job reserved by a \
                 different driver?",
                job.id
            ))
        })
    }
}

impl Queue for DatabaseQueue {
    fn driver(&self) -> &'static str {
        "database"
    }

    fn push(&self, job: QueuedJob) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            let now = unix_now();
            let id = self
                .db
                .table(&self.jobs)
                .insert(
                    &self.db,
                    &[
                        ("queue", Value::from(job.queue.as_str())),
                        ("name", Value::from(job.name.as_str())),
                        ("payload", Value::from(job.payload.clone())),
                        ("attempts", Value::from(0)),
                        ("max_tries", Value::from(job.max_tries)),
                        ("retry_after", Value::from(job.retry_after.as_secs() as i64)),
                        ("available_at", Value::from(now + job.delay.as_secs() as i64)),
                        ("created_at", Value::from(now)),
                    ],
                )
                .await?
                .to_string();

            record_pushed(self.driver(), &job, &id);
            Ok(id)
        })
    }

    fn pop<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<Option<ReservedJob>>> {
        Box::pin(async move {
            if let Some(job) = self.reserve(queue).await? {
                return Ok(Some(job));
            }

            // Nothing was ready. Before reporting an empty queue, check whether
            // a dead worker is sitting on something, and only re-select if it
            // actually released anything.
            if self.reclaim_expired(queue).await? > 0 {
                return self.reserve(queue).await;
            }

            Ok(None)
        })
    }

    fn size<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let count = self.db.table(&self.jobs).filter("queue", queue).count(&self.db).await?;
            Ok(count.max(0) as u64)
        })
    }

    fn delete<'a>(&'a self, job: &'a ReservedJob) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.db
                .table(&self.jobs)
                .filter("id", DatabaseQueue::job_id(job)?)
                .delete(&self.db)
                .await?;
            Ok(())
        })
    }

    fn release<'a>(&'a self, job: &'a ReservedJob, delay: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.db
                .table(&self.jobs)
                .filter("id", DatabaseQueue::job_id(job)?)
                .update(
                    &self.db,
                    &[
                        ("reserved_at", Value::Null),
                        ("available_at", Value::from(unix_now() + delay.as_secs() as i64)),
                    ],
                )
                .await?;
            Ok(())
        })
    }

    fn fail<'a>(&'a self, job: &'a ReservedJob, error: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let id = DatabaseQueue::job_id(job)?;

            // One transaction, so a job can never be in both tables or neither.
            let mut tx = self.db.begin().await?;

            tx.execute(
                &format!(
                    "insert into {} (queue, name, payload, attempts, error, failed_at) \
                     values ($1, $2, $3, $4, $5, $6)",
                    self.failed_sql
                ),
                &[
                    Value::from(job.job.queue.as_str()),
                    Value::from(job.job.name.as_str()),
                    Value::from(job.job.payload.clone()),
                    Value::from(job.attempts),
                    Value::from(error),
                    Value::from(unix_now()),
                ],
            )
            .await?;

            tx.execute(&format!("delete from {} where id = $1", self.jobs_sql), &[Value::from(id)])
                .await?;

            tx.commit().await
        })
    }

    fn failed_jobs(&self) -> BoxFuture<'_, Result<Vec<FailedJob>>> {
        Box::pin(async move {
            let rows = self
                .db
                .select(&format!("select * from {} order by id", self.failed_sql), &[])
                .await?;
            rows.iter().map(DatabaseQueue::row_to_failed).collect()
        })
    }

    fn clear<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            self.db.table(&self.jobs).filter("queue", queue).delete(&self.db).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_db::Migration;

    #[test]
    fn table_names_are_validated_once_at_construction() {
        let db = Database::lazy(rustlavel_db::DatabaseConfig::default())
            .expect("the default configuration names a driver that exists");

        assert!(DatabaseQueue::with_tables(db.clone(), "app_jobs", "app_failed").is_ok());

        let error = match DatabaseQueue::with_tables(db, "jobs; drop table users", "failed") {
            Err(error) => error.to_string(),
            Ok(_) => panic!("an injected table name should have been rejected"),
        };
        assert!(error.contains("not a valid SQL identifier"), "{error}");
    }

    #[test]
    fn the_jobs_table_has_the_columns_the_driver_reads() {
        // The tables assert PostgreSQL's spelling; the dialect layer covers the
        // other databases.
        let jobs = rustlavel_db::schema::create_statements(
            &rustlavel_db::dialect::Postgres,
            JOBS_TABLE,
            define_jobs_table,
        )
        .unwrap();

        for column in [
            r#""id" bigserial primary key"#,
            r#""queue" varchar(255) not null"#,
            r#""name" varchar(255) not null"#,
            r#""payload" jsonb not null"#,
            r#""attempts" integer not null default 0"#,
            r#""max_tries" integer not null default 3"#,
            r#""retry_after" integer not null default 60"#,
            r#""available_at" bigint not null"#,
            r#""created_at" bigint not null"#,
        ] {
            assert!(jobs[0].contains(column), "expected {column} in:\n{}", jobs[0]);
        }

        // Nullable on purpose: null is what "nobody holds this job" means, and
        // it is what the reservation query filters on.
        assert!(jobs[0].contains(r#""reserved_at" bigint,"#), "{}", jobs[0]);
        assert!(!jobs[0].contains(r#""reserved_at" bigint not null"#), "{}", jobs[0]);

        assert!(
            jobs[1].contains(r#"on "jobs" ("queue", "reserved_at", "available_at")"#),
            "the reservation query needs a covering index:\n{}",
            jobs[1]
        );
    }

    #[test]
    fn the_failed_jobs_table_keeps_everything_needed_to_diagnose_a_job() {
        let failed =
            rustlavel_db::schema::create_statements(
                &rustlavel_db::dialect::Postgres,
                FAILED_JOBS_TABLE,
                define_failed_jobs_table,
            )
                .unwrap();

        for column in [
            r#""queue" varchar(255) not null"#,
            r#""name" varchar(255) not null"#,
            r#""payload" jsonb not null"#,
            r#""attempts" integer not null"#,
            r#""error" text not null"#,
            r#""failed_at" bigint not null"#,
        ] {
            assert!(failed[0].contains(column), "expected {column} in:\n{}", failed[0]);
        }
    }

    #[test]
    fn the_migration_is_registered_under_a_sortable_name() {
        assert_eq!(CreateQueueTables.name(), "2026_08_29_000001_create_queue_tables");
    }

    #[test]
    fn an_id_from_another_driver_is_rejected_rather_than_silently_ignored() {
        let job = ReservedJob {
            id: "memory-7".to_string(),
            job: QueuedJob::new("x", Json::Null),
            attempts: 1,
        };

        let error = DatabaseQueue::job_id(&job).unwrap_err().to_string();
        assert!(error.contains("not an id from the database queue"), "{error}");
    }
}
