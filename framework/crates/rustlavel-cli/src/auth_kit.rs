//! The `--with auth-kit` starter kit.
//!
//! Every file below is written into the new project and belongs to it from
//! then on. That is the point: the login page somebody wants to change is a
//! file in their `resources/views`, not a template inside a crate they would
//! have to override. It is the shape Laravel Breeze has, for the same reason.
//!
//! The library half — TOTP, the QR encoder, passkeys, roles and permissions —
//! stays in `rustlavel-auth`, `rustlavel-webauthn` and `rustlavel-rbac`, so a
//! security fix there reaches an application through `cargo update` rather
//! than through a diff nobody applies. Copy-pasted security logic is security
//! logic that never gets fixed.
//!
//! The templates live as real files under `templates/auth-kit/` rather than as
//! string constants, so they can be read, edited and — for the stylesheet —
//! scanned by Tailwind. `include_str!` puts them in the binary at build time.

/// Every file the kit writes, as (path relative to the project, contents).
pub const FILES: &[(&str, &str)] = &[
    ("database/migrations/2026_09_02_000100_create_users_table.rs", include_str!("../templates/auth-kit/database/migrations/2026_09_02_000100_create_users_table.rs")),
    ("database/migrations/2026_09_02_000200_create_user_tokens_table.rs", include_str!("../templates/auth-kit/database/migrations/2026_09_02_000200_create_user_tokens_table.rs")),
    ("database/migrations/2026_09_02_000300_create_login_attempts_table.rs", include_str!("../templates/auth-kit/database/migrations/2026_09_02_000300_create_login_attempts_table.rs")),
    ("database/migrations/2026_09_02_000400_create_user_mfa_tables.rs", include_str!("../templates/auth-kit/database/migrations/2026_09_02_000400_create_user_mfa_tables.rs")),
    ("public/css/app.css", include_str!("../templates/auth-kit/public/css/app.css")),
    ("public/js/app.js", include_str!("../templates/auth-kit/public/js/app.js")),
    ("resources/css/app.css", include_str!("../templates/auth-kit/resources/css/app.css")),
    ("resources/views/admin/permissions/form.rl.html", include_str!("../templates/auth-kit/resources/views/admin/permissions/form.rl.html")),
    ("resources/views/admin/permissions/index.rl.html", include_str!("../templates/auth-kit/resources/views/admin/permissions/index.rl.html")),
    ("resources/views/admin/roles/form.rl.html", include_str!("../templates/auth-kit/resources/views/admin/roles/form.rl.html")),
    ("resources/views/admin/roles/index.rl.html", include_str!("../templates/auth-kit/resources/views/admin/roles/index.rl.html")),
    ("resources/views/admin/users/form.rl.html", include_str!("../templates/auth-kit/resources/views/admin/users/form.rl.html")),
    ("resources/views/admin/users/index.rl.html", include_str!("../templates/auth-kit/resources/views/admin/users/index.rl.html")),
    ("resources/views/auth/activate.rl.html", include_str!("../templates/auth-kit/resources/views/auth/activate.rl.html")),
    ("resources/views/auth/challenge.rl.html", include_str!("../templates/auth-kit/resources/views/auth/challenge.rl.html")),
    ("resources/views/auth/expired.rl.html", include_str!("../templates/auth-kit/resources/views/auth/expired.rl.html")),
    ("resources/views/auth/forgot.rl.html", include_str!("../templates/auth-kit/resources/views/auth/forgot.rl.html")),
    ("resources/views/auth/login.rl.html", include_str!("../templates/auth-kit/resources/views/auth/login.rl.html")),
    ("resources/views/auth/recovery.rl.html", include_str!("../templates/auth-kit/resources/views/auth/recovery.rl.html")),
    ("resources/views/auth/register.rl.html", include_str!("../templates/auth-kit/resources/views/auth/register.rl.html")),
    ("resources/views/auth/sent.rl.html", include_str!("../templates/auth-kit/resources/views/auth/sent.rl.html")),
    ("resources/views/dashboard.rl.html", include_str!("../templates/auth-kit/resources/views/dashboard.rl.html")),
    ("resources/views/layouts/app.rl.html", include_str!("../templates/auth-kit/resources/views/layouts/app.rl.html")),
    ("resources/views/layouts/guest.rl.html", include_str!("../templates/auth-kit/resources/views/layouts/guest.rl.html")),
    ("resources/views/partials/errors.rl.html", include_str!("../templates/auth-kit/resources/views/partials/errors.rl.html")),
    ("resources/views/partials/flash.rl.html", include_str!("../templates/auth-kit/resources/views/partials/flash.rl.html")),
    ("resources/views/partials/impersonation.rl.html", include_str!("../templates/auth-kit/resources/views/partials/impersonation.rl.html")),
    ("resources/views/partials/nav.rl.html", include_str!("../templates/auth-kit/resources/views/partials/nav.rl.html")),
    ("resources/views/partials/pagination.rl.html", include_str!("../templates/auth-kit/resources/views/partials/pagination.rl.html")),
    ("resources/views/profile.rl.html", include_str!("../templates/auth-kit/resources/views/profile.rl.html")),
    ("resources/views/settings/security.rl.html", include_str!("../templates/auth-kit/resources/views/settings/security.rl.html")),
    ("src/controllers/admin/mod.rs", include_str!("../templates/auth-kit/src/controllers/admin/mod.rs")),
    ("src/controllers/admin/permissions_controller.rs", include_str!("../templates/auth-kit/src/controllers/admin/permissions_controller.rs")),
    ("src/controllers/admin/roles_controller.rs", include_str!("../templates/auth-kit/src/controllers/admin/roles_controller.rs")),
    ("src/controllers/admin/users_controller.rs", include_str!("../templates/auth-kit/src/controllers/admin/users_controller.rs")),
    ("src/controllers/auth/login_controller.rs", include_str!("../templates/auth-kit/src/controllers/auth/login_controller.rs")),
    ("src/controllers/auth/mfa_controller.rs", include_str!("../templates/auth-kit/src/controllers/auth/mfa_controller.rs")),
    ("src/controllers/auth/mod.rs", include_str!("../templates/auth-kit/src/controllers/auth/mod.rs")),
    ("src/controllers/auth/password_controller.rs", include_str!("../templates/auth-kit/src/controllers/auth/password_controller.rs")),
    ("src/controllers/auth/register_controller.rs", include_str!("../templates/auth-kit/src/controllers/auth/register_controller.rs")),
    ("src/controllers/dashboard_controller.rs", include_str!("../templates/auth-kit/src/controllers/dashboard_controller.rs")),
    ("src/controllers/mod.rs", include_str!("../templates/auth-kit/src/controllers/mod.rs")),
    ("src/controllers/profile_controller.rs", include_str!("../templates/auth-kit/src/controllers/profile_controller.rs")),
    ("src/controllers/settings_controller.rs", include_str!("../templates/auth-kit/src/controllers/settings_controller.rs")),
    ("src/models/login_attempt.rs", include_str!("../templates/auth-kit/src/models/login_attempt.rs")),
    ("src/models/mod.rs", include_str!("../templates/auth-kit/src/models/mod.rs")),
    ("src/models/user.rs", include_str!("../templates/auth-kit/src/models/user.rs")),
    ("src/models/user_token.rs", include_str!("../templates/auth-kit/src/models/user_token.rs")),
    ("src/routes/auth.rs", include_str!("../templates/auth-kit/src/routes/auth.rs")),
    ("src/routes/mod.rs", include_str!("../templates/auth-kit/src/routes/mod.rs")),
    ("src/routes/web.rs", include_str!("../templates/auth-kit/src/routes/web.rs")),
    ("src/support/lockout.rs", include_str!("../templates/auth-kit/src/support/lockout.rs")),
    ("src/support/mod.rs", include_str!("../templates/auth-kit/src/support/mod.rs")),
    ("src/support/page.rs", include_str!("../templates/auth-kit/src/support/page.rs")),
    ("src/support/passkeys.rs", include_str!("../templates/auth-kit/src/support/passkeys.rs")),
    ("src/support/tokens.rs", include_str!("../templates/auth-kit/src/support/tokens.rs")),
];

