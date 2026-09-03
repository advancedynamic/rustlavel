//! Every route the starter kit adds.
//!
//! Registered from `main.rs` with `.routes(routes::auth::routes)`. The file is
//! yours: delete what you do not need, and the code behind it stops being
//! reachable.

use rustlavel::prelude::*;

use crate::controllers::admin::appearance_controller::AppearanceController;
use crate::controllers::admin::audit_controller::AuditController;
use crate::controllers::admin::backup_controller::BackupController;
use crate::controllers::admin::menu_controller::MenuController;
use crate::controllers::admin::permissions_controller::PermissionsController;
use crate::controllers::admin::settings_controller::AdminSettingsController;
use crate::controllers::admin::roles_controller::RolesController;
use crate::controllers::admin::users_controller::UsersController;
use crate::controllers::auth::login_controller::LoginController;
use crate::controllers::auth::magic_link_controller::MagicLinkController;
use crate::controllers::auth::mfa_controller::{MfaController, MfaSettingsController};
use crate::controllers::auth::password_controller::PasswordController;
use crate::controllers::auth::register_controller::{ActivationController, RegisterController};
use crate::controllers::dashboard_controller::DashboardController;
use crate::controllers::profile_controller::ProfileController;
use crate::controllers::settings_controller::{ImpersonationController, SettingsController};
use crate::controllers::theme_controller::ThemeController;
use crate::support::idle::IdleTimeout;

