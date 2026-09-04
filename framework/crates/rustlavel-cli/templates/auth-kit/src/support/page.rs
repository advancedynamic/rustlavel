//! The variables every page needs, gathered in one place.
//!
//! The layout reads `app_name`, `theme`, `user_name` and the `can_*` flags on
//! every render. Building that by hand in each controller is how one page ends
//! up with a menu the others do not have.

use rustlavel::prelude::*;

use crate::models::menu_item::{self, MenuItem};
use crate::models::user::User;

/// Start a context with everything the layouts need.
///
/// `nav` is the sidebar entry to mark current. Pass `""` for a page with no
/// entry of its own.
pub async fn shell(req: &Request, nav: &str) -> ViewContext {
    // Settings → General, not the configuration file. `Settings::get` prefers
    // the environment variable when one is set and falls back to the declared
    // default, so a deployment that pins `APP_NAME` still wins; reading config
    // directly was how a name typed on that tab reached the database and
    // nothing else. The description and the locale sit on the same tabs and
    // had no reader at all.
    let (name, description, locale) = match req.state::<crate::support::settings::Settings>() {
        Some(settings) => (
            settings.get("app.name").await,
            settings.get("app.description").await,
            settings.get("app.locale").await,
        ),
        None => (req.config().string("app.name", "Rustlavel"), String::new(), "en".into()),
    };

    let mut context = ViewContext::new()
        .with("app_name", Json::from(name.as_str()))
        .with("app_description", Json::from(description.as_str()))
        .with("app_locale", Json::from(if locale.is_empty() { "en" } else { locale.as_str() }))
        .with("app_initial", Json::from(name.chars().next().unwrap_or('R').to_string()))
        .with("nav", Json::from(nav))
        // The field for forms, and the bare token for the layouts' <meta>.
        // The scripts need it on pages where no form happens to render — see
        // the comment on the meta tag in `layouts/app.rl.html`.
        .with("csrf_field", Json::from(rustlavel::auth::csrf::field(req)))
        .with(
            "csrf_token",
            Json::from(rustlavel::auth::csrf::token(req).unwrap_or_default()),
        );

    // The theme is a cookie the server reads, not a class JavaScript adds after
    // the page has painted. Doing it here is what keeps the CSP free of the
    // inline script that the usual no-flash trick needs.
    let theme = req.cookie("theme").filter(|t| t == "dark" || t == "light").unwrap_or_else(|| "light".into());
    context = context
        .with("theme", Json::from(theme.as_str()))
        .with("theme_class", Json::from(if theme == "dark" { "dark" } else { "" }))
        .with("theme_next", Json::from(if theme == "dark" { "light" } else { "dark" }));

    // Whatever a failed form left behind on the way here.
    //
    // `validate(&mut req, ...)?` redirects a browser back to the form and
    // flashes the messages and the input; this is what puts them in front of
    // the template, so a controller that does nothing but propagate the error
    // with `?` still gets a redrawn form. The controllers below also render in
    // place for the cases where they have already loaded a record, and
    // `page::errors` covers that path — the two agree because both write the
    // same `error_<field>` names.
    let errors = req.errors();
    let old = req.old();
    let mut summary: Option<String> = None;

    if let Some(fields) = errors.as_object() {
        for (field, messages) in fields {
            if let Some(message) = messages.as_array().and_then(|m| m.first()).and_then(Json::as_str) {
                if summary.is_none() {
                    summary = Some(message.to_string());
                }
                context = context.with(format!("error_{field}"), Json::from(message));
            }
        }
    }
    if let Some(fields) = old.as_object() {
        for (field, value) in fields {
            context = context.with(format!("old_{field}"), value.clone());
        }
    }
    context = context
        .with("errors", errors)
        .with("old", old)
        .with("error_summary", summary.map_or(Json::Null, Json::from));

    // One flash message, consumed as it is read.
    let session = req.session();
    let message = session.forget("flash_message").and_then(|v| v.as_str().map(str::to_string));
    let kind = session.forget("flash_kind").and_then(|v| v.as_str().map(str::to_string));
    context = context
        .with("flash_message", message.map_or(Json::Null, Json::from))
        .with("flash_kind", Json::from(kind.unwrap_or_else(|| "success".into())));

    context
}

