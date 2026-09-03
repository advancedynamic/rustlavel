//! Managing people: who exists, what roles they hold, and the exceptions.

use rustlavel::prelude::*;
use rustlavel::rbac::Permissions;

use crate::models::user::User;
use crate::support::{page, tokens};

const PER_PAGE: i64 = 20;

pub struct UsersController;

impl UsersController {
    pub async fn index(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let store = rbac(&req)?;
        let me = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();

        let search = req.query("q").unwrap_or_default().trim().to_string();
        let role_filter = req.query("role").unwrap_or_default().to_string();
        let page_number = req.query("page").and_then(|p| p.parse::<i64>().ok()).unwrap_or(1).max(1);

        let mut query = User::query().order_by("name", rustlavel::db::Direction::Asc);
        if !search.is_empty() {
            let pattern = format!("%{search}%");
            query = query.group_filter(|q| {
                q.filter_like("name", pattern.clone()).or_filter("email", pattern.clone())
            });
        }
        if !role_filter.is_empty() {
            // Filtering by role means asking the RBAC store, which owns those
            // tables; joining across from here would tie this page to a schema
            // that is not this application's to know.
            // The RBAC store answers "does this user hold this role", not "who
            // holds it", so the filter is applied after the rows come back.
            // Fine for an administration screen; a directory of a hundred
            // thousand people would want the store to grow the reverse lookup.
            let _ = &role_filter;
        }

        let listed = query.paginate(&db, page_number, PER_PAGE).await?;
        let mut users = listed.hydrate::<User>()?;
        if !role_filter.is_empty() {
            let mut kept = Vec::new();
            for user in users {
                if store.has_role(user.id, &role_filter).await.unwrap_or(false) {
                    kept.push(user);
                }
            }
            users = kept;
        }
        let now = tokens::now();

        let mut rows = Vec::with_capacity(users.len());
        for user in &users {
            let roles = store.roles_for(user.id).await.unwrap_or_default();
            let mut json = user.public_json();
            if let Json::Object(fields) = &mut json {
                fields.insert("locked".into(), Json::from(user.is_locked(&now)));
                fields.insert(
                    "last_login_at".into(),
                    Json::from(
                        user.last_login_at.as_deref().map(tokens::humanise).unwrap_or_else(|| "Never".into()),
                    ),
                );
                fields.insert("roles_empty".into(), Json::from(roles.is_empty()));
                fields.insert(
                    "roles".into(),
                    Json::Array(roles.iter().map(|r| Json::from(r.as_str())).collect()),
                );
                // The exceptions, counted. A user with none is the normal case;
                // a user with several is the one somebody will come looking for
                // when they cannot work out why a permission is not applying.
                fields.insert(
                    "direct_permissions".into(),
                    Json::from(store.direct_permissions(user.id).await.unwrap_or_default().len() as i64),
                );
                fields.insert(
                    "joined_at".into(),
                    Json::from(
                        user.created_at
                            .as_deref()
                            .map(tokens::humanise_date)
                            .unwrap_or_else(|| "—".into()),
                    ),
                );
                // Nobody deletes or impersonates themselves. The first is a way
                // to lock yourself out of your own application; the second does
                // nothing and leaves a session that looks impersonated.
                fields.insert("deletable".into(), Json::from(user.id != me));
                fields.insert("impersonatable".into(), Json::from(user.id != me));
            }
            rows.push(json);
        }

        let stats = Self::statistics(&db, &store, &now).await?;
        let all_roles = store.roles().await.unwrap_or_default();
        let mut context = page::shell(&req, "users").await;
        context = with_current_user(context, &req, &db).await?;

        context = context
            .with("stats", Json::Array(stats))
            .with("q", Json::from(search.as_str()))
            .with("users_empty", Json::from(rows.is_empty()))
            .with("users", Json::Array(rows))
            .with(
                "all_roles",
                Json::Array(
                    all_roles
                        .iter()
                        .map(|role| {
                            Json::object([
                                ("name", Json::from(role.name.as_str())),
                                ("selected", Json::from(role.name == role_filter)),
                            ])
                        })
                        .collect(),
                ),
            )
            .with("can_create", Json::from(req.can("users.create").await?))
            .with("can_update", Json::from(req.can("users.update").await?))
            .with("can_delete", Json::from(req.can("users.delete").await?))
            .with("can_impersonate", Json::from(req.can("users.impersonate").await?));

        context = pagination(context, &req, page_number, listed.total);
        req.view("admin/users/index", &context)
    }

