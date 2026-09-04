//! The little counted cards at the top of an administration page.
//!
//! One module rather than a copy on each page: the cards are the same object
//! everywhere, and three copies of the same six SVG paths is three places to
//! fix a wrong viewBox. What differs per page is which counts are worth
//! showing, and that stays in the controller that knows.

use rustlavel::prelude::*;

/// One card: an icon in a tinted square, a number, and what it counts.
///
/// The number is left raw here and formatted by [`formatted`], which the
/// controllers call once they have the request. Splitting it that way keeps
/// this function free of the settings store while still letting Settings →
/// Language reach every count on every administration page — which is the
/// difference between a format setting and a decoration.
pub fn card(label: &str, value: i64, tint: &str, icon: &str) -> Json {
    Json::object([
        ("label", Json::from(label)),
        ("value", Json::from(value)),
        ("shown", Json::from(value.to_string())),
        ("tint", Json::from(tint)),
        ("icon", Json::from(icon)),
    ])
}

/// The same cards with their numbers written the way Settings → Language asks.
pub async fn formatted(req: &Request, cards: Json) -> Json {
    let number = crate::support::format::number_format(req).await;
    let Json::Array(cards) = cards else { return cards };
    Json::Array(
        cards
            .into_iter()
            .map(|card| {
                let value = card.get("value").and_then(Json::as_i64).unwrap_or_default();
                let shown = crate::support::format::integer(value, &number);
                match card {
                    Json::Object(mut fields) => {
                        fields.insert("shown".to_string(), Json::from(shown));
                        Json::Object(fields)
                    }
                    other => other,
                }
            })
            .collect(),
    )
}

/// The tints, named by what they mean rather than by their colour, so a page
/// picking one is saying something about the number rather than about blue.
pub const BRAND: &str = "bg-brand-50 text-brand-600 dark:bg-brand-500/10 dark:text-brand-400";
pub const GOOD: &str = "bg-emerald-50 text-emerald-600 dark:bg-emerald-500/10 dark:text-emerald-400";
pub const BUSY: &str = "bg-orange-50 text-orange-600 dark:bg-orange-500/10 dark:text-orange-400";
pub const PEOPLE: &str = "bg-violet-50 text-violet-600 dark:bg-violet-500/10 dark:text-violet-400";
pub const KEYED: &str = "bg-indigo-50 text-indigo-600 dark:bg-indigo-500/10 dark:text-indigo-400";
pub const TIMED: &str = "bg-amber-50 text-amber-600 dark:bg-amber-500/10 dark:text-amber-400";
pub const QUIET: &str = "bg-ink-100 text-ink-600 dark:bg-ink-800 dark:text-ink-300";

pub const ICON_USERS: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M7 8a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm6 1a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5ZM1.6 15.5A5.6 5.6 0 0 1 7 10.5a5.6 5.6 0 0 1 5.4 5H1.6Zm12.05 0a6.9 6.9 0 0 0-1.6-3.86A4.2 4.2 0 0 1 18.4 15.5h-4.75Z"/></svg>"#;
pub const ICON_CHECK: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M10 18a8 8 0 1 0 0-16 8 8 0 0 0 0 16Zm3.86-9.72a.75.75 0 0 0-1.22-.86l-3.24 4.53-1.62-1.62a.75.75 0 0 0-1.06 1.06l2.25 2.25a.75.75 0 0 0 1.14-.1l3.75-5.25Z" clip-rule="evenodd"/></svg>"#;
pub const ICON_BOLT: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M11.3 1.05a.75.75 0 0 1 .7.98L10.4 7.5h3.85a.75.75 0 0 1 .58 1.22l-6.5 8a.75.75 0 0 1-1.32-.68L8.6 11.5H4.75a.75.75 0 0 1-.58-1.22l6.5-8a.75.75 0 0 1 .63-.23Z"/></svg>"#;
pub const ICON_GROUP: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M10 8a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm-6.5 9.5a6.5 6.5 0 0 1 13 0H3.5Z"/></svg>"#;
pub const ICON_KEY: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M13 2a5 5 0 0 0-4.9 6L2 14.1V18h3.9l1.3-1.3v-1.6h1.6l1.3-1.3v-1.6h1.6l.4-.4A5 5 0 1 0 13 2Zm1.5 4.5a1 1 0 1 1-2 0 1 1 0 0 1 2 0Z"/></svg>"#;
pub const ICON_CLOCK: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M10 18a8 8 0 1 0 0-16 8 8 0 0 0 0 16Zm.75-11.5a.75.75 0 0 0-1.5 0v4c0 .28.16.54.41.67l2.5 1.25a.75.75 0 1 0 .68-1.34l-2.09-1.04V6.5Z" clip-rule="evenodd"/></svg>"#;

pub const ICON_SHIELD: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M10 1.5 3.5 4v5.2c0 3.9 2.6 7.5 6.5 9.3 3.9-1.8 6.5-5.4 6.5-9.3V4L10 1.5Zm3.3 6.6a.75.75 0 0 0-1.16-.95l-2.86 3.5-1.2-1.2a.75.75 0 1 0-1.06 1.06l1.8 1.8a.75.75 0 0 0 1.1-.05l3.38-4.16Z" clip-rule="evenodd"/></svg>"#;
pub const ICON_LOCK: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M10 1.5A3.5 3.5 0 0 0 6.5 5v2H6a2 2 0 0 0-2 2v7a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2V9a2 2 0 0 0-2-2h-.5V5A3.5 3.5 0 0 0 10 1.5ZM12 7V5a2 2 0 1 0-4 0v2h4Z" clip-rule="evenodd"/></svg>"#;
pub const ICON_LIST: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M3 5.25A.75.75 0 0 1 3.75 4.5h12.5a.75.75 0 0 1 0 1.5H3.75A.75.75 0 0 1 3 5.25Zm0 4.75a.75.75 0 0 1 .75-.75h12.5a.75.75 0 0 1 0 1.5H3.75A.75.75 0 0 1 3 10Zm0 4.75a.75.75 0 0 1 .75-.75h12.5a.75.75 0 0 1 0 1.5H3.75a.75.75 0 0 1-.75-.75Z"/></svg>"#;
pub const ICON_FOLDER: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M2 5.5A1.5 1.5 0 0 1 3.5 4h3.38a1.5 1.5 0 0 1 1.06.44l1.12 1.12H16.5A1.5 1.5 0 0 1 18 7v7.5a1.5 1.5 0 0 1-1.5 1.5h-13A1.5 1.5 0 0 1 2 14.5v-9Z"/></svg>"#;
pub const ICON_LAYERS: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path d="M10 2 1.75 6 10 10l8.25-4L10 2Zm7.1 7.1L10 12.5 2.9 9.1 1.75 9.7 10 13.7l8.25-4-1.15-.6Zm0 3.6L10 16.1 2.9 12.7l-1.15.6L10 17.3l8.25-4-1.15-.6Z"/></svg>"#;
pub const ICON_DOCUMENT: &str = r#"<svg class="h-5 w-5" viewBox="0 0 20 20" fill="currentColor" aria-hidden="true"><path fill-rule="evenodd" d="M4.5 3.5A1.5 1.5 0 0 1 6 2h4.6L15.5 6.9V16.5A1.5 1.5 0 0 1 14 18H6a1.5 1.5 0 0 1-1.5-1.5v-13ZM10.5 3.6V7h3.4l-3.4-3.4Z" clip-rule="evenodd"/></svg>"#;