/// Add the signed-in user, their initials, and what the sidebar may show them.
///
/// The `can_*` flags exist so the nav can leave out what a person may not
/// reach. A menu full of links that answer 403 teaches people to ignore the
/// menu — but the flags are decoration, and every route checks again.
pub async fn with_user(mut context: ViewContext, req: &Request, user: &User) -> Result<ViewContext> {
    context = context
        .with("user_name", Json::from(user.name.as_str()))
        .with("user_email", Json::from(user.email.as_str()))
        .with("user_initials", Json::from(user.initials()))
        .with("first_name", Json::from(user.first_name()));

    let impersonating = rustlavel::auth::Impersonation::is_impersonating(req.session());
    context = context.with("impersonating", Json::from(impersonating));

    // The menu somebody built on Settings → Menus, if they built one and if
    // this viewer may see any of it.
    let menu = sidebar(req).await?;
    context = context
        .with("menu_custom", Json::from(!menu.is_empty()))
        .with("menu", Json::Array(menu));

    // Where the Dashboard entry points. An application that has a better first
    // page than the built-in one should be able to say so without editing a
    // template — that is what the Menus screen is for.
    let dashboard = match req.state::<crate::support::settings::Settings>() {
        Some(settings) => settings.get("menus.dashboard_url").await,
        None => String::new(),
    };
    context = context.with(
        "dashboard_url",
        Json::from(match dashboard.trim().is_empty() {
            true => "/dashboard",
            false => dashboard.trim(),
        }),
    );

    for (flag, permission) in [
        ("can_view_users", "users.view"),
        ("can_view_roles", "roles.view"),
        ("can_view_permissions", "permissions.view"),
        ("can_manage_settings", "settings.manage"),
        ("can_view_menus", "menus.view"),
        ("can_view_audit", "audit.view"),
    ] {
        context = context.with(flag, Json::from(req.can(permission).await?));
    }
    Ok(context)
}

/// The sidebar the Menus screen edits, when there is one.
///
/// **The rows that screen writes had no reader.** It has always saved menu
/// items and told a person, in its own empty state, that "the application falls
/// back to its built-in navigation until you add something" — and nothing ever
/// read the table, so adding something changed nothing. This is the reader.
///
/// An item is drawn only when the viewer may reach what it points at, which is
/// the same rule the built-in menu follows: a menu full of links that answer
/// 403 teaches people to ignore the menu. And if that leaves nothing, the
/// built-in navigation comes back rather than a person being left with an empty
/// rail — a custom menu is a convenience, not a way to lock yourself out.
pub async fn sidebar(req: &Request) -> Result<Vec<Json>> {
    let Some(db) = req.state::<Database>() else { return Ok(Vec::new()) };

    let items = MenuItem::get(db, MenuItem::in_location("sidebar")).await?;
    if items.is_empty() {
        return Ok(Vec::new());
    }

    // The viewer's permissions once, rather than a `can` call per item: a menu
    // of twenty entries would otherwise be twenty round trips to draw a rail.
    let granted = req.permission_list().await.unwrap_or_default();
    Ok(entries(items, &granted))
}

/// The rows a viewer with `granted` may see, in the order they are drawn.
///
/// Separated from the loading so it can be tested: which items appear is the
/// part with rules in it, and the part that was wrong for as long as nothing
/// read this table at all.
pub fn entries(items: Vec<MenuItem>, granted: &[String]) -> Vec<Json> {
    let live: Vec<MenuItem> = items.into_iter().filter(|item| item.is_active).collect();

    let mut drawn = Vec::new();
    for node in menu_item::flatten(&menu_item::tree(live)) {
        // Blank means everybody signed in. A permission nobody holds — a typo,
        // or one not created yet — hides the item, and the Menus screen says so
        // when it is saved rather than leaving somebody to wonder.
        if let Some(permission) = node.item.permission.as_deref().filter(|p| !p.is_empty())
            && !granted.iter().any(|held| held == permission)
        {
            continue;
        }

        let href = node.item.href();
        drawn.push(Json::object([
            ("label", Json::from(node.item.label.as_str())),
            ("href", Json::from(href.as_str())),
            (
                "icon",
                Json::from(crate::controllers::admin::menu_controller::icon(
                    node.item.icon.as_deref(),
                )),
            ),
            ("depth", Json::from(node.depth as i64)),
            ("external", Json::from(node.item.is_external())),
            // A parent that points nowhere is a heading, not a dead link.
            ("heading", Json::from(href.starts_with('#'))),
        ]));
    }

    drawn
}

/// Remember a message for the next page this session renders.
pub fn flash(req: &Request, kind: &str, message: impl Into<String>) {
    let session = req.session();
    session.put("flash_message", Json::from(message.into()));
    session.put("flash_kind", Json::from(kind));
}