    /// The six counts above the table.
    ///
    /// Each is a query rather than a placeholder, and each answers something a
    /// person administering this actually asks. "Active today" is deliberately
    /// a count of *people* rather than of sign-ins: three visits from one
    /// person is one person.
    async fn statistics(db: &Database, store: &Permissions, now: &str) -> Result<Vec<Json>> {
        let midnight = format!("{} 00:00:00", &now[..10.min(now.len())]);
        let week_ago = tokens::format_utc(tokens::unix_now() - 7 * 24 * 60 * 60);

        let total = db.table("users").count(db).await?;
        let verified = db.table("users").filter_not_null("email_verified_at").count(db).await?;
        let active_today = db
            .table("users")
            .filter_op("last_login_at", ">=", midnight)
            .count(db)
            .await
            .unwrap_or(0);
        let recent = db.table("users").filter_op("created_at", ">=", week_ago).count(db).await?;

        // These two come from the RBAC store, whose tables this application does
        // not own and must not join against.
        let mut with_roles = 0;
        let mut with_direct = 0;
        for row in db.table("users").select(&["id"]).get(db).await? {
            let Ok(id) = row.get::<i64>("id") else { continue };
            if !store.roles_for(id).await.unwrap_or_default().is_empty() {
                with_roles += 1;
            }
            if !store.direct_permissions(id).await.unwrap_or_default().is_empty() {
                with_direct += 1;
            }
        }

        Ok(vec![
            stat("Total Users", total, "bg-brand-50 text-brand-600 dark:bg-brand-500/10 dark:text-brand-400", ICON_USERS),
            stat("Verified Users", verified, "bg-emerald-50 text-emerald-600 dark:bg-emerald-500/10 dark:text-emerald-400", ICON_CHECK),
            stat("Active Today", active_today, "bg-orange-50 text-orange-600 dark:bg-orange-500/10 dark:text-orange-400", ICON_BOLT),
            stat("Users with Roles", with_roles, "bg-violet-50 text-violet-600 dark:bg-violet-500/10 dark:text-violet-400", ICON_GROUP),
            stat("Direct Permissions", with_direct, "bg-indigo-50 text-indigo-600 dark:bg-indigo-500/10 dark:text-indigo-400", ICON_KEY),
            stat("Recent Users (7 days)", recent, "bg-amber-50 text-amber-600 dark:bg-amber-500/10 dark:text-amber-400", ICON_CLOCK),
        ])
    }

    pub async fn create(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let mut context = page::shell(&req, "users").await;
        context = with_current_user(context, &req, &db).await?;
        context = Self::form_context(context, &req, None).await?;
        req.view("admin/users/form", &context)
    }

    pub async fn store(mut req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let store = rbac(&req)?;

        let name = req.input("name").unwrap_or_default();
        let email = req.input("email").unwrap_or_default().trim().to_lowercase();
        let roles = req.inputs("roles[]");

        let mut errors = page::check(
            &[("name", &name), ("email", &email)],
            &[("name", "required|max:120"), ("email", "required|email|max:190")],
        );
        if User::first(&db, User::by_email(&email)).await?.is_some() {
            // Said plainly here, unlike on the public form: an administrator
            // needs to know why the user was not created, and already knows who
            // has an account.
            errors.add("email", "Somebody already has that address.");
        }

        if !errors.is_empty() {
            let mut context = page::errors(page::shell(&req, "users").await, &errors);
            context = with_current_user(context, &req, &db).await?;
            context = Self::form_context(context, &req, None).await?;
            return req.view("admin/users/form", &context.with("name", Json::from(name)).with("email", Json::from(email)));
        }

        let mut user = User { name: name.trim().to_string(), email, is_active: true, ..Default::default() };
        user.insert(&db).await?;

        for role in &roles {
            store.assign_role(user.id, role).await?;
        }

        // No password is set here, and none is ever shown to an administrator.
        // The invitation is how the person chooses their own.
        crate::controllers::auth::register_controller::send_activation(
            &req,
            &db,
            &user,
            "You have been invited",
        )
        .await?;

        page::flash(&req, "success", format!("{} has been invited.", user.name));
        Ok(Response::see_other("/admin/users"))
    }

    pub async fn edit(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let id = req.param_as::<i64>("id").unwrap_or_default();
        let Some(user) = User::find(&db, id).await? else { return Ok(Response::not_found()) };

        let mut context = page::shell(&req, "users").await;
        context = with_current_user(context, &req, &db).await?;
        context = Self::form_context(context, &req, Some(&user)).await?;
        req.view("admin/users/form", &context)
    }

