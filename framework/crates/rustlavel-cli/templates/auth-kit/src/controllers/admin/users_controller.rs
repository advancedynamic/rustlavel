//! Managing people: who exists, what roles they hold, and the exceptions.

use rustlavel::prelude::*;
use rustlavel::rbac::Permissions;

use crate::models::user::User;
use crate::support::stats;
use crate::support::{format, page, tokens};

const PER_PAGE: i64 = 20;

pub struct UsersController;

impl UsersController {
    pub async fn index(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let store = rbac(&req)?;
        let me = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();

        let dates = format::Dates::of(&req).await;
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
                        user.last_login_at
                            .as_deref()
                            .map(|at| dates.moment(at))
                            .unwrap_or_else(|| "Never".into()),
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
                            .map(|at| dates.day_of(at))
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
            .with("stats", stats::formatted(&req, Json::Array(stats)).await)
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
            stats::card("Total Users", total, stats::BRAND, stats::ICON_USERS),
            stats::card("Verified Users", verified, stats::GOOD, stats::ICON_CHECK),
            stats::card("Active Today", active_today, stats::BUSY, stats::ICON_BOLT),
            stats::card("Users with Roles", with_roles, stats::PEOPLE, stats::ICON_GROUP),
            stats::card("Direct Perms", with_direct, stats::KEYED, stats::ICON_KEY),
            stats::card("New This Week", recent, stats::TIMED, stats::ICON_CLOCK),
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

        if let Some(audit) = crate::support::audit::of(&req, "users.created") {
            audit
                .on("User", user.id)
                .describe(format!("Invited {} ({})", user.name, user.email))
                .record()
                .await;
        }
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

        // `destroy` has refused to act on your own account since it was
        // written; this did not, and the difference mattered. The form offers
        // every role, `super-admin` included, so a person holding nothing but
        // `users.update` could post it at their own id and hold the super role
        // a cache expiry later. Editing your own name is fine; editing your own
        // grants is the escalation.
        let me = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let changing_grants = !req.inputs("roles[]").is_empty()
            || store.permissions().await?.iter().any(|permission| {
                req.input(&format!("permission[{}]", permission.name)).is_some()
            });

        if id == me && changing_grants {
            page::flash(&req, "error", "You cannot change your own roles or permissions.");
            return Ok(Response::see_other(&format!("/admin/users/{id}/edit")));
        }

        // And a super role holder is not editable by somebody who is not one.
        // Otherwise `users.update` is a way to demote the owner of the
        // application, which is the same escalation approached from the side.
        let supers = store.super_role_names().clone();
        let holds_super = |roles: &[String]| roles.iter().any(|role| supers.contains(role));
        if holds_super(&store.roles_for(id).await?) && !holds_super(&store.roles_for(me).await?) {
            page::flash(&req, "error", "Only a super administrator can edit another one.");
            return Ok(Response::see_other("/admin/users"));
        }

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

        if let Some(audit) = crate::support::audit::of(&req, "users.updated") {
            audit
                .on("User", user.id)
                .describe(format!("Updated the account {}", user.name))
                .record()
                .await;
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

        // The email is kept on the entry. It is the only thing left that
        // identifies which account this was once the row is gone, and "who
        // deleted that account" is the question an audit trail exists for.
        if let Some(audit) = crate::support::audit::of(&req, "users.deleted") {
            audit
                .on("User", user.id)
                .describe(format!("Deleted the account {}", user.name))
                .with("email", Json::from(user.email.as_str()))
                .record()
                .await;
        }
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
