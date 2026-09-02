//! One table for every single-use link the application emails.
//!
//! Activation, email confirmation and password reset are the same shape — a
//! secret sent to an address, good once, for a while — so they are one table
//! with a `purpose` rather than three that would drift apart.

use rustlavel::db::migration;

migration!(
    CreateUserTokensTable,
    "2026_09_02_000200_create_user_tokens_table",
    up: |schema| {
        schema
            .create("user_tokens", |t| {
                t.id();
                t.foreign_id("user").references("users", "id").cascade_on_delete();
                // `activation`, `password_reset`, `email_change`.
                t.string("purpose").index();
                // The SHA-256 of the token, never the token. A leaked database
                // backup must not be a set of working password-reset links.
                t.string("token_hash").unique();
                // Where an email_change is headed, unused by the others.
                t.string("payload").nullable();
                t.timestamp("expires_at");
                t.timestamp("used_at").nullable();
                t.timestamps();
            })
            .await
    },
    down: |schema| { schema.drop("user_tokens").await },
);