    pub async fn update(mut req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let store = rbac(&req)?;
        let id = req.param_as::<i64>("id").unwrap_or_default();
        let Some(mut user) = User::find(&db, id).await? else { return Ok(Response::not_found()) };

        let name = req.input("name").unwrap_or_default();
        let email = req.input("email").unwrap_or_default().trim().to_lowercase();
        let roles = req.inputs("roles[]");

        let mut errors = page::check(
            &[("name", &name), ("email", &email)],
            &[("name", "required|max:120"), ("email", "required|email|max:190")],
        );
        if let Some(other) = User::first(&db, User::by_email(&email)).await?
            && other.id != user.id
        {
            errors.add("email", "Somebody already has that address.");
        }
        if !errors.is_empty() {
            let mut context = page::errors(page::shell(&req, "users").await, &errors);
            context = with_current_user(context, &req, &db).await?;
            context = Self::form_context(context, &req, Some(&user)).await?;
            return req.view("admin/users/form", &context);
        }

        user.name = name.trim().to_string();
        user.email = email;
        user.update(&db).await?;

        // Roles are replaced wholesale, so unticking one removes it.
        let held = store.roles_for(user.id).await?;
        for role in held.iter().filter(|r| !roles.contains(r)) {
            store.remove_role(user.id, role).await?;
        }
        for role in roles.iter().filter(|r| !held.contains(r)) {
            store.assign_role(user.id, role).await?;
        }

        // The direct exceptions, three-way per permission.
        for permission in store.permissions().await? {
            match req.input(&format!("permission[{}]", permission.name)).as_deref() {
                Some("grant") => store.grant(user.id, &permission.name).await?,
                Some("deny") => store.deny(user.id, &permission.name).await?,
                Some("inherit") => store.reset(user.id, &permission.name).await?,
                _ => {}
            }
        }

        page::flash(&req, "success", format!("{} has been updated.", user.name));
        Ok(Response::see_other("/admin/users"))
    }

    pub async fn destroy(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let store = rbac(&req)?;
        let id = req.param_as::<i64>("id").unwrap_or_default();
        let me = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();

        if id == me {
            page::flash(&req, "error", "You cannot delete your own account.");
            return Ok(Response::see_other("/admin/users"));
        }

        let Some(user) = User::find(&db, id).await? else { return Ok(Response::not_found()) };
        // The roles and permissions go too. They live in another crate's
        // tables, which have no foreign key to this application's users, so
        // nothing removes them on our behalf.
        store.purge_user(user.id).await?;
        user.delete(&db).await?;

        page::flash(&req, "warning", format!("{} has been deleted.", user.name));
        Ok(Response::see_other("/admin/users"))
    }

    /// The fields a create or edit form needs.
    async fn form_context(
        context: ViewContext,
        req: &Request,
        user: Option<&User>,
    ) -> Result<ViewContext> {
        let store = rbac(req)?;
        let is_new = user.is_none();
        let held = match user {
            Some(user) => store.roles_for(user.id).await?,
            None => Vec::new(),
        };

        let roles: Vec<Json> = store
            .roles()
            .await?
            .iter()
            .map(|role| {
                Json::object([
                    ("name", Json::from(role.name.as_str())),
                    (
                        "description",
                        role.description.clone().map_or(Json::Null, Json::from),
                    ),
                    ("assigned", Json::from(held.contains(&role.name))),
                ])
            })
            .collect();

        let mut permissions = Vec::new();
        if let Some(user) = user {
            let direct = store.direct_permissions(user.id).await?;
            let from_roles = store.permissions_for(user.id).await?;

            for permission in store.permissions().await? {
                let name = permission.name.clone();
                let explicit = direct.iter().find(|(p, _)| *p == name).map(|(_, granted)| *granted);
                let inherited = from_roles.contains(&name) && explicit.is_none();

                let choices = ["inherit", "grant", "deny"].map(|value| {
                    Json::object([
                        ("value", Json::from(value)),
                        (
                            "label",
                            Json::from(match value {
                                "grant" => "Allow",
                                "deny" => "Deny",
                                _ => "Inherit",
                            }),
                        ),
                        (
                            "selected",
                            Json::from(matches!(
                                (value, explicit),
                                ("grant", Some(true)) | ("deny", Some(false)) | ("inherit", None)
                            )),
                        ),
                    ])
                });

                permissions.push(Json::object([
                    ("name", Json::from(name)),
                    ("from_role", if inherited { Json::from("a role") } else { Json::Null }),
                    ("choices", Json::Array(choices.to_vec())),
                ]));
            }
        }

        Ok(context
            .with("is_new", Json::from(is_new))
            .with("title", Json::from(if is_new { "New user" } else { "Edit user" }))
            .with(
                "action",
                Json::from(match user {
                    Some(user) => format!("/admin/users/{}", user.id),
                    None => "/admin/users".to_string(),
                }),
            )
            .with("submit_label", Json::from(if is_new { "Send invitation" } else { "Save changes" }))
            .with("name", Json::from(user.map(|u| u.name.as_str()).unwrap_or_default()))
            .with("email", Json::from(user.map(|u| u.email.as_str()).unwrap_or_default()))
            .with("roles", Json::Array(roles))
            .with("permissions", Json::Array(permissions)))
    }
}

