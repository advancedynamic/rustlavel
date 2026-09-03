//! The audit trail, as a page.
//!
//! The reading half only. Entries are written by whatever did the thing —
//! `crate::support::audit::of(&req, "users.deleted")…save()` at the point of the deletion — because
//! a trail assembled here would be a trail that only knows what this file
//! thought to ask about.

use rustlavel::prelude::*;
use rustlavel::audit::{Filter, Trail};

use crate::models::user::User;
use crate::support::{audit, page, pdf, stats, tokens};

use super::users_controller::with_current_user;

/// How many rows a page shows, matching the pager at the bottom.
const PER_PAGE: i64 = 50;

/// The cap on an export. A trail is the biggest table in the application, and
/// "Export CSV" on a million rows is a request that never finishes.
const EXPORT_LIMIT: i64 = 20_000;

pub struct AuditController;

impl AuditController {
    /// `GET /admin/audit`
    pub async fn index(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let trail = trail(&req)?;
        let filter = filter_from(&req);
        let page_number = req.query("page").and_then(|p| p.parse::<i64>().ok()).unwrap_or(1);

        let listing = trail.page(&filter, page_number, PER_PAGE).await?;

        // The four counts along the top. Each is its own query rather than one
        // pass over the page, because they describe the whole trail and the
        // page is fifty rows of it.
        let today = tokens::now()[..10].to_string();
        let cards = Json::Array(vec![
            stats::card("Total Logs", trail.count(&Filter::default()).await?, stats::BRAND, stats::ICON_DOCUMENT),
            stats::card("Today's Activity", trail.count_since(&format!("{today} 00:00:00")).await?, stats::GOOD, stats::ICON_CHECK),
            stats::card("Unique Users", trail.distinct_users().await?, stats::PEOPLE, stats::ICON_GROUP),
            stats::card("Filtered", listing.total, stats::TIMED, stats::ICON_LIST),
        ]);

        let last = match trail.latest_at().await? {
            Some(at) => tokens::humanise(&at),
            None => "never".to_string(),
        };

        let rows: Vec<Json> = listing
            .entries
            .iter()
            .map(|entry| {
                Json::object([
                    ("id", Json::from(entry.id)),
                    ("created_at", Json::from(audit::stamp(&entry.created_at))),
                    ("ago", Json::from(tokens::humanise(&entry.created_at))),
                    ("user_name", Json::from(entry.user_name.clone().unwrap_or_else(|| "System".into()))),
                    ("initial", Json::from(initial(entry.user_name.as_deref()))),
                    ("event", Json::from(entry.event.as_str())),
                    ("event_tint", Json::from(tint(&entry.event))),
                    ("description", Json::from(entry.summary())),
                    ("subject", Json::from(subject(entry))),
                    ("ip_address", Json::from(entry.ip_address.clone().unwrap_or_else(|| "—".into()))),
                ])
            })
            .collect();

        let mut context = page::shell(&req, "audit").await;
        context = with_current_user(context, &req, &db).await?;
        context = context
            .with("logs", Json::Array(rows))
            .with("stats", cards)
            .with("last_activity", Json::from(last))
            .with("total", Json::from(listing.total))
            .with("first", Json::from(listing.first()))
            .with("last_row", Json::from(listing.last()))
            .with("page", Json::from(listing.page))
            .with("pages", Json::Array(pager(listing.page, listing.pages())))
            .with("previous", Json::from((listing.page - 1).max(1)))
            .with("next", Json::from((listing.page + 1).min(listing.pages())))
            .with("has_previous", Json::from(listing.page > 1))
            .with("has_next", Json::from(listing.page < listing.pages()))
            .with("empty", Json::from(listing.entries.is_empty()))
            .with("query", Json::from(query_string(&req, &[])))
            .with("users", Json::Array(Self::actors(&db, req.query("user")).await?))
            .with("events", Json::Array(named(trail.events().await?, req.query("event"))))
            .with("models", Json::Array(named(trail.model_types().await?, req.query("model"))))
            .with("has_filters", Json::from(!query_string(&req, &[]).is_empty()))
            .with("f_user", Json::from(req.query("user").unwrap_or_default().to_string()))
            .with("f_event", Json::from(req.query("event").unwrap_or_default().to_string()))
            .with("f_model", Json::from(req.query("model").unwrap_or_default().to_string()))
            .with("f_from", Json::from(req.query("from").unwrap_or_default().to_string()))
            .with("f_to", Json::from(req.query("to").unwrap_or_default().to_string()))
            .with("f_ip", Json::from(req.query("ip").unwrap_or_default().to_string()))
            .with("f_search", Json::from(req.query("q").unwrap_or_default().to_string()));
        req.view("admin/audit/index", &context)
    }