/// Keep what was typed, so a failed form does not empty itself.
///
/// Never the password: re-filling a password field means putting it back in
/// the HTML, and that HTML ends up in caches, in history and in screenshots.
pub fn old(context: ViewContext, fields: &[(&str, Option<String>)]) -> ViewContext {
    let mut context = context;
    for (name, value) in fields {
        context = context.with(
            format!("old_{name}"),
            Json::from(value.clone().unwrap_or_default()),
        );
    }
    context
}

/// Validate a handful of values that have already been read out of the request.
///
/// `validate(&mut req, ...)` reads the body itself and is the shorter road for
/// a handler that only needs the values afterwards. These controllers have
/// usually read and normalised the fields first — an address is lowercased
/// before it is checked — so they hand the values in rather than the request.
pub fn check(values: &[(&str, &str)], rules: &[(&str, &str)]) -> rustlavel::validation::Errors {
    let input = Json::object(values.iter().map(|(name, value)| (*name, Json::from(*value))));
    rustlavel::validation::Validator::new(&input).rules(rules).errors()
}

/// Turn validation errors into the `error_*` variables the forms read.
pub fn errors(context: ViewContext, errors: &rustlavel::validation::Errors) -> ViewContext {
    let mut context = context;
    let mut first: Option<String> = None;
    for (field, messages) in errors.all() {
        if let Some(message) = messages.first() {
            if first.is_none() {
                first = Some(message.clone());
            }
            context = context.with(format!("error_{field}"), Json::from(message.as_str()));
        }
    }
    context.with("error_summary", first.map_or(Json::Null, Json::from))
}

#[cfg(test)]
mod sidebar_tests {
    use super::*;

    fn item(id: i64, label: &str, route: &str, permission: Option<&str>, parent: Option<i64>) -> MenuItem {
        MenuItem {
            id,
            location: "sidebar".into(),
            parent_id: parent,
            label: label.into(),
            route: Some(route.into()),
            url: None,
            icon: None,
            permission: permission.map(str::to_string),
            sort_order: id,
            is_active: true,
            target: None,
        }
    }

    fn labels(entries: &[Json]) -> Vec<String> {
        entries
            .iter()
            .filter_map(|e| e.get("label").and_then(|l| l.as_str()).map(str::to_string))
            .collect()
    }

    /// The rows the Menus screen writes had no reader at all: the sidebar was
    /// hard-coded, so adding an item changed nothing on the page it promised
    /// to change.
    #[test]
    fn an_item_with_no_permission_is_drawn_for_anybody_signed_in() {
        let entries = entries(vec![item(1, "Reports", "/reports", None, None)], &[]);
        assert_eq!(labels(&entries), ["Reports"]);
    }

    /// The same rule the built-in menu follows: a link that answers 403
    /// teaches people to ignore the menu.
    #[test]
    fn an_item_is_drawn_only_for_somebody_who_holds_its_permission() {
        let rows = vec![item(1, "Openings", "/ats/job/opening", Some("ats.job.opening"), None)];

        assert!(labels(&entries(rows.clone(), &[])).is_empty());
        assert!(labels(&entries(rows.clone(), &["users.view".into()])).is_empty());
        assert_eq!(labels(&entries(rows, &["ats.job.opening".into()])), ["Openings"]);
    }

    /// A child is drawn under its parent, and says how deep it is so the rail
    /// can indent it.
    #[test]
    fn a_child_follows_its_parent_and_carries_its_depth() {
        let rows = vec![
            item(1, "ATS", "#ats", None, None),
            item(2, "Openings", "/ats/job/opening", None, Some(1)),
        ];
        let entries = entries(rows, &[]);

        assert_eq!(labels(&entries), ["ATS", "Openings"]);
        assert_eq!(entries[0].get("depth").and_then(|d| d.as_f64()), Some(0.0));
        assert_eq!(entries[1].get("depth").and_then(|d| d.as_f64()), Some(1.0));
    }

    /// A parent that points at `#` is a grouping label. Drawing it as a link
    /// gives the rail an entry that goes nowhere.
    #[test]
    fn an_item_pointing_at_a_fragment_is_a_heading_not_a_link() {
        let group = entries(vec![item(1, "ATS", "#ats", None, None)], &[]);
        assert_eq!(group[0].get("heading").and_then(|h| h.as_bool()), Some(true));

        let link = entries(vec![item(1, "Reports", "/reports", None, None)], &[]);
        assert_eq!(link[0].get("heading").and_then(|h| h.as_bool()), Some(false));
    }

    #[test]
    fn an_item_switched_off_is_not_drawn() {
        let mut off = item(1, "Hidden", "/hidden", None, None);
        off.is_active = false;
        assert!(labels(&entries(vec![off], &[])).is_empty());
    }
}
