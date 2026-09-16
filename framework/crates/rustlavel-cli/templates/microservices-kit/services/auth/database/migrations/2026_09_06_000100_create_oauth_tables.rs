//! The authorization server's tables, and only its.
//!
//! **The definition lives in the crate**, not here. The first version of this
//! file wrote the schema out by hand and the columns did not match what the
//! stores read: no `family` on a code, access and refresh tokens in one table,
//! no consent table at all. It migrated cleanly and nothing could use it,
//! because a schema and the code that reads it were two descriptions of one
//! thing kept in step by memory.
//!
//! The resource server has a database of its own and never reads these. A
//! second service reading this schema would be a second service to redeploy
//! whenever it changes, which is the coupling a separate database exists to
//! prevent.

use rustlavel::db::migration;
use rustlavel::oauth_provider::database;

migration!(
    CreateOauthTables,
    "2026_09_06_000100_create_oauth_tables",
    up: |schema| { database::schema(schema).await },
    down: |schema| { database::drop_schema(schema).await },
);
