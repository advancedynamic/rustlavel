//! The one table, and its migration.

use rustlavel_db::migration::Migration;
use rustlavel_db::schema::{Schema, Table};
use rustlavel_core::Result;

/// Where entries live. Not configurable: an audit trail that can be pointed at
/// a different table is an audit trail with two places to look.
pub const TABLE: &str = "audit_logs";

fn define(t: &mut Table) {
    t.id();

    // Nullable, and deliberately not a foreign key. An entry outlives the
    // account it describes — "who deleted this user" is a question people ask
    // *after* the user is gone — and a cascade would delete the evidence along
    // with the subject.
    t.big_integer("user_id").nullable().index();
    // The actor's name as it was at the time. A renamed account must not
    // silently rewrite what an old entry says happened.
    t.string("user_name").nullable();

    // `users.deleted`, `settings.updated`, `logged_in`. Dotted by convention,
    // so a filter can match a whole area with a prefix.
    t.string("event").index();

    // What was acted on, if anything: a type name and its key. Two columns
    // rather than one so a filter can ask for every entry about `User`.
    t.string("model_type").nullable().index();
    t.string("model_id").nullable();

    // One sentence, already written. Composing it at read time would mean the
    // page has to know about every event any application ever records.
    t.text("description").nullable();

    // Anything else worth keeping, as JSON. The before-and-after of a change
    // usually goes here.
    t.text("properties").nullable();

    t.string("ip_address").nullable().index();
    t.text("user_agent").nullable();

    t.timestamps();
}

rustlavel_db::migration!(
    CreateAuditLogsTable,
    "2026_09_03_000001_create_audit_logs_table",
    up: |schema| { schema.create(TABLE, define).await },
    down: |schema| { schema.drop(TABLE).await },
);

/// The migrations this package needs, for the application's registry.
pub fn migrations() -> Vec<&'static dyn Migration> {
    vec![&CreateAuditLogsTable]
}

/// Drop the table. For a test that made one.
pub async fn drop_table(schema: &Schema<'_>) -> Result<()> {
    schema.drop(TABLE).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_db::dialect::{MySql, Postgres, SqlServer};
    use rustlavel_db::schema::create_statements;

    /// The same definition has to be valid on all three databases, and the
    /// only way to know without three databases is to render the DDL.
    #[test]
    fn the_table_renders_on_every_supported_database() {
        for dialect in [
            &Postgres as &dyn rustlavel_db::Dialect,
            &MySql as &dyn rustlavel_db::Dialect,
            &SqlServer as &dyn rustlavel_db::Dialect,
        ] {
            let sql = create_statements(dialect, TABLE, define).expect("valid").join(";\n");

            assert!(sql.contains("audit_logs"), "{sql}");
            assert!(sql.contains("event"), "{sql}");
            assert!(sql.contains("ip_address"), "{sql}");
            // The actor is nullable: an entry can outlive its account, and a
            // NOT NULL here would make deleting a user delete its own record.
            assert!(!sql.to_lowercase().contains("user_id bigint not null"), "{sql}");
        }
    }
}
