//! Roles: named bundles of permissions.

use rustlavel::prelude::*;
use rustlavel::validation::Errors;

use crate::support::{page, stats};

use super::users_controller::{rbac, with_current_user};

pub struct RolesController;

impl RolesController {
    pub async fn index(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let store = rbac(&req)?;
        let supers = store.super_role_names();

        let mut rows = Vec::new();
        let mut held = 0i64;
        let mut unused = 0i64;
        let mut widest = 0i64;
        for role in store.roles().await? {
            let permissions = store.role_permissions(&role.name).await?;
            let is_super = supers.iter().any(|s| *s == role.name);
            rows.push(Json::object([
                ("id", Json::from(role.id)),
                ("name", Json::from(role.name.as_str())),
                ("description", role.description.clone().map_or(Json::Null, Json::from)),
                ("is_super", Json::from(is_super)),
                ("permissions_empty", Json::from(permissions.is_empty())),
                (
                    "permissions",
                    Json::Array(permissions.iter().take(8).map(|p| Json::from(p.as_str())).collect()),
                ),
                ("user_count", Json::from(store.users_in_role(&role.name).await?)),
                // A super role is not deletable from here. Removing the role
                // that grants everything, from a screen only that role can
                // reach, is a way to lock an application's owner out of it.
                ("deletable", Json::from(!is_super)),
            ]));

            let users = store.users_in_role(&role.name).await?;
            held += users;
            if users == 0 {
                unused += 1;
            }
            widest = widest.max(permissions.len() as i64);
        }

        // Counts an administrator of roles actually asks for. "Unused" is the
        // one worth having: a role nobody holds is either a mistake or
        // something that was meant to be cleaned up, and neither is visible
        // from the table alone.
        let total = rows.len() as i64;
        let cards = Json::Array(vec![
            stats::card("Total Roles", total, stats::BRAND, stats::ICON_SHIELD),
            stats::card("Super Roles", supers.len() as i64, stats::TIMED, stats::ICON_KEY),
            stats::card("Unused Roles", unused, stats::QUIET, stats::ICON_FOLDER),
            stats::card("Assignments", held, stats::PEOPLE, stats::ICON_GROUP),
            stats::card("Largest Role", widest, stats::KEYED, stats::ICON_LAYERS),
            stats::card("Permissions", store.permissions().await?.len() as i64, stats::GOOD, stats::ICON_LOCK),
        ]);

        let mut context = page::shell(&req, "roles").await;
        context = with_current_user(context, &req, &db).await?;
        context = context
            .with("roles", Json::Array(rows))
            .with("stats", stats::formatted(&req, cards).await)
            .with("can_create", Json::from(req.can("roles.create").await?))
            .with("can_update", Json::from(req.can("roles.update").await?))
            .with("can_delete", Json::from(req.can("roles.delete").await?));
        req.view("admin/roles/index", &context)
    }

    pub async fn create(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let mut context = page::shell(&req, "roles").await;
        context = with_current_user(context, &req, &db).await?;
        context = Self::form_context(context, &req, None).await?;
        req.view("admin/roles/form", &context)
    }

    pub async fn store(mut req: Request) -> Result<Response> {
        let store = rbac(&req)?;
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let name = req.input("name").unwrap_or_default().trim().to_lowercase();
        let description = req.input("description").unwrap_or_default();
        let permissions = req.inputs("permissions[]");

        let errors = name_errors(&name, "role");
        if !errors.is_empty() {
            let mut context = page::errors(page::shell(&req, "roles").await, &errors);
            context = with_current_user(context, &req, &db).await?;
            context = Self::form_context(context, &req, None).await?;
            return req.view("admin/roles/form", &context.with("name", Json::from(name)));
        }

        if let Err(error) = store.create_role_with(&name, &description).await {
            let mut context = page::shell(&req, "roles").await.with("error_summary", Json::from(error.to_string()));
            context = with_current_user(context, &req, &db).await?;
            context = Self::form_context(context, &req, None).await?;
            return req.view("admin/roles/form", &context.with("name", Json::from(name)));
        }

        let refs: Vec<&str> = permissions.iter().map(String::as_str).collect();
        store.set_role_permissions(&name, &refs).await?;

        if let Some(audit) = crate::support::audit::of(&req, "roles.created") {
            audit.on("Role", name.as_str()).describe(format!("Created the {name} role")).record().await;
        }
        page::flash(&req, "success", format!("The {name} role has been created."));
        Ok(Response::see_other("/admin/roles"))
    }

    pub async fn edit(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let store = rbac(&req)?;
        let id = req.param_as::<i64>("id").unwrap_or_default();
        let Some(role) = store.roles().await?.into_iter().find(|r| r.id == id) else {
            return Ok(Response::not_found());
        };

        let mut context = page::shell(&req, "roles").await;
        context = with_current_user(context, &req, &db).await?;
        context = Self::form_context(context, &req, Some(&role)).await?;
        req.view("admin/roles/form", &context)
    }

