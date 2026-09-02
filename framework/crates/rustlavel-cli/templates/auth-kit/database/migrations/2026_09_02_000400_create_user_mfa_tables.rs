//! Second factors: authenticator apps, passkeys, and the way back in.

use rustlavel::db::migration;

migration!(
    CreateUserMfaTables,
    "2026_09_02_000400_create_user_mfa_tables",
    up: |schema| {
        schema
            .create("user_totp", |t| {
                t.id();
                t.foreign_id("user").references("users", "id").cascade_on_delete();
                // Encrypted at rest with the application key. A TOTP secret is
                // a password equivalent: anyone holding it can mint codes
                // forever, so it must not sit in the clear beside the hash it
                // is supposed to be independent of.
                t.text("secret_encrypted");
                // Set only once the first code is verified. A row without it is
                // an enrolment somebody abandoned, and must not gate a login.
                t.timestamp("confirmed_at").nullable();
                // The last time step accepted, so a code cannot be replayed
                // inside its own thirty seconds.
                t.big_integer("last_step").nullable();
                t.timestamps();
            })
            .await?;

        schema
            .create("user_passkeys", |t| {
                t.id();
                t.foreign_id("user").references("users", "id").cascade_on_delete();
                t.string("credential_id").unique();
                t.text("public_key");
                // The authenticator's own counter. A value that goes backwards
                // means the credential has been cloned, which is the one thing
                // this column exists to catch.
                t.big_integer("sign_count").default_int(0);
                t.string("label").nullable();
                t.timestamp("last_used_at").nullable();
                t.timestamps();
            })
            .await?;

        schema
            .create("user_recovery_codes", |t| {
                t.id();
                t.foreign_id("user").references("users", "id").cascade_on_delete();
                // Argon2, like a password, because that is what it is.
                t.string("code_hash");
                t.timestamp("used_at").nullable();
                t.timestamps();
            })
            .await
    },
    down: |schema| {
        schema.drop("user_recovery_codes").await?;
        schema.drop("user_passkeys").await?;
        schema.drop("user_totp").await
    },
);