    /// `GET /admin/audit/{id}` — one entry in full, properties included.
    pub async fn show(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let trail = trail(&req)?;
        let id = req.param("id").and_then(|id| id.parse::<i64>().ok()).unwrap_or_default();

        let Some(entry) = trail.find(id).await? else {
            return Ok(Response::not_found());
        };

        let properties: Vec<Json> = match &entry.properties {
            Json::Object(map) => map
                .iter()
                .map(|(key, value)| {
                    Json::object([
                        ("key", Json::from(key.as_str())),
                        ("value", Json::from(readable(value))),
                    ])
                })
                .collect(),
            _ => Vec::new(),
        };

        let mut context = page::shell(&req, "audit").await;
        context = with_current_user(context, &req, &db).await?;
        context = context
            .with("id", Json::from(entry.id))
            .with("created_at", Json::from(audit::stamp(&entry.created_at)))
            .with("ago", Json::from(tokens::humanise(&entry.created_at)))
            .with("user_name", Json::from(entry.user_name.clone().unwrap_or_else(|| "System".into())))
            .with("user_id", entry.user_id.map_or(Json::Null, Json::from))
            .with("event", Json::from(entry.event.as_str()))
            .with("event_tint", Json::from(tint(&entry.event)))
            .with("description", Json::from(entry.summary()))
            .with("subject", Json::from(subject(&entry)))
            .with("ip_address", Json::from(entry.ip_address.clone().unwrap_or_else(|| "—".into())))
            .with("user_agent", Json::from(entry.user_agent.clone().unwrap_or_else(|| "—".into())))
            .with("properties", Json::Array(properties))
            .with("properties_empty", Json::from(entry.properties.is_null()));
        req.view("admin/audit/show", &context)
    }

    /// `GET /admin/audit/export.csv`
    pub async fn export_csv(req: Request) -> Result<Response> {
        let entries = trail(&req)?.all(&filter_from(&req), EXPORT_LIMIT).await?;

        let mut csv = String::from("Date,User,Event,Model,Description,IP Address\n");
        for entry in &entries {
            csv.push_str(&format!(
                "{},{},{},{},{},{}\n",
                cell(&audit::stamp(&entry.created_at)),
                cell(entry.user_name.as_deref().unwrap_or("System")),
                cell(&entry.event),
                cell(&subject(entry)),
                cell(&entry.summary()),
                cell(entry.ip_address.as_deref().unwrap_or("")),
            ));
        }

        Ok(Response::ok()
            .with_header("content-type", "text/csv; charset=utf-8")
            .with_header(
                "content-disposition",
                format!("attachment; filename=\"audit-{}.csv\"", &tokens::now()[..10]),
            )
            .with_body(csv.into_bytes()))
    }

