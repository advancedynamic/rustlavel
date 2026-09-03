//! The search box in the header, and the notification list beside it.
//!
//! Both are read-only and both answer JSON, because both are a dropdown that
//! appears while somebody is typing or looking — a full page render for either
//! would be a page they never see.
//!
//! **Neither invents a data source.** The search reads the same tables the
//! administration pages list, and a result is only included when the person
//! searching may open the page it links to — a search that finds what you
//! cannot reach is a search that tells you what exists. The notifications are
//! the audit trail, filtered to the entries worth interrupting somebody for;
//! a `notifications` table would be a second record of things already
//! recorded, kept in step by hand.

use rustlavel::prelude::*;
use rustlavel::audit::{Filter, Trail};

use crate::models::menu_item::MenuItem;
use crate::models::user::User;
use crate::support::{audit, settings::CATALOGUE, tokens};

use super::users_controller::rbac;

/// How many of each kind to offer. A dropdown is a shortcut, not a report.
const PER_KIND: usize = 5;

pub struct SearchController;

impl SearchController {
    /// `GET /admin/search?q=…`
    pub async fn index(req: Request) -> Result<Response> {
        let query = req.query("q").unwrap_or_default().trim().to_lowercase();
        if query.len() < 2 {
            // One letter matches most of the application. The dropdown says
            // nothing rather than everything.
            return Ok(Response::json(Json::object([("groups", Json::Array(Vec::new()))])));
        }

        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let mut groups: Vec<Json> = Vec::new();

        if req.can("users.view").await? {
            let users = User::get(
                &db,
                User::query()
                    .group_filter(|group| {
                        group
                            .filter_like("name", format!("%{query}%"))
                            .or_filter_like("email", format!("%{query}%"))
                    })
                    .limit(PER_KIND as i64),
            )
            .await?;
            groups.push(group(
                "People",
                users
                    .iter()
                    .map(|user| {
                        hit(&user.name, &user.email, &format!("/admin/users/{}/edit", user.id))
                    })
                    .collect(),
            ));
        }

        if req.can("roles.view").await? {
            let store = rbac(&req)?;
            let roles: Vec<Json> = store
                .roles()
                .await?
                .iter()
                .filter(|role| role.name.to_lowercase().contains(&query))
                .take(PER_KIND)
                .map(|role| {
                    hit(
                        &role.name,
                        role.description.as_deref().unwrap_or("Role"),
                        &format!("/admin/roles/{}/edit", role.id),
                    )
                })
                .collect();
            groups.push(group("Roles", roles));

            let permissions: Vec<Json> = store
                .permissions()
                .await?
                .iter()
                .filter(|p| p.name.to_lowercase().contains(&query))
                .take(PER_KIND)
                .map(|p| {
                    hit(
                        &p.name,
                        p.description.as_deref().unwrap_or("Permission"),
                        &format!("/admin/permissions/{}/edit", p.id),
                    )
                })
                .collect();
            groups.push(group("Permissions", permissions));
        }

        if req.can("menus.view").await? {
            let items = MenuItem::get(&db, MenuItem::query().limit(200)).await?;
            groups.push(group(
                "Menu items",
                items
                    .iter()
                    .filter(|item| item.label.to_lowercase().contains(&query))
                    .take(PER_KIND)
                    .map(|item| {
                        hit(&item.label, &item.href(), &format!("/admin/menus/item/{}/edit", item.id))
                    })
                    .collect(),
            ));
        }

        if req.can("settings.manage").await? {
            // The catalogue, not the stored values: a setting nobody has
            // changed is still a setting somebody is looking for.
            groups.push(group(
                "Settings",
                CATALOGUE
                    .iter()
                    .filter(|setting| setting.key.to_lowercase().contains(&query))
                    .take(PER_KIND)
                    .map(|setting| {
                        let tab = setting.key.split('.').next().unwrap_or("general");
                        hit(setting.key, "Setting", &format!("/admin/settings/{}", tab_for(tab)))
                    })
                    .collect(),
            ));
        }

        groups.retain(|group| {
            group.get("hits").and_then(Json::as_array).is_some_and(|hits| !hits.is_empty())
        });
        Ok(Response::json(Json::object([("groups", Json::Array(groups))])))
    }

