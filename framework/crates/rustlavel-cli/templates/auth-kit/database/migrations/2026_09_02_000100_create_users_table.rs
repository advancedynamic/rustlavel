//! The users table.

use rustlavel::db::migration;

migration!(
    CreateUsersTable,
    "2026_09_02_000100_create_users_table",
    up: |schema| {
        schema
            .create("users", |t| {
                t.id();
                t.string("name");
                t.string("email").unique();
                // Nullable, because a user invited by an administrator exists
                // before they have chosen one. Nothing may sign in until it is
                // set, which the controller checks rather than the schema.
                t.string("password_hash").nullable();
                t.timestamp("email_verified_at").nullable();
                // A locked account is one that failed to sign in too often.
                // Storing the moment it unlocks, rather than a flag plus a
                // counter, means the lock expires without anything sweeping it.
                t.timestamp("locked_until").nullable();
                t.integer("failed_attempts").default_int(0);
                t.timestamp("last_login_at").nullable();
                t.string("last_login_ip").nullable();
                // Bumping this invalidates every session but the current one.
                t.string("session_epoch").nullable();
                t.boolean("is_active").default_bool(true);
                t.timestamps();
            })
            .await
    },
    down: |schema| { schema.drop("users").await },
);
