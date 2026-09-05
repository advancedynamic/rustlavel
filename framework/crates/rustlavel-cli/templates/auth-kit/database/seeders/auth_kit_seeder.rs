use rustlavel::prelude::*;

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
    ("settings.manage", "Change the application's settings"),
    // `backups.*` moved with the feature: see `modules::backup`, and the
    // seeder below reads `modules::permissions()` alongside this list.
    ("menus.view", "See the navigation menus"),
    ("menus.manage", "Add, edit and reorder navigation items"),
    // No `audit.delete`. An audit log with a delete button is an audit log
    // that whoever matters can edit, which is not an audit log. Pruning it is
    // a scheduled job's decision, not a screen's.
    ("notifications.send", "Send a notification, or announce one to everybody"),
    ("audit.view", "Read the audit trail"),
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
            // The built-in list, then whatever the modules declare. A feature
            // that owns a permission declares it beside the code that checks
            // it, and this is where the two lists meet.
            let owned = crate::modules::permissions();
            let declared = PERMISSIONS.iter().copied().chain(owned.iter().copied());
            for (name, description) in declared {
                if !existing.iter().any(|p| p.name == name) {
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
                let url = std::env::var("APP_URL").unwrap_or_else(|_| "http://localhost:8000".into());
                // Printed, not logged. This is the output of the command
                // rather than a note about it, and it is the only way into a
                // brand new application — a person running with LOG_LEVEL=warn
                // would otherwise seed an administrator they cannot sign in as.
                println!();
                println!("  The first administrator is {email}.");
                println!("  Set their password here: {}/activate/{token}", url.trim_end_matches('/'));
                println!("  The link is good for one hour, and works once.");
                println!();
            }

            Ok(())
        })
    }
}