/// The lines added to `.env`.
pub const ENV_ADDITIONS: &str = r#"
# --- Authentication ---------------------------------------------------------
# Whether anybody may create an account, or only an administrator.
AUTH_REGISTRATION_OPEN=true
# The shortest password accepted. Length beats complexity rules: they push
# people towards Password1! and away from the passphrase that is actually hard
# to guess, which is why NIST dropped them.
AUTH_PASSWORD_MIN_LENGTH=12

# --- Roles and permissions --------------------------------------------------
# The role that passes every check without holding every permission.
RBAC_SUPER_ROLE=super-admin

# --- Passkeys ---------------------------------------------------------------
# The relying party id is a domain: example.com, never a URL and never a port.
# Left empty it is taken from APP_URL, which is right in development.
WEBAUTHN_ID=
WEBAUTHN_ORIGINS=
"#;

/// `config/auth.json`.
pub const CONFIG_AUTH: &str = r#"{
  "registration": {
    "open": "${AUTH_REGISTRATION_OPEN:true}"
  },
  "password": {
    "min_length": "${AUTH_PASSWORD_MIN_LENGTH:12}"
  }
}
"#;

/// `config/rbac.json`.
pub const CONFIG_RBAC: &str = r#"{
  "super_role": "${RBAC_SUPER_ROLE:super-admin}",
  "cache_ttl_ms": 30000
}
"#;