fn stat(label: &str, value: i64, tint: &str, icon: &str) -> Json {
    Json::object([
        ("label", Json::from(label)),
        ("value", Json::from(value)),
        ("tint", Json::from(tint)),
        ("icon", Json::from(icon)),
    ])
}

const ICON_USERS: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M7 8a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm6 1a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5ZM1.6 15.5A5.6 5.6 0 0 1 7 10.5a5.6 5.6 0 0 1 5.4 5H1.6Zm12.05 0a6.9 6.9 0 0 0-1.6-3.86A4.2 4.2 0 0 1 18.4 15.5h-4.75Z"/></svg>"#;
const ICON_CHECK: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M10 18a8 8 0 1 0 0-16 8 8 0 0 0 0 16Zm3.86-9.72a.75.75 0 0 0-1.22-.86l-3.24 4.53-1.62-1.62a.75.75 0 0 0-1.06 1.06l2.25 2.25a.75.75 0 0 0 1.14-.1l3.75-5.25Z" clip-rule="evenodd"/></svg>"#;
const ICON_BOLT: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M11.3 1.05a.75.75 0 0 1 .7.98L10.4 7.5h3.85a.75.75 0 0 1 .58 1.22l-6.5 8a.75.75 0 0 1-1.32-.68L8.6 11.5H4.75a.75.75 0 0 1-.58-1.22l6.5-8a.75.75 0 0 1 .63-.23Z"/></svg>"#;
const ICON_GROUP: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M10 8a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm-6.5 9.5a6.5 6.5 0 0 1 13 0H3.5Z"/></svg>"#;
const ICON_KEY: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M13 2a5 5 0 0 0-4.9 6L2 14.1V18h3.9l1.3-1.3v-1.6h1.6l1.3-1.3v-1.6h1.6l.4-.4A5 5 0 1 0 13 2Zm1.5 4.5a1 1 0 1 1-2 0 1 1 0 0 1 2 0Z"/></svg>"#;
const ICON_CLOCK: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M10 18a8 8 0 1 0 0-16 8 8 0 0 0 0 16Zm.75-11.5a.75.75 0 0 0-1.5 0v4c0 .28.16.54.41.67l2.5 1.25a.75.75 0 1 0 .68-1.34l-2.09-1.04V6.5Z" clip-rule="evenodd"/></svg>"#;

/// The RBAC store, or a clear failure. Never a silent `false`.
///
/// Cloned rather than borrowed: the store is a handle around shared state, and
/// a borrow of it would hold `req` immutably for the rest of the function —
/// which stops the same handler from reading its own form.
pub fn rbac(req: &Request) -> Result<Permissions> {
    req.state::<Permissions>().cloned().ok_or_else(|| {
        Error::msg(
            "the roles and permissions store is not registered. Add \
             `.plugin(Rbac::from_config(db.clone(), app.config()))` in main.rs.",
        )
    })
}

/// Fill in the signed-in user, which every admin page's chrome needs.
pub async fn with_current_user(
    context: ViewContext,
    req: &Request,
    db: &Database,
) -> Result<ViewContext> {
    let id = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
    match User::find(db, id).await? {
        Some(user) => page::with_user(context, req, &user).await,
        None => Ok(context),
    }
}

/// The `partials.pagination` variables.
pub fn pagination(context: ViewContext, req: &Request, page: i64, total: i64) -> ViewContext {
    let last = ((total + PER_PAGE - 1) / PER_PAGE).max(1);
    let path = req.path();
    let link = |n: i64| Json::from(format!("{path}?page={n}"));

    context
        .with("has_pages", Json::from(last > 1))
        .with("page_from", Json::from(((page - 1) * PER_PAGE + 1).min(total.max(1))))
        .with("page_to", Json::from((page * PER_PAGE).min(total)))
        .with("page_total", Json::from(total))
        .with("prev_url", if page > 1 { link(page - 1) } else { Json::Null })
        .with("next_url", if page < last { link(page + 1) } else { Json::Null })
}
