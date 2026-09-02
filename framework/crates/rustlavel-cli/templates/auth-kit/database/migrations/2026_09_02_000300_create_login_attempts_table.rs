//! Every sign-in attempt, successful or not.
//!
//! Failures are the interesting half — they are how somebody notices a
//! password being guessed — so both are recorded, and the email is stored as
//! typed even when it matches no account. The password never is, not even
//! hashed: an audit table is the wrong place to keep a second copy of one.

use rustlavel::db::migration;

migration!(
    CreateLoginAttemptsTable,
    "2026_09_02_000300_create_login_attempts_table",
    up: |schema| {
        schema
            .create("login_attempts", |t| {
                t.id();
                // Nullable: an attempt against an address with no account still
                // matters, and there is no user to point at.
                t.big_integer("user_id").nullable().index();
                t.string("email").index();
                t.boolean("successful").default_bool(false);
                // `bad_password`, `unknown_email`, `locked`, `mfa_failed`,
                // `inactive` — so a report can tell one kind of failure apart
                // from another.
                t.string("reason").nullable();
                t.string("ip").nullable();
                t.string("user_agent").nullable();
                t.timestamp("created_at").default_now().index();
            })
            .await
    },
    down: |schema| { schema.drop("login_attempts").await },
);
