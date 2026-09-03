//! The variables every page needs, gathered in one place.
//!
//! The layout reads `app_name`, `theme`, `user_name` and the `can_*` flags on
//! every render. Building that by hand in each controller is how one page ends
//! up with a menu the others do not have.

use rustlavel::prelude::*;

use crate::models::user::User;

/// Start a context with everything the layouts need.
///
/// `nav` is the sidebar entry to mark current. Pass `""` for a page with no
/// entry of its own.
pub async fn shell(req: &Request, nav: &str) -> ViewContext {
    let config = req.config();
    let name = config.string("app.name", "Rustlavel");

    let mut context = ViewContext::new()
        .with("app_name", Json::from(name.as_str()))
        .with("app_initial", Json::from(name.chars().next().unwrap_or('R').to_string()))
        .with("nav", Json::from(nav))
        .with("csrf_field", Json::from(rustlavel::auth::csrf::field(req)));

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