pub fn routes(r: &mut Router) {
    // The colours from Settings → Appearance. Public, because the sign-in page
    // is drawn with them and nobody is signed in yet.
    r.get("/css/theme.css", ThemeController::stylesheet).name("theme.css");

    // The uploaded logos. Public for the same reason: the sign-in page shows
    // one, and nobody is signed in yet. The handler serves them under a policy
    // of their own, because an SVG served from this origin is script the browser
    // would otherwise trust.
    r.get("/storage/logos/{file}", AppearanceController::logo).name("logo");

    // --- Open to anyone -------------------------------------------------
    //
    // The throttle is on the endpoints that accept a secret. It is keyed by
    // address and route, so a shared office address exhausting the login
    // allowance still leaves the password-reset form working.
    r.get("/login", LoginController::show).name("login");
    r.post("/login", LoginController::store);
    r.post("/logout", LoginController::destroy).name("logout");

    r.get("/register", RegisterController::show).name("register");
    r.post("/register", RegisterController::store);

    r.get("/activate/{token}", ActivationController::show).name("activate");
    r.post("/activate", ActivationController::store);

    // Magic-link sign-in. The handlers answer 404 while Settings → Security has
    // it switched off, so the routes can be registered unconditionally and the
    // switch can be thrown without a restart.
    r.get("/magic-link", MagicLinkController::show).name("magic.request");
    r.post("/magic-link", MagicLinkController::store);
    r.get("/magic/{token}", MagicLinkController::consume).name("magic.consume");

    r.get("/forgot-password", PasswordController::forgot).name("password.forgot");
    r.post("/forgot-password", PasswordController::send);
    r.get("/reset-password/{token}", PasswordController::reset_form).name("password.reset");
    r.post("/reset-password", PasswordController::reset);

    // --- Half signed in: password accepted, second factor owed -----------
    r.get("/mfa", MfaController::challenge).name("mfa.challenge");
    r.post("/mfa/verify", MfaController::verify);
    r.get("/mfa/recovery", MfaController::recovery_form).name("mfa.recovery");
    r.post("/mfa/recovery", MfaController::recovery);
    r.post("/mfa/passkey/options", MfaController::passkey_options);
    r.post("/mfa/passkey/verify", MfaController::passkey_verify);

    // --- Signed in --------------------------------------------------------
    r.group("", |auth| {
        auth.middleware(Authenticate::default().login_path("/login"));
        auth.middleware(IdleTimeout);

        auth.get("/dashboard", DashboardController::index).name("dashboard");

        auth.get("/profile", ProfileController::show).name("profile");
        auth.post("/profile", ProfileController::update);
        auth.get("/profile/email/{token}", ProfileController::confirm_email).name("profile.email");
        auth.post("/profile/password", ProfileController::change_password).name("profile.password");

        auth.get("/settings/security", SettingsController::security).name("settings.security");
        auth.post("/settings/theme", SettingsController::theme).name("settings.theme");
        auth.post("/settings/security/sessions/revoke", SettingsController::revoke_sessions);

        auth.post("/settings/security/totp/start", MfaSettingsController::start_totp);
        auth.post("/settings/security/totp/confirm", MfaSettingsController::confirm_totp);
        auth.post("/settings/security/totp/disable", MfaSettingsController::disable_totp);
        auth.post("/settings/security/recovery-codes", MfaSettingsController::recovery_codes);
        auth.post("/settings/security/passkeys/options", MfaSettingsController::passkey_options);
        auth.post("/settings/security/passkeys", MfaSettingsController::store_passkey);
        auth.post("/settings/security/passkeys/{id}/delete", MfaSettingsController::delete_passkey);

        // Stopping needs no permission on purpose: an administrator whose
        // rights were revoked while viewing as somebody else must still be
        // able to get back to their own account.
        auth.post("/impersonate/stop", ImpersonationController::stop).name("impersonate.stop");
    });

    // --- Administration ---------------------------------------------------
    //
    // Each verb has its own permission rather than one blanket `users.manage`,
    // so a support role can be given the read half without the delete half.
    r.group("/admin", |admin| {
        admin.middleware(Authenticate::default().login_path("/login"));
        admin.middleware(IdleTimeout);

        admin.get("/users", UsersController::index).name("admin.users").middleware(guard("users.view"));
        admin.get("/users/create", UsersController::create).middleware(guard("users.create"));
        admin.post("/users", UsersController::store).middleware(guard("users.create"));
        admin.get("/users/{id}/edit", UsersController::edit).middleware(guard("users.update"));
        admin.post("/users/{id}", UsersController::update).middleware(guard("users.update"));
        admin.post("/users/{id}/delete", UsersController::destroy).middleware(guard("users.delete"));

        admin.get("/roles", RolesController::index).name("admin.roles").middleware(guard("roles.view"));
        admin.get("/roles/create", RolesController::create).middleware(guard("roles.create"));
        admin.post("/roles", RolesController::store).middleware(guard("roles.create"));
        admin.get("/roles/{id}/edit", RolesController::edit).middleware(guard("roles.update"));
        admin.post("/roles/{id}", RolesController::update).middleware(guard("roles.update"));
        admin.post("/roles/{id}/delete", RolesController::destroy).middleware(guard("roles.delete"));

        // Settings. One permission for the lot: an administrator who can change
        // the mail host can already change the application URL, and pretending
        // otherwise would be six permissions that always travel together.
        admin.get("/settings", AdminSettingsController::index).name("admin.settings").middleware(guard("settings.manage"));
        admin.get("/settings/export", AdminSettingsController::export).middleware(guard("settings.manage"));
        admin.get("/settings/{tab}", AdminSettingsController::tab).middleware(guard("settings.manage"));
        admin.post("/settings/email/test", AdminSettingsController::send_test).middleware(guard("settings.manage"));

        // Appearance saves in three pieces because the page has three Save
        // buttons, and a person who changed only the sidebar should not have
        // their logo settings rewritten by the same click.
        admin.post("/settings/appearance/brand", AppearanceController::save_brand).middleware(guard("settings.manage"));
        admin.post("/settings/appearance/login", AppearanceController::save_login).middleware(guard("settings.manage"));
        admin.post("/settings/appearance/sidebar", AppearanceController::save_sidebar).middleware(guard("settings.manage"));
        admin.post("/settings/appearance/logos", AppearanceController::save_logos).middleware(guard("settings.manage"));
        admin.post("/settings/appearance/logo", AppearanceController::upload).middleware(guard("settings.manage"));

        // Each backup action carries the permission for its own verb rather
        // than the blanket one: restoring replaces every account in the
        // application, which is not the same authority as changing a mail host.
        admin.post("/settings/backup/create", BackupController::store).middleware(guard("backups.create"));
        admin.get("/settings/backup/{id}/download", BackupController::download).middleware(guard("backups.view"));
        admin.post("/settings/backup/{id}/restore", BackupController::restore).middleware(guard("backups.restore"));
        admin.post("/settings/backup/{id}/delete", BackupController::destroy).middleware(guard("backups.delete"));

        // Registered last, so the specific paths above win over `{tab}`.
        admin.post("/settings/{tab}", AdminSettingsController::save).middleware(guard("settings.manage"));

        // Menus. The order matters: `/menus/item/...` is registered before
        // `/menus/{location}`, or the literal `item` would be read as a
        // location and every edit link would 404.
        admin.get("/menus", MenuController::index).name("admin.menus").middleware(guard("menus.view"));
        admin.get("/menus/item/{id}/edit", MenuController::edit).middleware(guard("menus.manage"));
        admin.post("/menus/item/{id}", MenuController::update).middleware(guard("menus.manage"));
        admin.post("/menus/item/{id}/toggle", MenuController::toggle).middleware(guard("menus.manage"));
        admin.post("/menus/item/{id}/delete", MenuController::destroy).middleware(guard("menus.manage"));
        admin.get("/menus/{location}", MenuController::index).middleware(guard("menus.view"));
        admin.get("/menus/{location}/create", MenuController::create).middleware(guard("menus.manage"));
        admin.post("/menus/{location}", MenuController::store).middleware(guard("menus.manage"));
        admin.post("/menus/{location}/reorder", MenuController::reorder).middleware(guard("menus.manage"));
        admin.post("/menus/{location}/clear-cache", MenuController::clear_cache).middleware(guard("menus.manage"));

        // The audit trail. Read-only on purpose: an audit log with a delete
        // button is an audit log anybody who matters can edit. Pruning it is a
        // scheduled job's decision, not a screen's.
        admin.get("/audit", AuditController::index).name("admin.audit").middleware(guard("audit.view"));
        admin.get("/audit/export.csv", AuditController::export_csv).middleware(guard("audit.view"));
        admin.get("/audit/export.pdf", AuditController::export_pdf).middleware(guard("audit.view"));
        admin.get("/audit/{id}", AuditController::show).middleware(guard("audit.view"));

        admin
            .get("/permissions", PermissionsController::index)
            .name("admin.permissions")
            .middleware(guard("permissions.view"));
        admin.get("/permissions/create", PermissionsController::create).middleware(guard("permissions.create"));
        admin.post("/permissions", PermissionsController::store).middleware(guard("permissions.create"));
        admin.get("/permissions/{id}/edit", PermissionsController::edit).middleware(guard("permissions.update"));
        admin.post("/permissions/{id}", PermissionsController::update).middleware(guard("permissions.update"));
        admin
            .post("/permissions/{id}/delete", PermissionsController::destroy)
            .middleware(guard("permissions.delete"));
    });

    r.group("/impersonate", |group| {
        group.middleware(Authenticate::default().login_path("/login"));
        group.middleware(IdleTimeout);
        group.middleware(guard("users.impersonate"));
        group.post("/{id}", ImpersonationController::start).name("impersonate.start");
    });
}

fn guard(permission: &str) -> Can {
    Can::permission(permission).login_path("/login")
}
