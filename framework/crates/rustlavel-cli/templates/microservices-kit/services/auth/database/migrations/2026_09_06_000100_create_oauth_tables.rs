//! The authorization server's tables, and only its.
//!
//! The resource server has a database of its own and never reads these. A
//! second service reading this schema would be a second service to redeploy
//! whenever it changes, which is the coupling a separate database exists to
//! prevent.

use rustlavel::db::migration;

migration!(
    CreateOauthTables,
    "2026_09_06_000100_create_oauth_tables",
    up: |schema| {
        schema
            .create("oauth_clients", |t| {
                t.id();
                t.string("client_id").unique();
                // Null for a public client, which proves itself with PKCE
                // rather than with a secret it was never able to keep.
                t.string("secret_hash").nullable();
                t.string("name");
                t.text("redirect_uris");
                t.text("scopes");
                t.timestamps();
            })
            .await?;

        schema
            .create("oauth_codes", |t| {
                t.id();
                // The hash, never the code. A leaked database backup should not
                // be a folder of working credentials.
                t.string("code_hash").unique();
                t.string("client_id");
                t.string("subject");
                t.text("scopes");
                t.string("challenge").nullable();
                t.string("redirect_uri");
                t.string("expires_at");
                t.string("used_at").nullable();
            })
            .await?;

        schema
            .create("oauth_tokens", |t| {
                t.id();
                t.string("token_hash").unique();
                t.string("client_id");
                t.string("subject");
                t.text("scopes");
                t.string("expires_at");
                // Set rather than deleted, so a revoked token can still be
                // explained afterwards. A row that vanishes answers nothing.
                t.string("revoked_at").nullable();
            })
            .await
    },
    down: |schema| {
        schema.drop("oauth_tokens").await?;
        schema.drop("oauth_codes").await?;
        schema.drop("oauth_clients").await
    },
);