/// `config/webauthn.json`.
pub const CONFIG_WEBAUTHN: &str = r#"{
  "id": "${WEBAUTHN_ID:}",
  "origins": "${WEBAUTHN_ORIGINS:}",
  "user_verification": "preferred",
  "resident_key": "preferred"
}
"#;

/// `src/main.rs` for a project scaffolded with the kit.
pub const MAIN_RS: &str = r#"use rustlavel::prelude::*;
use {{crate_name}}::{database, routes};

#[rustlavel::main]
async fn main() -> Result<()> {
    let app = App::new()?;

    // The database is resolved once and shared: the pool, the roles store and
    // the passkey store are all handles onto it. Read before the builder is
    // consumed, since `config()` borrows it.
    let url = rustlavel::env::env_or("DATABASE_URL", "");
    if url.is_empty() {
        return Err(Error::msg(
            "DATABASE_URL is not set. The starter kit keeps users, roles and sign-in history \
             in a database, so it needs one before it can start.",
        ));
    }
    let db = Database::connect(&url).await?;
    let cache = CacheStore::from_config(app.config())?;
    let mailer = rustlavel::mail::Mail::from_config(app.config())?;
    let rbac = Rbac::from_config(db.clone(), app.config())?;

    // Sessions on disk rather than in memory, so a restart does not sign
    // everybody out. Swap the store for Redis when there is more than one
    // process, since a session written by one is invisible to the others.
    let sessions = SessionManager::from_config(
        app.config(),
        FileStore::new("storage/sessions"),
    )?;

    app.state(db.clone())
        // The cache backs the per-address half of the sign-in lockout.
        .state(cache)
        .state(mailer)
        // Roles and permissions. `req.can(...)` and the `Can` guard both
        // resolve the store from here, and fail closed if it is missing.
        .plugin(rbac)
        // Order matters: the session has to exist before anything reads a
        // login out of it, and the CSRF check reads the session.
        .middleware(sessions)
        .middleware(Csrf::new())
        .routes(routes::auth::routes)
        .routes(routes::web::routes)
        .migrations(database::migrations::all())
        .seeders(database::seeders::all())
        .run()
        .await
}
"#;

/// The migration registry, holding the kit's own tables and the ones
/// `rustlavel-rbac` brings.
pub const MIGRATIONS_REGISTRY: &str = r#"//! Generated by the rustlavel CLI. Do not edit.
//!
//! A compiled program cannot discover migrations by scanning a directory, so
//! the CLI keeps this list in step with the files beside it.

#[path = "2026_09_02_000100_create_users_table.rs"]
mod create_users_table;
#[path = "2026_09_02_000200_create_user_tokens_table.rs"]
mod create_user_tokens_table;
#[path = "2026_09_02_000300_create_login_attempts_table.rs"]
mod create_login_attempts_table;
#[path = "2026_09_02_000400_create_user_mfa_tables.rs"]
mod create_user_mfa_tables;

use rustlavel::db::Migration;