    /// `GET /admin/audit/export.pdf`
    pub async fn export_pdf(req: Request) -> Result<Response> {
        let entries = trail(&req)?.all(&filter_from(&req), EXPORT_LIMIT).await?;
        let name = req.config().string("app.name", "Rustlavel");

        // Column x positions and widths, in points, against a landscape A4.
        let columns: [(f64, f64); 5] = [(32.0, 110.0), (146.0, 120.0), (270.0, 110.0), (384.0, 320.0), (708.0, 100.0)];
        let mut doc = pdf::Document::new(format!("{name} — audit log"));
        doc.heading(&format!("{name} — audit log"));
        doc.text(&format!("{} entries, exported {}", entries.len(), tokens::now()), 9.0);
        doc.rule();
        doc.row(&header(&columns), 9.0, true);
        doc.rule();

        for entry in &entries {
            let cells = [
                audit::stamp(&entry.created_at),
                entry.user_name.clone().unwrap_or_else(|| "System".into()),
                entry.event.clone(),
                entry.summary(),
                entry.ip_address.clone().unwrap_or_default(),
            ];
            let row: Vec<(f64, f64, String)> = columns
                .iter()
                .zip(cells)
                .map(|((x, width), text)| (*x, *width, text))
                .collect();
            doc.row(&row, 8.0, false);
        }

        Ok(Response::ok()
            .with_header("content-type", "application/pdf")
            .with_header(
                "content-disposition",
                format!("attachment; filename=\"audit-{}.pdf\"", &tokens::now()[..10]),
            )
            .with_body(doc.finish()))
    }

    /// Everybody who appears in the trail, for the User filter.
    async fn actors(db: &Database, chosen: Option<&str>) -> Result<Vec<Json>> {
        let users = User::get(db, User::query().order_by("name", rustlavel::db::Direction::Asc)).await?;
        Ok(users
            .iter()
            .map(|user| {
                Json::object([
                    ("id", Json::from(user.id)),
                    ("name", Json::from(user.name.as_str())),
                    ("selected", Json::from(chosen == Some(user.id.to_string().as_str()))),
                ])
            })
            .collect())
    }
}

fn header(columns: &[(f64, f64); 5]) -> Vec<(f64, f64, String)> {
    ["DATE/TIME", "USER", "EVENT", "DESCRIPTION", "IP ADDRESS"]
        .iter()
        .zip(columns)
        .map(|(label, (x, width))| (*x, *width, (*label).to_string()))
        .collect()
}

/// The trail, or a message that names the missing line in `main.rs`.
fn trail(req: &Request) -> Result<Trail> {
    req.state::<Trail>().cloned().ok_or_else(|| {
        Error::msg(
            "the audit trail is not registered. Add \
             `.plugin(rustlavel::audit::Audit::new(db.clone()))` in main.rs.",
        )
    })
}

/// The filter bar, read off the query string.
fn filter_from(req: &Request) -> Filter {
    Filter {
        user_id: req.query("user").and_then(|v| v.parse::<i64>().ok()),
        event: owned(req, "event"),
        model_type: owned(req, "model"),
        from: owned(req, "from"),
        to: owned(req, "to"),
        ip_address: owned(req, "ip"),
        search: owned(req, "q"),
    }
}

/// One query parameter, owned, and `None` rather than `Some("")` — an empty
/// box on the filter bar means "do not filter", not "match the empty string".
fn owned(req: &Request, key: &str) -> Option<String> {
    req.query(key).map(str::to_string).filter(|value| !value.is_empty())
}

/// Percent-encode a query-string value.
///
/// Written here rather than reached for: `rustlavel-http`'s `url` module is
/// not re-exported through the meta-crate, and a search box holding `&` or `#`
/// would otherwise end the parameter early and quietly widen the export.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The current filters as a query string, for the pager and the export links.
fn query_string(req: &Request, skip: &[&str]) -> String {
    let mut parts = Vec::new();
    for key in ["user", "event", "model", "from", "to", "ip", "q"] {
        if skip.contains(&key) {
            continue;
        }
        match req.query(key).filter(|v| !v.is_empty()) {
            Some(value) => parts.push(format!("{key}={}", encode(value))),
            None => continue,
        }
    }
    match parts.is_empty() {
        true => String::new(),
        false => format!("&{}", parts.join("&")),
    }
}

