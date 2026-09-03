//! The hashes of passwords somebody used to have.
//!
//! Only here to answer one question — "have you used this one before?" — for
//! Settings → Security → *Password Reuse Prevention*. Nothing is stored that
//! is not already stored on the account itself: an argon2 hash, which is what
//! `users.password_hash` holds. The row is deleted with its user.

use rustlavel::db::migration;

migration!(
    CreatePasswordHistoryTable,
    "2026_09_03_000200_create_password_history_table",
    up: |schema| {
        schema
            .create("password_history", |t| {
                t.id();
                t.foreign_id("user").references("users", "id").cascade_on_delete();
                t.string("password_hash");
                t.timestamps();
            })
            .await
    },
    down: |schema| { schema.drop("password_history").await },
);
