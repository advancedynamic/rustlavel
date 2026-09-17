//! The webhook log, backed by a table.
//!
//! **This is the one to deploy.** A gateway retries a callback it was not
//! acknowledged for, and a retry that arrives after a restart is still a
//! duplicate — the in-memory log has forgotten it by then, and the customer is
//! credited twice.
//!
//! `record` is atomic by construction: one `INSERT` against a unique index on
//! the key. Two deliveries racing each other reach the database, exactly one
//! insert succeeds, and the other is told it lost. There is no read before
//! the write, because the window between them is the whole problem.

use crate::webhook::{LogFuture, WebhookEvent, WebhookLog};
use rustlavel_core::{Json, Result};
use rustlavel_db::{Database, Schema};

/// Create the table. Call it from a migration.
pub async fn schema(schema: &Schema<'_>) -> Result<()> {
    schema
        .create("payment_webhooks", |t| {
            t.id();
            // `kind:provider_id`. Unique: this index *is* the deduplication.
            t.string("dedup_key").unique();
            t.string("gateway");
            t.string("kind");
            t.string("event_id");
            // The body exactly as received, for replay.
            t.text("raw");
            t.big_integer("received_at");
        })
        .await
}

pub async fn drop_schema(schema: &Schema<'_>) -> Result<()> {
    schema.drop("payment_webhooks").await
}

#[derive(Clone)]
pub struct DatabaseWebhookLog {
    db: Database,
    gateway: &'static str,
}

impl DatabaseWebhookLog {
    pub fn new(db: Database, gateway: &'static str) -> DatabaseWebhookLog {
        DatabaseWebhookLog { db, gateway }
    }
}

impl WebhookLog for DatabaseWebhookLog {
    fn record<'a>(&'a self, event: &'a WebhookEvent) -> LogFuture<'a, bool> {
        Box::pin(async move {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()) as i64;

            let inserted = self
                .db
                .table("payment_webhooks")
                .insert_without_id(
                    &self.db,
                    &[
                        ("dedup_key", event.dedup_key().as_str().into()),
                        ("gateway", self.gateway.into()),
                        ("kind", event.kind.as_str().into()),
                        ("event_id", event.id.as_str().into()),
                        ("raw", event.raw.to_string().as_str().into()),
                        ("received_at", now.into()),
                    ],
                )
                .await;

            match inserted {
                Ok(_) => Ok(true),
                // A unique-index violation is the duplicate. Anything else is
                // a real failure and is passed up, so the receiver answers 500
                // and the gateway retries rather than the event being lost.
                Err(error) if is_unique_violation(&error) => Ok(false),
                Err(error) => Err(error),
            }
        })
    }

    fn raw<'a>(&'a self, dedup_key: &'a str) -> LogFuture<'a, Option<Json>> {
        Box::pin(async move {
            let row = self
                .db
                .table("payment_webhooks")
                .filter("dedup_key", dedup_key)
                .first(&self.db)
                .await?;
            Ok(row.and_then(|row| row.get::<String>("raw").ok()).and_then(|raw| Json::parse(&raw).ok()))
        })
    }

    fn withdraw<'a>(&'a self, dedup_key: &'a str) -> LogFuture<'a, ()> {
        Box::pin(async move {
            self.db.table("payment_webhooks").filter("dedup_key", dedup_key).delete(&self.db).await?;
            Ok(())
        })
    }
}

/// Whether a database error is "that key already exists".
///
/// Matched on the message, because the three databases report it three ways
/// and the driver does not yet classify errors. The strings are each
/// engine's own: PostgreSQL's SQLSTATE 23505 text, MySQL's 1062, SQL
/// Server's 2601/2627.
fn is_unique_violation(error: &rustlavel_core::Error) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("duplicate key")
        || text.contains("unique constraint")
        || text.contains("duplicate entry")
        || text.contains("cannot insert duplicate key")
        || text.contains("23505")
}