/// The page numbers worth drawing: a window around the current one.
fn pager(current: i64, total: i64) -> Vec<Json> {
    let start = (current - 3).max(1);
    let end = (start + 6).min(total);
    (start..=end)
        .map(|number| {
            Json::object([
                ("number", Json::from(number)),
                ("current", Json::from(number == current)),
            ])
        })
        .collect()
}

/// A dropdown's options, with the chosen one marked so the form comes back
/// showing what was filtered rather than resetting itself on every page.
fn named(values: Vec<String>, chosen: Option<&str>) -> Vec<Json> {
    values
        .into_iter()
        .map(|name| {
            Json::object([
                ("selected", Json::from(chosen == Some(name.as_str()))),
                ("name", Json::from(name)),
            ])
        })
        .collect()
}

fn initial(name: Option<&str>) -> String {
    name.and_then(|n| n.chars().next())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "S".to_string())
}

fn subject(entry: &rustlavel::audit::Entry) -> String {
    match (&entry.model_type, &entry.model_id) {
        (Some(kind), Some(id)) => format!("{kind} #{id}"),
        (Some(kind), None) => kind.clone(),
        _ => String::new(),
    }
}

/// A badge colour per event family, so a page of entries reads at a glance.
///
/// By prefix rather than by an exhaustive list: an application that records
/// `invoices.deleted` gets the red badge without registering anything.
fn tint(event: &str) -> &'static str {
    let verb = event.rsplit('.').next().unwrap_or(event);
    match verb {
        "deleted" | "destroyed" | "revoked" | "failed" => "badge-danger",
        "created" | "restored" | "granted" => "badge-success",
        "updated" | "changed" | "saved" => "badge-warning",
        "logged_in" | "login" => "badge-brand",
        _ => "badge-neutral",
    }
}

/// A JSON value as something to read in a table cell.
fn readable(value: &Json) -> String {
    match value {
        Json::String(text) => text.clone(),
        Json::Null => "—".to_string(),
        other => other.to_string(),
    }
}

/// One CSV cell, quoted when it has to be.
///
/// A description holds whatever the application wrote into it, and a comma in
/// there would otherwise shift every later column by one — which is how an
/// export that "works" turns out to have been wrong for a year.
fn cell(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A search for `a&b=c` must not end the parameter and start a new one.
    #[test]
    fn a_query_value_cannot_break_out_of_its_parameter() {
        assert_eq!(encode("plain"), "plain");
        assert_eq!(encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(encode("two words"), "two+words");
        assert_eq!(encode("a#b"), "a%23b");
    }

    #[test]
    fn a_cell_with_punctuation_in_it_is_quoted() {
        assert_eq!(cell("plain"), "plain");
        assert_eq!(cell("a,b"), "\"a,b\"");
        assert_eq!(cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(cell("two\nlines"), "\"two\nlines\"");
    }

    #[test]
    fn the_badge_follows_the_verb_not_a_list_of_events() {
        assert_eq!(tint("users.deleted"), "badge-danger");
        assert_eq!(tint("invoices.deleted"), "badge-danger");
        assert_eq!(tint("settings.updated"), "badge-warning");
        assert_eq!(tint("logged_in"), "badge-brand");
        assert_eq!(tint("something.else"), "badge-neutral");
    }

    #[test]
    fn the_pager_keeps_a_window_around_the_current_page() {
        let numbers: Vec<i64> = pager(1, 7)
            .iter()
            .filter_map(|p| p.get("number").and_then(Json::as_i64))
            .collect();
        assert_eq!(numbers, vec![1, 2, 3, 4, 5, 6, 7]);

        let numbers: Vec<i64> = pager(20, 40)
            .iter()
            .filter_map(|p| p.get("number").and_then(Json::as_i64))
            .collect();
        assert_eq!(numbers, vec![17, 18, 19, 20, 21, 22, 23]);
    }
}
