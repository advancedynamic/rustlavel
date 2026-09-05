//! Where the application opens.
//!
//! One setting, `menus.home`, read by everything that has to answer "where does
//! this person go now": `GET /`, the fallback after signing in, and the link
//! under the logo. Before this the three disagreed — the setting drove the logo
//! only, so an administrator could point "Dashboard opens" at `/reports` and
//! still land on `/dashboard` by typing the address or signing in.
//!
//! **It holds a route name, not a path.** `dashboard`, not `/dashboard`. A name
//! survives the route moving, and — because `url_for` returns nothing for a
//! name that was never registered — a typed-in name can be refused when it is
//! saved rather than discovered as a 404 on everybody's logo.
//!
//! A value that begins with `/` is taken as a literal path, for an application
//! that wants to point somewhere the router does not own and for anybody whose
//! setting predates this.

use rustlavel::prelude::*;

use crate::support::settings::Settings;

/// The route the application opens on when nothing else is asked for.
pub const DEFAULT: &str = "dashboard";

/// Where to send somebody who has asked for nothing in particular.
///
/// Falls back to `/dashboard` rather than to `/`, because `/` redirects here
/// and a mistake in the setting must not become a loop.
pub async fn path(req: &Request) -> String {
    let raw = match req.state::<Settings>() {
        Some(settings) => settings.get("menus.home").await,
        None => String::new(),
    };
    resolve(req, raw.trim())
}

/// Turn a setting value into a path.
fn resolve(req: &Request, raw: &str) -> String {
    if raw.starts_with('/') {
        return raw.to_string();
    }

    let name = if raw.is_empty() { DEFAULT } else { raw };
    req.state::<rustlavel::NamedRoutes>()
        .and_then(|routes| routes.url_for(name, &[]))
        // A name nobody registered lands on the built-in dashboard rather than
        // on nothing. The Menus screen refuses to save such a name, so this is
        // the case where a route was deleted after the fact.
        .unwrap_or_else(|| "/dashboard".to_string())
}

/// Whether this value can be saved: a path inside the application, or the name
/// of a route that exists.
pub fn is_valid(req: &Request, raw: &str) -> bool {
    let raw = raw.trim();
    if raw.is_empty() {
        return true;
    }
    if raw.starts_with('/') {
        // Not `//`, which the browser reads as another site.
        return !raw.starts_with("//");
    }
    req.state::<rustlavel::NamedRoutes>()
        .is_some_and(|routes| routes.url_for(raw, &[]).is_some())
}