    /// `GET /admin/notifications` — the audit trail, as a dropdown.
    pub async fn notifications(req: Request) -> Result<Response> {
        let Some(trail) = req.state::<Trail>() else {
            return Ok(Response::json(Json::object([("items", Json::Array(Vec::new()))])));
        };
        if !req.can("audit.view").await? {
            return Ok(Response::json(Json::object([("items", Json::Array(Vec::new()))])));
        }

        let entries = trail.all(&Filter::default(), 40).await?;
        let items: Vec<Json> = entries
            .iter()
            .filter(|entry| worth_reporting(&entry.event))
            .take(8)
            .map(|entry| {
                Json::object([
                    ("id", Json::from(entry.id)),
                    ("text", Json::from(entry.summary())),
                    ("when", Json::from(tokens::humanise(&audit::stamp(&entry.created_at)))),
                    ("tint", Json::from(tint(&entry.event))),
                    ("href", Json::from(format!("/admin/audit/{}", entry.id))),
                ])
            })
            .collect();

        Ok(Response::json(Json::object([
            ("count", Json::from(items.len() as i64)),
            ("items", Json::Array(items)),
        ])))
    }
}

/// Which events are worth putting in front of somebody.
///
/// A sign-in is not: an administrator who is told about every sign-in stops
/// reading the list, and then misses the restore. Deletions, restores and
/// settings changes are.
fn worth_reporting(event: &str) -> bool {
    let verb = event.rsplit('.').next().unwrap_or(event);
    matches!(verb, "deleted" | "restored" | "updated" | "created" | "revoked" | "failed")
        && !event.starts_with("logged")
}

fn tint(event: &str) -> &'static str {
    match event.rsplit('.').next().unwrap_or(event) {
        "deleted" | "revoked" | "failed" => "bg-red-50 text-red-600 dark:bg-red-500/10 dark:text-red-400",
        "restored" | "created" => "bg-emerald-50 text-emerald-600 dark:bg-emerald-500/10 dark:text-emerald-400",
        _ => "bg-amber-50 text-amber-600 dark:bg-amber-500/10 dark:text-amber-400",
    }
}

/// Which Settings tab a key lives on.
fn tab_for(root: &str) -> &'static str {
    match root {
        "mail" => "email",
        "auth" => "security",
        "backup" => "backup",
        "theme" => "appearance",
        _ => "general",
    }
}

fn group(label: &str, hits: Vec<Json>) -> Json {
    Json::object([("label", Json::from(label)), ("hits", Json::Array(hits))])
}

fn hit(title: &str, note: &str, href: &str) -> Json {
    Json::object([
        ("title", Json::from(title)),
        ("note", Json::from(note)),
        ("href", Json::from(href)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A list that reports every sign-in is a list nobody reads, and then the
    /// restore goes past unnoticed.
    #[test]
    fn a_sign_in_is_not_worth_interrupting_somebody_for() {
        assert!(!worth_reporting("logged_in"));
        assert!(!worth_reporting("logged_out"));

        assert!(worth_reporting("users.deleted"));
        assert!(worth_reporting("backups.restored"));
        assert!(worth_reporting("settings.updated"));
        assert!(!worth_reporting("something.viewed"));
    }

    #[test]
    fn a_setting_links_to_the_tab_it_is_actually_on() {
        assert_eq!(tab_for("mail"), "email");
        assert_eq!(tab_for("auth"), "security");
        assert_eq!(tab_for("theme"), "appearance");
        assert_eq!(tab_for("app"), "general");
    }
}