/// Every migration, in the order they must run.
///
/// The roles and permissions tables come from `rustlavel-rbac` rather than
/// from a file here, so a fix to them arrives with `cargo update` instead of
/// having to be copied into every project that was ever scaffolded.
pub fn all() -> Vec<&'static dyn Migration> {
    let mut migrations: Vec<&'static dyn Migration> = vec![
        &create_users_table::CreateUsersTable,
        &create_user_tokens_table::CreateUserTokensTable,
        &create_login_attempts_table::CreateLoginAttemptsTable,
        &create_user_mfa_tables::CreateUserMfaTables,
    ];
    migrations.extend(rustlavel::rbac::migrations());
    migrations
}
"#;

/// The seeder that makes the application usable: the roles, the permissions,
/// and the first administrator.
pub const SEEDER: &str = r#"use rustlavel::prelude::*;

/// Everything an empty database needs before anybody can sign in.
///
/// Run it with `rustlavel db:seed`. It is written to be safe to run twice:
/// each role and permission is created only if it is missing, so re-running
/// after adding a permission to this list fills in the gap rather than
/// failing half-way.
pub struct AuthKitSeeder;

/// The permissions the generated administration area checks for.
///
/// One per verb rather than a single `users.manage`, so a support role can be
/// given the read half without the delete half.
const PERMISSIONS: &[(&str, &str)] = &[
    ("users.view", "See the list of people and their details"),
    ("users.create", "Invite a new person"),
    ("users.update", "Change somebody's details, roles and permissions"),
    ("users.delete", "Delete an account"),
    ("users.impersonate", "View the site as somebody else"),
    ("roles.view", "See the roles that exist"),
    ("roles.create", "Create a role"),
    ("roles.update", "Change what a role grants"),
    ("roles.delete", "Delete a role"),
    ("permissions.view", "See the permissions that exist"),
    ("permissions.create", "Create a permission"),
    ("permissions.update", "Rename or describe a permission"),
    ("permissions.delete", "Delete a permission"),
];

impl Seeder for AuthKitSeeder {
    fn name(&self) -> &'static str {
        "AuthKitSeeder"
    }

    fn run<'a>(
        &'a self,
        db: &'a Database,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            // The tables come from the migration list, not from here: a
            // seeder that also creates schema is a second, quieter migration
            // path, and the two drift.
            let store = Permissions::from_config(db.clone(), &Config::with_defaults())?;

            let existing = store.permissions().await?;
            for (name, description) in PERMISSIONS {
                if !existing.iter().any(|p| p.name == *name) {
                    store.create_permission_with(name, description).await?;
                }
            }

            let roles = store.roles().await?;
            if !roles.iter().any(|r| r.name == "super-admin") {
                store
                    .create_role_with("super-admin", "Passes every check, without holding every permission")
                    .await?;
            }
            if !roles.iter().any(|r| r.name == "support") {
                store.create_role_with("support", "Can look, and can view the site as somebody else").await?;
                store
                    .set_role_permissions("support", &["users.view", "users.impersonate", "roles.view"])
                    .await?;
            }

            // The first administrator. Created without a password on purpose:
            // a seeded default password is a published default password, and
            // it survives into production more often than anybody admits. The
            // activation link is printed instead.
            let email = std::env::var("ADMIN_EMAIL").unwrap_or_else(|_| "admin@example.com".into());
            if crate::models::user::User::first(db, crate::models::user::User::by_email(&email))
                .await?
                .is_none()
            {
                let mut admin = crate::models::user::User {
                    name: std::env::var("ADMIN_NAME").unwrap_or_else(|_| "Administrator".into()),
                    email: email.clone(),
                    is_active: true,
                    ..Default::default()
                };
                admin.insert(db).await?;
                store.assign_role(admin.id, "super-admin").await?;

                let token = crate::support::tokens::issue(
                    db,
                    admin.id,
                    crate::models::user_token::ACTIVATION,
                    None,
                )
                .await?;
                info!("");
                info!("The first administrator is {email}.");
                info!("Set their password here: /activate/{token}");
                info!("The link is good for one hour.");
                info!("");
            }

            Ok(())
        })
    }
}
"#;

/// The seeder registry.
pub const SEEDERS_REGISTRY: &str = r#"//! Generated by the rustlavel CLI. Do not edit.

#[path = "auth_kit_seeder.rs"]
mod auth_kit_seeder;

use rustlavel::db::Seeder;

pub fn all() -> Vec<&'static dyn Seeder> {
    vec![&auth_kit_seeder::AuthKitSeeder]
}
"#;
