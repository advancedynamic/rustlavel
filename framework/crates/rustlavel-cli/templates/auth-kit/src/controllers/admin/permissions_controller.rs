//! Permissions: the individual things a role can be allowed to do.

use rustlavel::prelude::*;

use crate::support::{page, stats};

use super::roles_controller::name_errors;
use super::users_controller::{rbac, with_current_user};

pub struct PermissionsController;

impl PermissionsController {
    pub async fn index(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let store = rbac(&req)?;

        // Which roles hold each one, so deleting a permission is an informed
        // decision rather than a surprise on somebody else's screen.
        let roles = store.roles().await?;
        let mut holders: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
        for role in &roles {
            for permission in store.role_permissions(&role.name).await? {
                holders.entry(permission).or_default().push(role.name.clone());
            }
        }

        let rows: Vec<Json> = store
            .permissions()
            .await?
            .iter()
            .map(|permission| {
                let held = holders.get(&permission.name).cloned().unwrap_or_default();
                Json::object([
                    ("id", Json::from(permission.id)),
                    ("name", Json::from(permission.name.as_str())),
                    ("description", permission.description.clone().map_or(Json::Null, Json::from)),
                    ("roles_empty", Json::from(held.is_empty())),
                    ("roles", Json::Array(held.iter().map(|r| Json::from(r.as_str())).collect())),
                ])
            })
            .collect();

        // Counts that answer what a permissions list is usually opened for.
        // "Orphaned" is the one that earns its place: a permission no role
        // holds does nothing, and there is no way to see that from a table
        // sorted by name.
        let total = rows.len() as i64;
        let orphaned = rows
            .iter()
            .filter(|row| row.get("roles_empty").and_then(Json::as_bool).unwrap_or(false))
            .count() as i64;
        let areas: std::collections::BTreeSet<String> = store
            .permissions()
            .await?
            .iter()
            .map(|p| p.name.split('.').next().unwrap_or_default().to_string())
            .collect();
        let described = rows
            .iter()
            .filter(|row| !row.get("description").is_none_or(Json::is_null))
            .count() as i64;
        let cards = Json::Array(vec![
            stats::card("Permissions", total, stats::BRAND, stats::ICON_LOCK),
            stats::card("In Use", total - orphaned, stats::GOOD, stats::ICON_CHECK),
            stats::card("Orphaned", orphaned, stats::QUIET, stats::ICON_FOLDER),
            stats::card("Areas", areas.len() as i64, stats::PEOPLE, stats::ICON_LAYERS),
            stats::card("Roles", roles.len() as i64, stats::KEYED, stats::ICON_SHIELD),
            stats::card("Described", described, stats::TIMED, stats::ICON_DOCUMENT),
        ]);

        let mut context = page::shell(&req, "permissions").await;
        context = with_current_user(context, &req, &db).await?;
        context = context
            .with("permissions", Json::Array(rows))
            .with("stats", cards)
            .with("can_create", Json::from(req.can("permissions.create").await?))
            .with("can_update", Json::from(req.can("permissions.update").await?))
            .with("can_delete", Json::from(req.can("permissions.delete").await?));
        req.view("admin/permissions/index", &context)
    }

    pub async fn create(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let mut context = page::shell(&req, "permissions").await;
        context = with_current_user(context, &req, &db).await?;
        req.view("admin/permissions/form", &Self::form_context(context, None))
    }

    pub async fn store(mut req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let store = rbac(&req)?;
        let name = req.input("name").unwrap_or_default().trim().to_lowercase();
        let description = req.input("description").unwrap_or_default();

        let errors = name_errors(&name, "permission");
        if !errors.is_empty() {
            let mut context = page::errors(page::shell(&req, "permissions").await, &errors);
            context = with_current_user(context, &req, &db).await?;
            return req.view(
                "admin/permissions/form",
                &Self::form_context(context, None).with("name", Json::from(name)),
            );
        }

        if let Err(error) = store.create_permission_with(&name, &description).await {
            let mut context =
                page::shell(&req, "permissions").await.with("error_summary", Json::from(error.to_string()));
            context = with_current_user(context, &req, &db).await?;
            return req.view(
                "admin/permissions/form",
                &Self::form_context(context, None).with("name", Json::from(name)),
            );
        }

        if let Some(audit) = crate::support::audit::of(&req, "permissions.created") {
            audit.on("Permission", name.as_str()).describe(format!("Created the permission {name}")).record().await;
        }
        page::flash(&req, "success", format!("{name} has been created."));
        Ok(Response::see_other("/admin/permissions"))
    }

    pub async fn edit(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let store = rbac(&req)?;
        let id = req.param_as::<i64>("id").unwrap_or_default();
        let Some(permission) = store.permissions().await?.into_iter().find(|p| p.id == id) else {
            return Ok(Response::not_found());
        };

        let mut context = page::shell(&req, "permissions").await;
        context = with_current_user(context, &req, &db).await?;
        req.view("admin/permissions/form", &Self::form_context(context, Some(&permission)))
    }

    pub async fn update(mut req: Request) -> Result<Response> {
        let store = rbac(&req)?;
        let id = req.param_as::<i64>("id").unwrap_or_default();
        let Some(permission) = store.permissions().await?.into_iter().find(|p| p.id == id) else {
            return Ok(Response::not_found());
        };
        let name = req.input("name").unwrap_or_default().trim().to_lowercase();

        let errors = name_errors(&name, "permission");
        if !errors.is_empty() {
            let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
            let mut context = page::errors(page::shell(&req, "permissions").await, &errors);
            context = with_current_user(context, &req, &db).await?;
            return req.view("admin/permissions/form", &Self::form_context(context, Some(&permission)));
        }

        if name != permission.name {
            // Renaming changes what every role grants and what every check in
            // the codebase is asking about. Worth saying out loud.
            store.rename_permission(&permission.name, &name).await?;
            page::flash(
                &req,
                "warning",
                format!(
                    "Renamed to {name}. Any code still asking for `{}` will now be refused.",
                    permission.name
                ),
            );
        } else {
            page::flash(&req, "success", "Saved.");
        }
        Ok(Response::see_other("/admin/permissions"))
    }

    pub async fn destroy(req: Request) -> Result<Response> {
        let store = rbac(&req)?;
        let id = req.param_as::<i64>("id").unwrap_or_default();
        let Some(permission) = store.permissions().await?.into_iter().find(|p| p.id == id) else {
            return Ok(Response::not_found());
        };

        store.delete_permission(&permission.name).await?;
        page::flash(&req, "warning", format!("{} has been deleted.", permission.name));
        Ok(Response::see_other("/admin/permissions"))
    }

    fn form_context(context: ViewContext, permission: Option<&rustlavel::rbac::Named>) -> ViewContext {
        context
            .with("title", Json::from(if permission.is_some() { "Edit permission" } else { "New permission" }))
            .with(
                "action",
                Json::from(match permission {
                    Some(p) => format!("/admin/permissions/{}", p.id),
                    None => "/admin/permissions".to_string(),
                }),
            )
            .with(
                "submit_label",
                Json::from(if permission.is_some() { "Save changes" } else { "Create permission" }),
            )
            .with("name", Json::from(permission.map(|p| p.name.as_str()).unwrap_or_default()))
            .with(
                "description",
                Json::from(permission.and_then(|p| p.description.clone()).unwrap_or_default()),
            )
    }
}
