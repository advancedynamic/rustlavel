//! The development error page.
//!
//! When something throws, a developer should see what happened, where, and what
//! to do about it — not a blank 500. In production the same failure renders as
//! a plain page with no internals, and the detail goes to the logs instead.

use crate::request::Request;
use crate::response::Response;
use crate::status::Status;
use rustlavel_core::{Error, Json};
use std::sync::atomic::{AtomicBool, Ordering};

static DEBUG: AtomicBool = AtomicBool::new(false);

/// Enable the detailed page. Set once during boot from `app.debug`.
pub fn set_debug(enabled: bool) {
    DEBUG.store(enabled, Ordering::Relaxed);
}

pub fn debug_enabled() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

/// Everything the page knows about a failure.
pub struct Diagnostic {
    pub title: String,
    pub message: String,
    pub hint: Option<String>,
    pub location: Option<(String, u32)>,
    pub status: Status,
}

impl Diagnostic {
    pub fn from_error(error: &Error) -> Self {
        Diagnostic {
            title: error.title().to_string(),
            message: error.to_string(),
            hint: error.hint(),
            location: None,
            status: Status(error.status()),
        }
    }

    pub fn from_panic(message: String, location: Option<(String, u32)>) -> Self {
        Diagnostic {
            title: "Unhandled Panic".to_string(),
            message,
            hint: Some(
                "A handler panicked. Prefer returning `Result` so the failure is part of the \
                 type, rather than unwrapping a value that can be absent."
                    .to_string(),
            ),
            location,
            status: Status::INTERNAL_ERROR,
        }
    }
}

/// Render an error into a response, honouring debug mode and content negotiation.
pub fn response_for(error: &Error) -> Response {
    render(&Diagnostic::from_error(error), None)
}

/// Render a diagnostic, using the request (when available) for the detail
/// panels and to decide between HTML and JSON.
pub fn render(diagnostic: &Diagnostic, request: Option<&Request>) -> Response {
    let wants_json = request.is_some_and(Request::wants_json);

    if !debug_enabled() {
        rustlavel_core::log::log(
            rustlavel_core::log::Level::Error,
            format!("{}: {}", diagnostic.title, diagnostic.message),
        );
        return if wants_json {
            Response::new(diagnostic.status).with_json(Json::object([(
                "message",
                Json::from("Server Error"),
            )]))
        } else {
            Response::new(diagnostic.status).with_html(PRODUCTION_PAGE)
        };
    }

    if wants_json {
        let mut fields = vec![
            ("message", Json::from(diagnostic.message.as_str())),
            ("exception", Json::from(diagnostic.title.as_str())),
        ];
        if let Some(hint) = &diagnostic.hint {
            fields.push(("hint", Json::from(hint.as_str())));
        }
        if let Some((file, line)) = &diagnostic.location {
            fields.push(("file", Json::from(file.as_str())));
            fields.push(("line", Json::from(*line)));
        }
        return Response::new(diagnostic.status).with_json(Json::object(fields));
    }

    Response::new(diagnostic.status).with_html(html(diagnostic, request))
}

fn html(diagnostic: &Diagnostic, request: Option<&Request>) -> String {
    let mut sections = String::new();

    if let Some(hint) = &diagnostic.hint {
        sections.push_str(&format!(
            r#"<div class="hint"><span class="hint-label">Suggestion</span><p>{}</p></div>"#,
            escape(hint)
        ));
    }

    if let Some((file, line)) = &diagnostic.location {
        sections.push_str(&format!(
            r#"<div class="location">{}<span class="line">:{}</span></div>"#,
            escape(file),
            line
        ));
        if let Some(snippet) = source_snippet(file, *line) {
            sections.push_str(&snippet);
        }
    }

    if let Some(request) = request {
        sections.push_str(&request_panel(request));
    }

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} — Rustlavel</title>
<style>{CSS}</style>
</head>
<body>
<main>
  <header>
    <div class="badge">{status}</div>
    <h1>{title}</h1>
    <p class="message">{message}</p>
  </header>
  {sections}
  <footer>Rustlavel debug page — hidden automatically when <code>APP_DEBUG=false</code>.</footer>
</main>
</body>
</html>"#,
        title = escape(&diagnostic.title),
        status = diagnostic.status.code(),
        message = escape(&diagnostic.message),
    )
}

