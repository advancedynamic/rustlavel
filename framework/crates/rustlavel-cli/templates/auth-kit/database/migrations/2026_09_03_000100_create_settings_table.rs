//! Application settings an administrator can change without a deploy.
//!
//! Key and value, not a column per setting. A column per setting means a
//! migration every time somebody wants a new toggle, and this table exists
//! precisely so that changing the application does not take a deploy.
//!
//! The cost is that everything is text and the meaning lives in the code that
//! reads it. `src/support/settings.rs` is that code, and it holds the
//! catalogue: the key, its type, its default, and which of them are secret.

use rustlavel::db::migration;

migration!(
    CreateSettingsTable,
    "2026_09_03_000100_create_settings_table",
    up: |schema| {
        schema
            .create("settings", |t| {
                t.id();
                t.string("key").unique();
                // Text rather than string: a logo path is short, but a list of
                // allowed origins or an exported theme is not, and a silently
                // truncated setting is worse than a long column.
                t.text("value").nullable();
                // Whether the value is encrypted at rest. Read by the store so
                // it knows to decrypt, rather than guessing from the key name.
                t.boolean("is_secret").default_bool(false);
                t.timestamps();
            })
            .await?;

        schema
            .create("backups", |t| {
                t.id();
                t.string("name").unique();
                t.string("path");
                t.big_integer("bytes").default_int(0);
                // `running`, `ready`, `failed` — a backup that died half-way
                // must not look like one you can restore from.
                t.string("status").default("running");
                t.text("note").nullable();
                t.big_integer("created_by").nullable();
                t.timestamps();
            })
            .await
    },
    down: |schema| {
        schema.drop("backups").await?;
        schema.drop("settings").await
    },
);