    pub async fn update(mut req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let store = rbac(&req)?;
        let id = req.param_as::<i64>("id").unwrap_or_default();
        let Some(role) = store.roles().await?.into_iter().find(|r| r.id == id) else {
            return Ok(Response::not_found());
        };

        let name = req.input("name").unwrap_or_default().trim().to_lowercase();
        let permissions = req.inputs("permissions[]");

        let errors = name_errors(&name, "role");
        if !errors.is_empty() {
            let mut context = page::errors(page::shell(&req, "roles").await, &errors);
            context = with_current_user(context, &req, &db).await?;
            context = Self::form_context(context, &req, Some(&role)).await?;
            return req.view("admin/roles/form", &context);
        }

        if name != role.name {
            store.rename_role(&role.name, &name).await?;
        }
        let refs: Vec<&str> = permissions.iter().map(String::as_str).collect();
        store.set_role_permissions(&name, &refs).await?;

        if let Some(audit) = crate::support::audit::of(&req, "roles.updated") {
            audit.on("Role", name.as_str()).describe(format!("Changed what the {name} role grants")).record().await;
        }
        page::flash(&req, "success", format!("The {name} role has been updated."));
        Ok(Response::see_other("/admin/roles"))
    }

    pub async fn destroy(req: Request) -> Result<Response> {
        let store = rbac(&req)?;
        let id = req.param_as::<i64>("id").unwrap_or_default();
        let Some(role) = store.roles().await?.into_iter().find(|r| r.id == id) else {
            return Ok(Response::not_found());
        };

        if store.super_role_names().iter().any(|s| *s == role.name) {
            page::flash(&req, "error", "The super role cannot be deleted from here.");
            return Ok(Response::see_other("/admin/roles"));
        }

        store.delete_role(&role.name).await?;
        if let Some(audit) = crate::support::audit::of(&req, "roles.deleted") {
            audit
                .on("Role", role.name.as_str())
                .describe(format!("Deleted the {} role", role.name))
                .record()
                .await;
        }
        page::flash(&req, "warning", format!("The {} role has been deleted.", role.name));
        Ok(Response::see_other("/admin/roles"))
    }

    async fn form_context(
        context: ViewContext,
        req: &Request,
        role: Option<&rustlavel::rbac::Named>,
    ) -> Result<ViewContext> {
        let store = rbac(req)?;
        let held = match role {
            Some(role) => store.role_permissions(&role.name).await?,
            None => Vec::new(),
        };

        // Grouped by the part before the dot, so a long list reads as areas
        // rather than as one column of eighty checkboxes.
        let mut groups: std::collections::BTreeMap<String, Vec<Json>> = std::collections::BTreeMap::new();
        for permission in store.permissions().await? {
            let area = permission.name.split('.').next().unwrap_or("other").to_string();
            groups.entry(area).or_default().push(Json::object([
                ("name", Json::from(permission.name.as_str())),
                ("description", permission.description.clone().map_or(Json::Null, Json::from)),
                ("assigned", Json::from(held.contains(&permission.name))),
            ]));
        }

        let grouped: Vec<Json> = groups
            .into_iter()
            .map(|(label, permissions)| {
                Json::object([
                    ("label", Json::from(label)),
                    ("permissions", Json::Array(permissions)),
                ])
            })
            .collect();

        Ok(context
            .with("title", Json::from(if role.is_some() { "Edit role" } else { "New role" }))
            .with(
                "action",
                Json::from(match role {
                    Some(role) => format!("/admin/roles/{}", role.id),
                    None => "/admin/roles".to_string(),
                }),
            )
            .with("submit_label", Json::from(if role.is_some() { "Save changes" } else { "Create role" }))
            .with("name", Json::from(role.map(|r| r.name.as_str()).unwrap_or_default()))
            .with(
                "description",
                Json::from(role.and_then(|r| r.description.clone()).unwrap_or_default()),
            )
            .with("groups", Json::Array(grouped)))
    }
}

/// A name a person types and code checks against.
///
/// Lowercase, dotted, no spaces — because these end up in source as string
/// literals, and a role called `Content Editor` becomes a bug the first time
/// somebody writes `Can::role("content editor")`.
pub fn name_errors(name: &str, kind: &str) -> Errors {
    let mut errors = page::check(&[("name", name)], &[("name", "required|max:80")]);
    if !name.is_empty()
        && !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-' || c == '_' || c == '*')
    {
        errors.add(
            "name",
            format!("A {kind} name may hold only lowercase letters, digits, dots, dashes and underscores."),
        );
    }
    errors
}