/// Show the failing line with a few lines of context around it.
fn source_snippet(file: &str, line: u32) -> Option<String> {
    let source = std::fs::read_to_string(file).ok()?;
    let lines: Vec<&str> = source.lines().collect();
    let target = line as usize;
    let start = target.saturating_sub(4).max(1);
    let end = (target + 3).min(lines.len());

    let mut rows = String::new();
    for number in start..=end {
        let content = lines.get(number - 1).copied().unwrap_or_default();
        let class = if number == target { "row highlight" } else { "row" };
        rows.push_str(&format!(
            r#"<div class="{class}"><span class="num">{number}</span><code>{}</code></div>"#,
            escape(content)
        ));
    }
    Some(format!(r#"<div class="snippet">{rows}</div>"#))
}

fn request_panel(request: &Request) -> String {
    let mut rows = format!(
        r#"<tr><th>Method</th><td>{}</td></tr><tr><th>Path</th><td>{}</td></tr>"#,
        request.method(),
        escape(request.target())
    );
    if let Some(route) = request.route() {
        rows.push_str(&format!(r#"<tr><th>Route</th><td>{}</td></tr>"#, escape(route)));
    }
    for (name, value) in request.headers().iter() {
        // Never echo credentials back onto a page someone might screenshot.
        let shown = if is_sensitive(name) { "[hidden]" } else { value };
        rows.push_str(&format!(
            r#"<tr><th>{}</th><td>{}</td></tr>"#,
            escape(name),
            escape(shown)
        ));
    }
    format!(r#"<div class="panel"><h2>Request</h2><table>{rows}</table></div>"#)
}

fn is_sensitive(header: &str) -> bool {
    matches!(header, "authorization" | "cookie" | "proxy-authorization" | "x-api-key")
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

const PRODUCTION_PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Server Error</title>
<style>body{font:16px/1.6 system-ui,sans-serif;display:grid;place-content:center;height:100vh;margin:0;color:#334}</style>
</head><body><div><h1>500</h1><p>Something went wrong. Please try again.</p></div></body></html>"#;

const CSS: &str = r#"
:root { color-scheme: light dark; --bg:#faf9f7; --fg:#1c1b1a; --muted:#6b6864; --line:#e5e2dd;
        --accent:#b4483c; --panel:#fff; --code:#f4f2ef; }
@media (prefers-color-scheme: dark) {
  :root { --bg:#181716; --fg:#eceae7; --muted:#9a958e; --line:#2e2c29; --accent:#e0796c;
          --panel:#201f1d; --code:#252321; }
}
* { box-sizing: border-box; }
body { margin:0; background:var(--bg); color:var(--fg);
       font:15px/1.6 ui-sans-serif,-apple-system,'Segoe UI',sans-serif; }
main { max-width: 900px; margin: 0 auto; padding: 48px 24px 80px; }
header { border-left: 3px solid var(--accent); padding-left: 20px; margin-bottom: 32px; }
.badge { display:inline-block; background:var(--accent); color:#fff; font-size:12px; font-weight:600;
         letter-spacing:.06em; padding:3px 9px; border-radius:4px; }
h1 { font-size: 26px; margin: 12px 0 8px; font-weight: 650; }
.message { font-size: 17px; margin:0; color:var(--fg); }
h2 { font-size: 13px; text-transform: uppercase; letter-spacing:.08em; color: var(--muted);
     margin: 0 0 12px; font-weight: 600; }
.hint { background:var(--panel); border:1px solid var(--line); border-radius:8px;
        padding:16px 20px; margin-bottom:24px; }
.hint-label { display:block; font-size:12px; text-transform:uppercase; letter-spacing:.08em;
              color:var(--accent); font-weight:600; margin-bottom:4px; }
.hint p { margin:0; }
.location { font-family: ui-monospace,SFMono-Regular,Menlo,monospace; font-size:13px;
            color:var(--muted); margin-bottom:8px; }
.location .line { color: var(--accent); }
.snippet { background:var(--code); border:1px solid var(--line); border-radius:8px;
           overflow-x:auto; margin-bottom:24px; padding:12px 0; }
.row { display:flex; gap:16px; padding:1px 20px; font-family:ui-monospace,SFMono-Regular,Menlo,monospace;
       font-size:13px; white-space:pre; }
.row.highlight { background:color-mix(in srgb, var(--accent) 14%, transparent); }
.num { color:var(--muted); min-width:3ch; text-align:right; user-select:none; }
.panel { background:var(--panel); border:1px solid var(--line); border-radius:8px; padding:20px; }
table { width:100%; border-collapse:collapse; font-size:13px; }
th { text-align:left; color:var(--muted); font-weight:500; width:180px; vertical-align:top;
     padding:4px 12px 4px 0; word-break:break-word; }
td { font-family:ui-monospace,SFMono-Regular,Menlo,monospace; padding:4px 0; word-break:break-all; }
footer { margin-top:40px; font-size:12px; color:var(--muted); }
code { font-family:ui-monospace,SFMono-Regular,Menlo,monospace; }
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;

    /// Debug mode is process-wide, so these tests must not run concurrently.
    static DEBUG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        DEBUG_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn production_hides_the_details() {
        let _guard = exclusive();
        set_debug(false);
        let response = response_for(&Error::msg("connection string leaked"));

        assert_eq!(response.status, Status::INTERNAL_ERROR);
        assert!(!response.body_string().contains("connection string leaked"));
    }

    #[test]
    fn debug_shows_the_message_and_hint() {
        let _guard = exclusive();
        set_debug(true);
        let error = Error::Config { file: ".env".into(), line: 3, message: "bad line".into() };
        let response = response_for(&error);
        let body = response.body_string();

        assert!(body.contains("bad line"));
        assert!(body.contains("Suggestion"));
        set_debug(false);
    }

    #[test]
    fn json_clients_get_json_errors() {
        let _guard = exclusive();
        set_debug(true);
        let request = Request::new(Method::Get, "/api/users").with_header("accept", "application/json");
        let response = render(&Diagnostic::from_error(&Error::msg("nope")), Some(&request));

        assert_eq!(response.headers.content_type(), Some("application/json"));
        assert!(response.body_string().contains("\"message\":\"nope\""));
        set_debug(false);
    }

    #[test]
    fn credentials_are_not_echoed_onto_the_page() {
        let _guard = exclusive();
        set_debug(true);
        let request = Request::new(Method::Get, "/")
            .with_header("authorization", "Bearer super-secret")
            .with_header("x-trace", "abc");
        let response = render(&Diagnostic::from_error(&Error::msg("boom")), Some(&request));
        let body = response.body_string();

        assert!(!body.contains("super-secret"));
        assert!(body.contains("[hidden]"));
        assert!(body.contains("abc"));
        set_debug(false);
    }

    #[test]
    fn markup_in_a_message_is_escaped() {
        let _guard = exclusive();
        set_debug(true);
        let response = response_for(&Error::msg("<script>alert(1)</script>"));

        assert!(!response.body_string().contains("<script>"));
        assert!(response.body_string().contains("&lt;script&gt;"));
        set_debug(false);
    }
}
