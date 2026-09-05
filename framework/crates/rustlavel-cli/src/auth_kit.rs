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
///
/// Text only — see `BINARY_FILES` for the rest. The split is not tidiness: the
/// entries here are run through the placeholder renderer on the way out, and a
/// woff2 put through a text renderer is a corrupt woff2.
pub const FILES: &[(&str, &str)] = &[
    ("src/lib.rs", include_str!("../templates/auth-kit/src/lib.rs")),
    ("src/main.rs", include_str!("../templates/auth-kit/src/main.rs")),
    ("database/migrations/mod.rs", include_str!("../templates/auth-kit/database/migrations/mod.rs")),
    ("database/seeders/mod.rs", include_str!("../templates/auth-kit/database/seeders/mod.rs")),
    ("database/seeders/auth_kit_seeder.rs", include_str!("../templates/auth-kit/database/seeders/auth_kit_seeder.rs")),
    ("config/auth.json", include_str!("../templates/auth-kit/config/auth.json")),
    ("config/rbac.json", include_str!("../templates/auth-kit/config/rbac.json")),
    ("config/webauthn.json", include_str!("../templates/auth-kit/config/webauthn.json")),
    ("src/modules/mod.rs", include_str!("../templates/auth-kit/src/modules/mod.rs")),
    ("src/modules/backup/mod.rs", include_str!("../templates/auth-kit/src/modules/backup/mod.rs")),
    ("src/modules/backup/archive.rs", include_str!("../templates/auth-kit/src/modules/backup/archive.rs")),
    ("src/modules/backup/controller.rs", include_str!("../templates/auth-kit/src/modules/backup/controller.rs")),
    ("src/modules/backup/schedule.rs", include_str!("../templates/auth-kit/src/modules/backup/schedule.rs")),
    ("lang/en.json", include_str!("../templates/auth-kit/lang/en.json")),
    ("lang/id.json", include_str!("../templates/auth-kit/lang/id.json")),
    ("database/migrations/2026_09_02_000100_create_users_table.rs", include_str!("../templates/auth-kit/database/migrations/2026_09_02_000100_create_users_table.rs")),
    ("database/migrations/2026_09_02_000200_create_user_tokens_table.rs", include_str!("../templates/auth-kit/database/migrations/2026_09_02_000200_create_user_tokens_table.rs")),
    ("database/migrations/2026_09_02_000300_create_login_attempts_table.rs", include_str!("../templates/auth-kit/database/migrations/2026_09_02_000300_create_login_attempts_table.rs")),
    ("database/migrations/2026_09_02_000400_create_user_mfa_tables.rs", include_str!("../templates/auth-kit/database/migrations/2026_09_02_000400_create_user_mfa_tables.rs")),
    ("database/migrations/2026_09_03_000100_create_settings_table.rs", include_str!("../templates/auth-kit/database/migrations/2026_09_03_000100_create_settings_table.rs")),
    ("database/migrations/2026_09_03_000200_create_password_history_table.rs", include_str!("../templates/auth-kit/database/migrations/2026_09_03_000200_create_password_history_table.rs")),
    ("src/models/notification.rs", include_str!("../templates/auth-kit/src/models/notification.rs")),
    ("src/controllers/notification_controller.rs", include_str!("../templates/auth-kit/src/controllers/notification_controller.rs")),
    ("resources/views/notifications/index.rl.html", include_str!("../templates/auth-kit/resources/views/notifications/index.rl.html")),
    ("database/migrations/2026_09_04_000100_create_notifications_table.rs", include_str!("../templates/auth-kit/database/migrations/2026_09_04_000100_create_notifications_table.rs")),
    ("database/migrations/2026_09_03_000300_create_menu_items_table.rs", include_str!("../templates/auth-kit/database/migrations/2026_09_03_000300_create_menu_items_table.rs")),
    ("public/css/app.css", include_str!("../templates/auth-kit/public/css/app.css")),
    ("public/js/app.js", include_str!("../templates/auth-kit/public/js/app.js")),
    ("public/js/appearance.js", include_str!("../templates/auth-kit/public/js/appearance.js")),
    ("resources/css/app.css", include_str!("../templates/auth-kit/resources/css/app.css")),
    ("resources/views/admin/audit/index.rl.html", include_str!("../templates/auth-kit/resources/views/admin/audit/index.rl.html")),
    ("resources/views/admin/audit/show.rl.html", include_str!("../templates/auth-kit/resources/views/admin/audit/show.rl.html")),
    ("resources/views/admin/menus/form.rl.html", include_str!("../templates/auth-kit/resources/views/admin/menus/form.rl.html")),
    ("resources/views/admin/menus/index.rl.html", include_str!("../templates/auth-kit/resources/views/admin/menus/index.rl.html")),
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
    ("resources/views/auth/magic.rl.html", include_str!("../templates/auth-kit/resources/views/auth/magic.rl.html")),
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
    ("resources/views/partials/stats.rl.html", include_str!("../templates/auth-kit/resources/views/partials/stats.rl.html")),
    ("resources/views/profile.rl.html", include_str!("../templates/auth-kit/resources/views/profile.rl.html")),
    ("resources/views/settings/index.rl.html", include_str!("../templates/auth-kit/resources/views/settings/index.rl.html")),
    ("resources/views/settings/security.rl.html", include_str!("../templates/auth-kit/resources/views/settings/security.rl.html")),
    ("resources/views/settings/tabs/appearance.rl.html", include_str!("../templates/auth-kit/resources/views/settings/tabs/appearance.rl.html")),
    ("resources/views/settings/tabs/backup.rl.html", include_str!("../templates/auth-kit/resources/views/settings/tabs/backup.rl.html")),
    ("resources/views/settings/tabs/email.rl.html", include_str!("../templates/auth-kit/resources/views/settings/tabs/email.rl.html")),
    ("resources/views/settings/tabs/general.rl.html", include_str!("../templates/auth-kit/resources/views/settings/tabs/general.rl.html")),
    ("resources/views/settings/tabs/language.rl.html", include_str!("../templates/auth-kit/resources/views/settings/tabs/language.rl.html")),
    ("resources/views/settings/tabs/security.rl.html", include_str!("../templates/auth-kit/resources/views/settings/tabs/security.rl.html")),
    ("src/controllers/admin/appearance_controller.rs", include_str!("../templates/auth-kit/src/controllers/admin/appearance_controller.rs")),
    ("src/controllers/admin/audit_controller.rs", include_str!("../templates/auth-kit/src/controllers/admin/audit_controller.rs")),
    ("src/controllers/admin/menu_controller.rs", include_str!("../templates/auth-kit/src/controllers/admin/menu_controller.rs")),
    ("src/controllers/admin/mod.rs", include_str!("../templates/auth-kit/src/controllers/admin/mod.rs")),
    ("src/controllers/admin/permissions_controller.rs", include_str!("../templates/auth-kit/src/controllers/admin/permissions_controller.rs")),
    ("src/controllers/admin/roles_controller.rs", include_str!("../templates/auth-kit/src/controllers/admin/roles_controller.rs")),
    ("src/controllers/admin/search_controller.rs", include_str!("../templates/auth-kit/src/controllers/admin/search_controller.rs")),
    ("src/controllers/admin/settings_controller.rs", include_str!("../templates/auth-kit/src/controllers/admin/settings_controller.rs")),
    ("src/controllers/admin/users_controller.rs", include_str!("../templates/auth-kit/src/controllers/admin/users_controller.rs")),
    ("src/controllers/auth/login_controller.rs", include_str!("../templates/auth-kit/src/controllers/auth/login_controller.rs")),
    ("src/controllers/auth/magic_link_controller.rs", include_str!("../templates/auth-kit/src/controllers/auth/magic_link_controller.rs")),
    ("src/controllers/auth/mfa_controller.rs", include_str!("../templates/auth-kit/src/controllers/auth/mfa_controller.rs")),
    ("src/controllers/auth/mod.rs", include_str!("../templates/auth-kit/src/controllers/auth/mod.rs")),
    ("src/controllers/auth/password_controller.rs", include_str!("../templates/auth-kit/src/controllers/auth/password_controller.rs")),
    ("src/controllers/auth/register_controller.rs", include_str!("../templates/auth-kit/src/controllers/auth/register_controller.rs")),
    ("src/controllers/dashboard_controller.rs", include_str!("../templates/auth-kit/src/controllers/dashboard_controller.rs")),
    ("src/controllers/mod.rs", include_str!("../templates/auth-kit/src/controllers/mod.rs")),
    ("src/controllers/profile_controller.rs", include_str!("../templates/auth-kit/src/controllers/profile_controller.rs")),
    ("src/controllers/settings_controller.rs", include_str!("../templates/auth-kit/src/controllers/settings_controller.rs")),
    ("src/controllers/theme_controller.rs", include_str!("../templates/auth-kit/src/controllers/theme_controller.rs")),
    ("src/models/login_attempt.rs", include_str!("../templates/auth-kit/src/models/login_attempt.rs")),
    ("src/models/menu_item.rs", include_str!("../templates/auth-kit/src/models/menu_item.rs")),
    ("src/models/mod.rs", include_str!("../templates/auth-kit/src/models/mod.rs")),
    ("src/models/password_history.rs", include_str!("../templates/auth-kit/src/models/password_history.rs")),
    ("src/models/user.rs", include_str!("../templates/auth-kit/src/models/user.rs")),
    ("src/models/user_token.rs", include_str!("../templates/auth-kit/src/models/user_token.rs")),
    ("src/routes/auth.rs", include_str!("../templates/auth-kit/src/routes/auth.rs")),
    ("src/routes/mod.rs", include_str!("../templates/auth-kit/src/routes/mod.rs")),
    ("src/routes/web.rs", include_str!("../templates/auth-kit/src/routes/web.rs")),
    ("src/support/audit.rs", include_str!("../templates/auth-kit/src/support/audit.rs")),
    ("src/support/format.rs", include_str!("../templates/auth-kit/src/support/format.rs")),
    ("src/support/idle.rs", include_str!("../templates/auth-kit/src/support/idle.rs")),
    ("src/support/lockout.rs", include_str!("../templates/auth-kit/src/support/lockout.rs")),
    ("src/support/mail.rs", include_str!("../templates/auth-kit/src/support/mail.rs")),
    ("src/support/mod.rs", include_str!("../templates/auth-kit/src/support/mod.rs")),
    ("src/support/page.rs", include_str!("../templates/auth-kit/src/support/page.rs")),
    ("src/support/palette.rs", include_str!("../templates/auth-kit/src/support/palette.rs")),
    ("src/support/passkeys.rs", include_str!("../templates/auth-kit/src/support/passkeys.rs")),
    ("src/support/passwords.rs", include_str!("../templates/auth-kit/src/support/passwords.rs")),
    ("src/support/pdf.rs", include_str!("../templates/auth-kit/src/support/pdf.rs")),
    ("src/support/settings.rs", include_str!("../templates/auth-kit/src/support/settings.rs")),
    ("src/support/stats.rs", include_str!("../templates/auth-kit/src/support/stats.rs")),
    ("src/support/tokens.rs", include_str!("../templates/auth-kit/src/support/tokens.rs")),
    ("src/support/views.rs", include_str!("../templates/auth-kit/src/support/views.rs")),
    ("tests/web.rs", include_str!("../templates/auth-kit/tests/web.rs")),
];

/// The files that are not text, written byte for byte.
///
/// Inter, subset to Latin and Latin Extended, under the SIL Open Font License
/// — the licence travels with it, because a font shipped without one is a font
/// somebody has to go and look up. Two files rather than one: `unicode-range`
/// in the stylesheet means a page that never shows an accented character never
/// asks for the 85K half.
/// The template `FILES` holds for a path, for a test that wants to read one.
///
/// Reaching for the manifest rather than a named constant is the point: a file
/// the manifest does not carry is a file `new` does not write, so a test that
/// can find it here is testing something a project actually receives.
#[cfg(test)]
pub fn file(path: &str) -> &'static str {
    FILES
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, contents)| *contents)
        .unwrap_or_else(|| panic!("`{path}` is not in the kit's manifest"))
}

pub const BINARY_FILES: &[(&str, &[u8])] = &[
    ("public/fonts/inter-latin.woff2", include_bytes!("../templates/auth-kit/public/fonts/inter-latin.woff2")),
    ("public/fonts/inter-latin-ext.woff2", include_bytes!("../templates/auth-kit/public/fonts/inter-latin-ext.woff2")),
    ("public/fonts/LICENSE.txt", include_bytes!("../templates/auth-kit/public/fonts/LICENSE.txt")),
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

