//! Every route the starter kit adds.
//!
//! Registered from `main.rs` with `.routes(routes::auth::routes)`. The file is
//! yours: delete what you do not need, and the code behind it stops being
//! reachable.

use rustlavel::prelude::*;

use crate::controllers::admin::permissions_controller::PermissionsController;
use crate::controllers::admin::roles_controller::RolesController;
use crate::controllers::admin::users_controller::UsersController;
use crate::controllers::auth::login_controller::LoginController;
use crate::controllers::auth::mfa_controller::{MfaController, MfaSettingsController};
use crate::controllers::auth::password_controller::PasswordController;
use crate::controllers::auth::register_controller::{ActivationController, RegisterController};
use crate::controllers::dashboard_controller::DashboardController;
use crate::controllers::profile_controller::ProfileController;
use crate::controllers::settings_controller::{ImpersonationController, SettingsController};

pub fn routes(r: &mut Router) {
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
        group.middleware(guard("users.impersonate"));
        group.post("/{id}", ImpersonationController::start).name("impersonate.start");
    });
}

fn guard(permission: &str) -> Can {
    Can::permission(permission).login_path("/login")
}
