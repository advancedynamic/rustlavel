//! Settings → Menus: the navigation, edited rather than written.
//!
//! The sidebar in `partials/nav.rl.html` is still the fallback. This page
//! edits rows in `menu_items`, and the layout draws them when there are any —
//! so an application that never opens this screen behaves exactly as before.

use rustlavel::prelude::*;

use crate::models::menu_item::{self, MenuItem};
use crate::support::{page, stats};

use super::users_controller::with_current_user;

/// The menus that exist. A fixed list rather than a table: a location is
/// somewhere the layout draws, and inventing one at runtime would name a place
/// no template renders.
pub const LOCATIONS: &[(&str, &str)] =
    &[("sidebar", "Sidebar Menu"), ("portal", "Portal Menu"), ("topbar", "Topbar Menu")];

/// The icons a menu item may use, by name.
///
/// Names rather than markup, and a closed list rather than a free field: a
/// column holding `<svg>` is a column holding whatever somebody pasted, and
/// this application renders it into a page with no `unsafe-inline`.
pub const ICONS: &[(&str, &str)] = &[
    ("home", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M10 2.5 2 8.6V17h5v-4.5h6V17h5V8.6L10 2.5Z"/></svg>"#),
    ("user", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M10 9a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7Zm-7 8.5a7 7 0 0 1 14 0H3Z"/></svg>"#),
    ("cog", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M8.34 2.5a1 1 0 0 1 .98-.8h1.36a1 1 0 0 1 .98.8l.2 1a6.5 6.5 0 0 1 1.3.75l.96-.34a1 1 0 0 1 1.2.45l.68 1.18a1 1 0 0 1-.22 1.25l-.76.66a6.6 6.6 0 0 1 0 1.5l.76.66a1 1 0 0 1 .22 1.25l-.68 1.18a1 1 0 0 1-1.2.45l-.96-.34c-.4.31-.84.56-1.3.75l-.2 1a1 1 0 0 1-.98.8H9.32a1 1 0 0 1-.98-.8l-.2-1a6.5 6.5 0 0 1-1.3-.75l-.96.34a1 1 0 0 1-1.2-.45l-.68-1.18a1 1 0 0 1 .22-1.25l.76-.66a6.6 6.6 0 0 1 0-1.5l-.76-.66a1 1 0 0 1-.22-1.25l.68-1.18a1 1 0 0 1 1.2-.45l.96.34c.4-.31.84-.56 1.3-.75l.2-1ZM10 12.5a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5Z" clip-rule="evenodd"/></svg>"#),
    ("document", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M4.5 3.5A1.5 1.5 0 0 1 6 2h4.6L15.5 6.9V16.5A1.5 1.5 0 0 1 14 18H6a1.5 1.5 0 0 1-1.5-1.5v-13ZM10.5 3.6V7h3.4l-3.4-3.4Z" clip-rule="evenodd"/></svg>"#),
    ("menu", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M3 5.25A.75.75 0 0 1 3.75 4.5h12.5a.75.75 0 0 1 0 1.5H3.75A.75.75 0 0 1 3 5.25Zm0 4.75a.75.75 0 0 1 .75-.75h12.5a.75.75 0 0 1 0 1.5H3.75A.75.75 0 0 1 3 10Zm0 4.75a.75.75 0 0 1 .75-.75h12.5a.75.75 0 0 1 0 1.5H3.75a.75.75 0 0 1-.75-.75Z"/></svg>"#),
    ("chart-bar", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M3 16.5V9h3v7.5H3Zm5.5 0V3.5h3v13h-3Zm5.5 0V12h3v4.5h-3Z"/></svg>"#),
    ("folder", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M2 5.5A1.5 1.5 0 0 1 3.5 4h3.38a1.5 1.5 0 0 1 1.06.44l1.12 1.12H16.5A1.5 1.5 0 0 1 18 7v7.5a1.5 1.5 0 0 1-1.5 1.5h-13A1.5 1.5 0 0 1 2 14.5v-9Z"/></svg>"#),
    ("shield", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M10 1.5 3.5 4v5.2c0 3.9 2.6 7.5 6.5 9.3 3.9-1.8 6.5-5.4 6.5-9.3V4L10 1.5Z" clip-rule="evenodd"/></svg>"#),
    ("lock", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M10 1.5A3.5 3.5 0 0 0 6.5 5v2H6a2 2 0 0 0-2 2v7a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2V9a2 2 0 0 0-2-2h-.5V5A3.5 3.5 0 0 0 10 1.5ZM12 7V5a2 2 0 1 0-4 0v2h4Z" clip-rule="evenodd"/></svg>"#),
    ("bell", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M10 2a5 5 0 0 0-5 5v3l-1.5 3h13L15 10V7a5 5 0 0 0-5-5Zm-2 13a2 2 0 1 0 4 0H8Z"/></svg>"#),
    // A second row, added because ten icons ran out the moment somebody built
    // a menu for something this kit knows nothing about. All from the same
    // 20x20 grid as the ten above, so they line up in the picker.
    ("briefcase", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M7.5 4A1.5 1.5 0 0 1 9 2.5h2A1.5 1.5 0 0 1 12.5 4v1h2.75A1.75 1.75 0 0 1 17 6.75V9H3V6.75A1.75 1.75 0 0 1 4.75 5H7.5V4Zm1.5 1h2V4H9v1ZM3 10.5V15.25c0 .966.784 1.75 1.75 1.75h10.5A1.75 1.75 0 0 0 17 15.25V10.5H3Z" clip-rule="evenodd"/></svg>"#),
    ("users", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M7 9a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm7 0a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5ZM1.5 17a5.5 5.5 0 0 1 11 0h-11Zm12.2 0a6.9 6.9 0 0 0-1.36-3.9A4.5 4.5 0 0 1 18.5 17h-4.8Z"/></svg>"#),
    ("calendar", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M6 2.75A.75.75 0 0 1 6.75 3.5v.5h6.5v-.5a.75.75 0 0 1 1.5 0v.5h.5A1.75 1.75 0 0 1 17 5.75v9.5A1.75 1.75 0 0 1 15.25 17H4.75A1.75 1.75 0 0 1 3 15.25v-9.5A1.75 1.75 0 0 1 4.75 4h.5v-.5A.75.75 0 0 1 6 2.75ZM4.5 8v7.25c0 .138.112.25.25.25h10.5a.25.25 0 0 0 .25-.25V8h-11Z" clip-rule="evenodd"/></svg>"#),
    ("inbox", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M3 5.75A1.75 1.75 0 0 1 4.75 4h10.5A1.75 1.75 0 0 1 17 5.75v8.5A1.75 1.75 0 0 1 15.25 16H4.75A1.75 1.75 0 0 1 3 14.25v-8.5ZM4.5 11.5v2.75c0 .138.112.25.25.25h10.5a.25.25 0 0 0 .25-.25V11.5h-3a2.5 2.5 0 0 1-5 0h-3Z" clip-rule="evenodd"/></svg>"#),
    ("tag", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M3 4.75A1.75 1.75 0 0 1 4.75 3h4.34c.46 0 .9.18 1.23.51l6.17 6.17a1.75 1.75 0 0 1 0 2.48l-4.33 4.33a1.75 1.75 0 0 1-2.48 0L3.51 10.32A1.75 1.75 0 0 1 3 9.09V4.75ZM6.5 7.5a1 1 0 1 0 0-2 1 1 0 0 0 0 2Z" clip-rule="evenodd"/></svg>"#),
    ("map-pin", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M10 1.5a5.5 5.5 0 0 0-5.5 5.5c0 4.02 4.7 10.2 4.9 10.46a.75.75 0 0 0 1.2 0C10.8 17.2 15.5 11.02 15.5 7A5.5 5.5 0 0 0 10 1.5Zm0 7.5a2 2 0 1 1 0-4 2 2 0 0 1 0 4Z" clip-rule="evenodd"/></svg>"#),
    ("credit-card", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M3 6.75A1.75 1.75 0 0 1 4.75 5h10.5A1.75 1.75 0 0 1 17 6.75V8H3V6.75ZM3 10v3.25A1.75 1.75 0 0 0 4.75 15h10.5A1.75 1.75 0 0 0 17 13.25V10H3Zm2 2.5h3v1H5v-1Z"/></svg>"#),
    ("box", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="m10 2 7 3.2v9.6L10 18l-7-3.2V5.2L10 2Zm0 1.7L5.1 5.9 10 8.1l4.9-2.2L10 3.7ZM4.5 7.1v6.7l4.75 2.17V9.27L4.5 7.1Zm11 0-4.75 2.17v6.7L15.5 13.8V7.1Z"/></svg>"#),
    ("clipboard", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M8 2.5A1.5 1.5 0 0 0 6.5 4h-.75A1.75 1.75 0 0 0 4 5.75v10.5A1.75 1.75 0 0 0 5.75 18h8.5A1.75 1.75 0 0 0 16 16.25V5.75A1.75 1.75 0 0 0 14.25 4h-.75A1.5 1.5 0 0 0 12 2.5H8ZM8 4h4v.5H8V4Z" clip-rule="evenodd"/></svg>"#),
    ("chat", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M10 3c-4.14 0-7.5 2.69-7.5 6 0 1.87 1.07 3.54 2.75 4.64L4.5 17l3.4-1.7c.67.13 1.37.2 2.1.2 4.14 0 7.5-2.69 7.5-6S14.14 3 10 3Z" clip-rule="evenodd"/></svg>"#),
    ("wrench", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M13.5 2a4.5 4.5 0 0 0-4.32 5.79L3.3 13.67a1.75 1.75 0 0 0 2.48 2.47l5.88-5.88A4.5 4.5 0 0 0 17.4 5.2l-2.3 2.3-2.12-2.12 2.3-2.3A4.5 4.5 0 0 0 13.5 2Z" clip-rule="evenodd"/></svg>"#),
    ("truck", r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M2 6.25A1.25 1.25 0 0 1 3.25 5h7.5A1.25 1.25 0 0 1 12 6.25V8h1.9c.4 0 .78.19 1.02.51l1.85 2.47c.15.2.23.45.23.7v2.07A1.25 1.25 0 0 1 15.75 15h-.35a2 2 0 0 1-4 0h-3.3a2 2 0 0 1-4 0h-.85A1.25 1.25 0 0 1 2 13.75v-7.5ZM12 9.5v2h3.2l-1.5-2H12Z"/></svg>"#),
];

/// The markup for an icon name, or nothing.
pub fn icon(name: Option<&str>) -> &'static str {
    let Some(name) = name else { return "" };
    ICONS.iter().find(|(key, _)| *key == name).map(|(_, svg)| *svg).unwrap_or("")
}

/// Say so when an item names a permission that does not exist.
///
/// The field is free text on purpose — a menu may point at a feature whose
/// permission has not been created yet. What is not on purpose is the silence:
/// an item guarded by a permission nobody holds is drawn for nobody, so it is
/// saved, it is listed on this screen, and it never appears in the sidebar. A
/// person then reports that menus do not work, which is a fair reading of what
/// they saw.
async fn warn_if_unknown(req: &Request, item: &MenuItem) {
    let Some(permission) = item.permission.as_deref().filter(|p| !p.is_empty()) else { return };
    let Some(store) = req.state::<rustlavel::rbac::Permissions>() else { return };

    let known = store.permissions().await.unwrap_or_default();
    if known.iter().any(|p| p.name == permission) {
        return;
    }

    page::flash(
        req,
        "error",
        format!(
            "Saved, but no permission is called `{permission}`, so this item is hidden from              everybody until one exists. Create it under Permissions, or clear the field to              show the item to everybody signed in."
        ),
    );
}

pub struct MenuController;

impl MenuController {
    /// `POST /admin/menus/dashboard` — where the Dashboard entry points.
    ///
    /// On this screen rather than in Settings because it is navigation, and
    /// this is the screen that edits navigation. Blank restores the built-in
    /// page rather than leaving an entry that goes nowhere.
    pub async fn dashboard(mut req: Request) -> Result<Response> {
        let Some(settings) = req.state::<crate::support::settings::Settings>() else {
            return Ok(Response::see_other("/admin/menus"));
        };
        let store = settings.clone();

        let url = req.input("dashboard_url").unwrap_or_default().trim().to_string();
        // A path within this application, or nothing. Not an arbitrary URL: the
        // logo and the first entry in the rail both point here, and sending
        // everybody's home button to another site is not a menu edit.
        if !url.is_empty() && !url.starts_with('/') {
            page::flash(&req, "error", "That has to be a path inside this application, like `/reports`.");
            return Ok(Response::see_other("/admin/menus"));
        }

        store.put("menus.dashboard_url", &url).await?;

        if let Some(audit) = crate::support::audit::of(&req, "menus.updated") {
            audit
                .describe(match url.is_empty() {
                    true => "Reset the Dashboard entry to the built-in page".to_string(),
                    false => format!("Pointed the Dashboard entry at {url}"),
                })
                .record()
                .await;
        }

        page::flash(&req, "success", match url.is_empty() {
            true => "The Dashboard entry points at the built-in page again.".to_string(),
            false => format!("The Dashboard entry now opens {url}."),
        });
        Ok(Response::see_other("/admin/menus"))
    }

    /// `GET /admin/menus` and `GET /admin/menus/{location}`
    pub async fn index(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let location = location_of(&req);
        let search = req.query("q").unwrap_or_default().trim().to_lowercase();

        let items = MenuItem::get(&db, MenuItem::in_location(&location)).await?;
        let total = items.len() as i64;
        let active = items.iter().filter(|item| item.is_active).count() as i64;
        // Items that *have* children, not items that have no parent. Counting
        // the top level and calling it "parents" makes a flat menu report that
        // every item is a parent, which is the opposite of what the number is
        // for.
        let parents = items
            .iter()
            .filter(|item| items.iter().any(|child| child.parent_id == Some(item.id)))
            .count() as i64;

        let nodes = menu_item::tree(items);
        let depth = menu_item::depth_of(&nodes) as i64;

        let rows: Vec<Json> = menu_item::flatten(&nodes)
            .iter()
            .filter(|node| {
                search.is_empty()
                    || node.item.label.to_lowercase().contains(&search)
                    || node.item.route.as_deref().unwrap_or_default().to_lowercase().contains(&search)
            })
            .map(|node| {
                Json::object([
                    ("id", Json::from(node.item.id)),
                    ("label", Json::from(node.item.label.as_str())),
                    ("route", Json::from(node.item.route.clone().unwrap_or_default())),
                    ("has_route", Json::from(node.item.route.is_some())),
                    ("href", Json::from(node.item.href())),
                    ("icon", Json::from(icon(node.item.icon.as_deref()))),
                    ("has_icon", Json::from(!icon(node.item.icon.as_deref()).is_empty())),
                    ("is_active", Json::from(node.item.is_active)),
                    ("is_parent", Json::from(!node.children.is_empty())),
                    ("depth", Json::from(node.depth as i64)),
                    // The indent is a class rather than an inline width: this
                    // page is served under a policy with no `unsafe-inline`,
                    // and a `style="padding-left:..."` would be dropped in
                    // silence.
                    ("indent", Json::from(indent(node.depth))),
                ])
            })
            .collect();

        let cards = Json::Array(vec![
            stats::card("Total Items", total, stats::BRAND, stats::ICON_LIST),
            stats::card("Active Items", active, stats::GOOD, stats::ICON_CHECK),
            stats::card("Parent Items", parents, stats::PEOPLE, stats::ICON_FOLDER),
            stats::card("Max Depth", depth, stats::TIMED, stats::ICON_LAYERS),
        ]);

        let mut context = page::shell(&req, "menus").await;
        context = with_current_user(context, &req, &db).await?;
        context = context
            .with("items", Json::Array(rows))
            .with("stats", stats::formatted(&req, cards).await)
            .with("location", Json::from(location.as_str()))
            .with("locations", Json::Array(location_options(&location)))
            .with("icons", Json::Array(icon_options()))
            .with("q", Json::from(req.query("q").unwrap_or_default().to_string()))
            .with("empty", Json::from(total == 0))
            .with("can_manage", Json::from(req.can("menus.manage").await?))
            // The stored value, blank when it has never been set — so the
            // field shows what was chosen rather than the default dressed up
            // as a choice somebody made.
            .with(
                "dashboard_setting",
                Json::from(match req.state::<crate::support::settings::Settings>() {
                    Some(settings) => settings.get("menus.dashboard_url").await,
                    None => String::new(),
                }),
            );
        req.view("admin/menus/index", &context)
    }

    /// `GET /admin/menus/{location}/create`
    pub async fn create(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let location = location_of(&req);
        let items = MenuItem::get(&db, MenuItem::in_location(&location)).await?;

        let mut context = page::shell(&req, "menus").await;
        context = with_current_user(context, &req, &db).await?;
        context = Self::form_context(context, &location, &items, None);
        req.view("admin/menus/form", &context)
    }

    /// `GET /admin/menus/item/{id}/edit`
    pub async fn edit(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let id = req.param("id").and_then(|id| id.parse::<i64>().ok()).unwrap_or_default();
        let Some(item) = MenuItem::find(&db, id).await? else { return Ok(Response::not_found()) };

        let items = MenuItem::get(&db, MenuItem::in_location(&item.location)).await?;
        let mut context = page::shell(&req, "menus").await;
        context = with_current_user(context, &req, &db).await?;
        context = Self::form_context(context, &item.location.clone(), &items, Some(&item));
        req.view("admin/menus/form", &context)
    }

    /// `POST /admin/menus/{location}` — create.
    pub async fn store(mut req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let location = location_of(&req);

        let mut item = MenuItem { location: location.clone(), ..Default::default() };
        if let Some(response) = Self::fill(&mut req, &db, &mut item, None).await? {
            return Ok(response);
        }

        // Appended rather than inserted: a new item at the top would move
        // everything a person had already arranged.
        item.sort_order = MenuItem::in_location(&location).count(&db).await? + 1;
        item.insert(&db).await?;

        if let Some(audit) = crate::support::audit::of(&req, "menus.created") {
            audit
                .on("MenuItem", item.id)
                .describe(format!("Added \"{}\" to the {location} menu", item.label))
                .record()
                .await;
        }
        page::flash(&req, "success", format!("\"{}\" has been added.", item.label));
        warn_if_unknown(&req, &item).await;
        Ok(Response::see_other(format!("/admin/menus/{location}")))
    }

    /// `POST /admin/menus/item/{id}` — update.
    pub async fn update(mut req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let id = req.param("id").and_then(|id| id.parse::<i64>().ok()).unwrap_or_default();
        let Some(mut item) = MenuItem::find(&db, id).await? else { return Ok(Response::not_found()) };

        let before = item.label.clone();
        if let Some(response) = Self::fill(&mut req, &db, &mut item, Some(id)).await? {
            return Ok(response);
        }
        item.update(&db).await?;

        if let Some(audit) = crate::support::audit::of(&req, "menus.updated") {
            audit
                .on("MenuItem", id)
                .describe(format!("Edited the menu item \"{before}\""))
                .changed(Json::from(before.as_str()), Json::from(item.label.as_str()))
                .record()
                .await;
        }
        page::flash(&req, "success", format!("\"{}\" has been saved.", item.label));
        warn_if_unknown(&req, &item).await;
        Ok(Response::see_other(format!("/admin/menus/{}", item.location)))
    }

    /// `POST /admin/menus/item/{id}/toggle` — the switch on each row.
    pub async fn toggle(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let id = req.param("id").and_then(|id| id.parse::<i64>().ok()).unwrap_or_default();
        let Some(mut item) = MenuItem::find(&db, id).await? else { return Ok(Response::not_found()) };

        item.is_active = !item.is_active;
        item.update(&db).await?;

        if let Some(audit) = crate::support::audit::of(&req, "menus.updated") {
            let state = if item.is_active { "Enabled" } else { "Disabled" };
            audit.on("MenuItem", id).describe(format!("{state} the menu item \"{}\"", item.label)).record().await;
        }
        Ok(Response::see_other(format!("/admin/menus/{}", item.location)))
    }

    /// `POST /admin/menus/item/{id}/delete`
    ///
    /// The children are promoted to this item's parent rather than deleted
    /// with it. Removing one entry from a menu should not silently take a
    /// whole branch, and a person who wanted the branch gone can say so.
    pub async fn destroy(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let id = req.param("id").and_then(|id| id.parse::<i64>().ok()).unwrap_or_default();
        let Some(item) = MenuItem::find(&db, id).await? else { return Ok(Response::not_found()) };

        let promoted = db
            .table("menu_items")
            .filter("parent_id", id)
            .update(&db, &[("parent_id", item.parent_id.map_or(rustlavel::db::Value::Null, rustlavel::db::Value::from))])
            .await?;
        MenuItem::query().filter("id", id).delete(&db).await?;

        if let Some(audit) = crate::support::audit::of(&req, "menus.deleted") {
            audit
                .on("MenuItem", id)
                .describe(format!("Removed \"{}\" from the {} menu", item.label, item.location))
                .with("promoted_children", Json::from(promoted as i64))
                .record()
                .await;
        }
        page::flash(&req, "success", format!("\"{}\" has been removed.", item.label));
        Ok(Response::see_other(format!("/admin/menus/{}", item.location)))
    }

    /// `POST /admin/menus/{location}/clear-cache`
    ///
    /// The menu is read fresh on every request, so there is no menu cache to
    /// clear — what this drops is the *settings* cache, which is the one that
    /// holds a value across processes and the one people mean when a change
    /// they made is not showing.
    pub async fn clear_cache(req: Request) -> Result<Response> {
        let location = location_of(&req);
        if let Some(settings) = req.state::<crate::support::settings::Settings>() {
            settings.forget();
        }
        if let Some(audit) = crate::support::audit::of(&req, "cache.cleared") {
            audit.describe("Cleared the settings cache").record().await;
        }
        page::flash(&req, "success", "The cache has been cleared.");
        Ok(Response::see_other(format!("/admin/menus/{location}")))
    }

    /// `POST /admin/menus/{location}/reorder` — the drag-and-drop result.
    ///
    /// The whole order arrives at once, as `order=3,7,1,...`, because a drag
    /// that moves one row changes the position of every row after it, and
    /// sending one number back would be sending a position that is already
    /// stale.
    pub async fn reorder(mut req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let location = location_of(&req);
        let order = req.input("order").unwrap_or_default();

        // Scoped to this location: an id from another menu, submitted by hand,
        // must not be reordered into it.
        let mine: std::collections::BTreeSet<i64> =
            MenuItem::get(&db, MenuItem::in_location(&location)).await?.iter().map(|i| i.id).collect();

        let mut position = 0i64;
        for id in order.split(',').filter_map(|id| id.trim().parse::<i64>().ok()) {
            if !mine.contains(&id) {
                continue;
            }
            position += 1;
            db.table("menu_items")
                .filter("id", id)
                .update(&db, &[("sort_order", position.into())])
                .await?;
        }

        if let Some(audit) = crate::support::audit::of(&req, "menus.updated") {
            audit
                .describe(format!("Reordered the {location} menu"))
                .with("items", Json::from(position))
                .record()
                .await;
        }
        Ok(Response::see_other(format!("/admin/menus/{location}")))
    }

    /// Read the form into an item. Returns a rendered form when it does not
    /// validate, so the caller can hand it straight back.
    async fn fill(
        req: &mut Request,
        db: &Database,
        item: &mut MenuItem,
        editing: Option<i64>,
    ) -> Result<Option<Response>> {
        let label = req.input("label").unwrap_or_default().trim().to_string();
        let route = req.input("route").unwrap_or_default().trim().to_string();
        let url = req.input("url").unwrap_or_default().trim().to_string();
        let icon_name = req.input("icon").unwrap_or_default();
        let permission = req.input("permission").unwrap_or_default().trim().to_string();
        let parent = req.input("parent_id").and_then(|p| p.parse::<i64>().ok());

        let mut errors = page::check(&[("label", &label)], &[("label", "required|max:80")]);

        if route.is_empty() && url.is_empty() {
            errors.add("route", "Give the item somewhere to go: a route, or a URL.");
        }
        // An icon name that is not in the list would render as nothing, which
        // looks like a bug rather than like a choice.
        if !icon_name.is_empty() && icon(Some(&icon_name)).is_empty() {
            errors.add("icon", "That is not one of the icons on this page.");
        }
        // Its own parent, or its own descendant: either makes a loop that the
        // tree builder would then have to survive rather than never see.
        if let (Some(parent_id), Some(id)) = (parent, editing)
            && (parent_id == id || is_descendant(db, parent_id, id).await?)
        {
            errors.add("parent_id", "An item cannot sit inside itself.");
        }

        if !errors.is_empty() {
            let items = MenuItem::get(db, MenuItem::in_location(&item.location)).await?;
            let mut context = page::errors(page::shell(req, "menus").await, &errors);
            context = with_current_user(context, req, db).await?;
            context = Self::form_context(context, &item.location.clone(), &items, editing.map(|_| &*item));
            return Ok(Some(req.view("admin/menus/form", &context)?));
        }

        item.label = label;
        item.route = (!route.is_empty()).then_some(route);
        item.url = (!url.is_empty()).then_some(url);
        item.icon = (!icon_name.is_empty()).then_some(icon_name);
        item.permission = (!permission.is_empty()).then_some(permission);
        item.parent_id = parent;
        item.is_active = req.input("is_active").is_some();
        item.target = req.input("target").filter(|t| t == "_blank");
        Ok(None)
    }

    fn form_context(context: ViewContext, location: &str, items: &[MenuItem], item: Option<&MenuItem>) -> ViewContext {
        let current = item.map(|i| i.id);
        let parents: Vec<Json> = items
            .iter()
            .filter(|candidate| Some(candidate.id) != current)
            .map(|candidate| {
                Json::object([
                    ("id", Json::from(candidate.id)),
                    ("label", Json::from(candidate.label.as_str())),
                    ("selected", Json::from(item.and_then(|i| i.parent_id) == Some(candidate.id))),
                ])
            })
            .collect();

        let chosen = item.and_then(|i| i.icon.clone()).unwrap_or_default();
        let icons: Vec<Json> = ICONS
            .iter()
            .map(|(name, svg)| {
                Json::object([
                    ("name", Json::from(*name)),
                    ("svg", Json::from(*svg)),
                    ("selected", Json::from(chosen == *name)),
                ])
            })
            .collect();

        context
            .with("location", Json::from(location))
            .with("editing", Json::from(item.is_some()))
            .with("action", Json::from(match item {
                Some(existing) => format!("/admin/menus/item/{}", existing.id),
                None => format!("/admin/menus/{location}"),
            }))
            .with("heading", Json::from(match item {
                Some(_) => "Edit menu item",
                None => "Add menu item",
            }))
            .with("label", Json::from(item.map(|i| i.label.clone()).unwrap_or_default()))
            .with("route", Json::from(item.and_then(|i| i.route.clone()).unwrap_or_default()))
            .with("url", Json::from(item.and_then(|i| i.url.clone()).unwrap_or_default()))
            .with("permission", Json::from(item.and_then(|i| i.permission.clone()).unwrap_or_default()))
            .with("is_active", Json::from(item.is_none_or(|i| i.is_active)))
            .with("new_tab", Json::from(item.and_then(|i| i.target.clone()).as_deref() == Some("_blank")))
            .with("has_icon", Json::from(!chosen.is_empty()))
            .with("parents", Json::Array(parents))
            .with("icons", Json::Array(icons))
    }
}

/// Whether `candidate` sits under `ancestor`. Walks up from the candidate,
/// which is the shorter way and cannot loop for longer than the tree is deep.
async fn is_descendant(db: &Database, candidate: i64, ancestor: i64) -> Result<bool> {
    let mut at = Some(candidate);
    for _ in 0..16 {
        let Some(id) = at else { return Ok(false) };
        if id == ancestor {
            return Ok(true);
        }
        at = MenuItem::find(db, id).await?.and_then(|item| item.parent_id);
    }
    Ok(true)
}

/// The location from the path, defaulting to the sidebar and refusing one that
/// no template draws.
fn location_of(req: &Request) -> String {
    let asked = req.param("location").unwrap_or("sidebar");
    LOCATIONS
        .iter()
        .find(|(key, _)| *key == asked)
        .map(|(key, _)| (*key).to_string())
        .unwrap_or_else(|| "sidebar".to_string())
}

fn location_options(current: &str) -> Vec<Json> {
    LOCATIONS
        .iter()
        .map(|(key, label)| {
            Json::object([
                ("key", Json::from(*key)),
                ("label", Json::from(*label)),
                ("current", Json::from(*key == current)),
            ])
        })
        .collect()
}

fn icon_options() -> Vec<Json> {
    ICONS
        .iter()
        .map(|(name, svg)| Json::object([("name", Json::from(*name)), ("svg", Json::from(*svg))]))
        .collect()
}

/// The left padding for a row at this depth, as a class name.
fn indent(depth: usize) -> &'static str {
    match depth {
        0 => "pl-0",
        1 => "pl-6",
        2 => "pl-12",
        3 => "pl-16",
        4 => "pl-20",
        _ => "pl-24",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_icon_name_renders_nothing_rather_than_markup() {
        assert!(icon(Some("home")).starts_with("<svg"));
        assert_eq!(icon(Some("<script>alert(1)</script>")), "");
        assert_eq!(icon(Some("nope")), "");
        assert_eq!(icon(None), "");
    }

    #[test]
    fn the_indent_stops_growing_rather_than_running_off_the_page() {
        assert_eq!(indent(0), "pl-0");
        assert_eq!(indent(2), "pl-12");
        assert_eq!(indent(99), "pl-24");
    }
}
