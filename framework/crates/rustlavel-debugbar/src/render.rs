//! Turning what was collected into the bar itself.
//!
//! One string of HTML with its own styles and script inlined. No asset to
//! serve, no build step, nothing to keep in sync with a version number — a
//! debug bar that needs its own pipeline is one that stops working the first
//! time somebody upgrades something.

use crate::collector::Collected;
use std::time::Duration;

/// Build the bar for one request.
pub fn bar(collected: &Collected, route: &str, status: u16) -> String {
    let elapsed = collected.started.map(|started| started.elapsed()).unwrap_or_default();
    let queries = collected.of("db.query");
    let repeated = collected.repeated_queries();

    let mut html = String::with_capacity(4096);
    html.push_str(r#"<div id="rl-bar" data-rl-bar><style>"#);
    html.push_str(STYLE);
    html.push_str("</style>");

    html.push_str(r#"<div class="rl-summary" onclick="rlToggle()">"#);
    push_chip(&mut html, "route", &format!("{status} {route}"), false);
    push_chip(&mut html, "time", &millis(elapsed), false);
    push_chip(
        &mut html,
        "queries",
        &format!("{} · {}", queries.len(), millis(collected.total("db.query"))),
        // The count turns red when the same query repeats, because that is the
        // one number a person should act on rather than merely read.
        !repeated.is_empty(),
    );

    let hits = collected.of("cache.hit").len();
    let misses = collected.of("cache.miss").len();
    if hits + misses > 0 {
        push_chip(&mut html, "cache", &format!("{hits} hit · {misses} miss"), false);
    }
    html.push_str(r#"<span class="rl-grow"></span><span class="rl-caret">▲</span></div>"#);

    html.push_str(r#"<div class="rl-panel" hidden>"#);

    if !repeated.is_empty() {
        html.push_str(r#"<div class="rl-warn"><strong>Repeated queries.</strong> "#);
        html.push_str("The same statement ran more than once, which is usually a relation \
                       loaded inside a loop rather than alongside the set.</div>");
        html.push_str("<table>");
        for (sql, count) in repeated.iter().take(10) {
            html.push_str(&format!(
                "<tr><td class=\"rl-count\">×{count}</td><td><code>{}</code></td></tr>",
                escape(sql)
            ));
        }
        html.push_str("</table>");
    }

    for (kind, heading) in SECTIONS {
        let entries = collected.of(kind);
        if entries.is_empty() {
            continue;
        }
        html.push_str(&format!("<h4>{heading} <span>{}</span></h4><table>", entries.len()));
        for entry in entries.iter().take(100) {
            html.push_str("<tr><td class=\"rl-dur\">");
            html.push_str(&entry.duration.map(millis).unwrap_or_else(|| "—".into()));
            html.push_str("</td><td><code>");
            html.push_str(&escape(&entry.label));
            html.push_str("</code></td></tr>");
        }
        html.push_str("</table>");
    }

    html.push_str(
        r#"<p class="rl-note">Work moved to another task with <code>tokio::spawn</code> is
        not counted here — it is no longer on the request's task, which is what the bar
        follows.</p>"#,
    );
    html.push_str("</div>");
    html.push_str(r#"<script>function rlToggle(){var p=document.querySelector('#rl-bar .rl-panel');p.hidden=!p.hidden;document.querySelector('#rl-bar .rl-caret').textContent=p.hidden?'▲':'▼';}</script>"#);
    html.push_str("</div>");
    html
}

const SECTIONS: &[(&str, &str)] = &[
    ("db.query", "Queries"),
    ("cache.hit", "Cache hits"),
    ("cache.miss", "Cache misses"),
    ("http.client", "Outbound HTTP"),
    ("ai.call", "AI"),
    ("mcp.call", "MCP"),
    ("mail.sent", "Mail"),
    ("queue.pushed", "Jobs queued"),
];

fn push_chip(html: &mut String, label: &str, value: &str, alarming: bool) {
    let class = if alarming { "rl-chip rl-alarm" } else { "rl-chip" };
    html.push_str(&format!(
        r#"<span class="{class}"><b>{}</b>{}</span>"#,
        escape(label),
        escape(value)
    ));
}

fn millis(duration: Duration) -> String {
    let ms = duration.as_secs_f64() * 1000.0;
    if ms < 1.0 { format!("{:.2} ms", ms) } else { format!("{:.1} ms", ms) }
}

/// Escape for HTML text.
///
/// Everything here — SQL, cache keys, a route — can contain characters a user
/// put there. A debug bar that renders them raw is a cross-site scripting hole
/// in the one tool you are staring at while you work.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

const STYLE: &str = "#rl-bar{position:fixed;left:0;right:0;bottom:0;z-index:2147483000;\
font:12px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace;color:#e6e8ea;\
background:#14181d;border-top:1px solid #2a313a;box-shadow:0 -2px 12px rgba(0,0,0,.3)}\
#rl-bar .rl-summary{display:flex;align-items:center;gap:14px;padding:7px 12px;cursor:pointer}\
#rl-bar .rl-chip{white-space:nowrap}#rl-bar .rl-chip b{color:#7b8794;font-weight:400;margin-right:6px}\
#rl-bar .rl-alarm{color:#ff8a6a}#rl-bar .rl-grow{flex:1}#rl-bar .rl-caret{color:#7b8794}\
#rl-bar .rl-panel{max-height:45vh;overflow:auto;border-top:1px solid #2a313a;padding:10px 12px}\
#rl-bar h4{margin:14px 0 6px;font-size:11px;text-transform:uppercase;letter-spacing:.06em;color:#7b8794}\
#rl-bar h4 span{color:#4a5561}#rl-bar table{width:100%;border-collapse:collapse}\
#rl-bar td{padding:3px 8px 3px 0;vertical-align:top;border-bottom:1px solid #1b2027}\
#rl-bar .rl-dur,#rl-bar .rl-count{white-space:nowrap;color:#7b8794;width:1%}\
#rl-bar code{color:#cfd6dd;word-break:break-all}\
#rl-bar .rl-warn{background:#2a1d18;border-left:3px solid #ff8a6a;padding:8px 10px;margin:4px 0 8px}\
#rl-bar .rl-note{color:#4a5561;margin:14px 0 0}";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::Timing;

    fn query(sql: &str) -> Timing {
        Timing {
            kind: "db.query".into(),
            label: sql.into(),
            duration: Some(Duration::from_millis(2)),
            fields: Vec::new(),
        }
    }

    #[test]
    fn shows_the_route_status_and_query_count() {
        let collected =
            Collected { timings: vec![query("select 1"), query("select 2")], started: None };
        let html = bar(&collected, "/users/{id}", 200);

        assert!(html.contains("200 /users/{id}"));
        assert!(html.contains("2 · "), "the query count belongs on the summary");
    }

    #[test]
    fn a_repeated_query_is_called_out_rather_than_just_counted() {
        let mut timings = vec![];
        for _ in 0..12 {
            timings.push(query("select * from users where id = ?"));
        }
        let html = bar(&Collected { timings, started: None }, "/posts", 200);

        assert!(html.contains("Repeated queries"));
        assert!(html.contains("×12"));
        assert!(html.contains("rl-alarm"), "the chip turns red so it is noticed");
    }

    #[test]
    fn sql_containing_html_cannot_close_the_page_or_run_a_script() {
        // A value a user typed reaches the bar through a query. Rendering it
        // raw would be a scripting hole in the tool you stare at all day.
        let hostile = "select * from t where name = '</script><img src=x onerror=alert(1)>'";
        let html = bar(&Collected { timings: vec![query(hostile)], started: None }, "/", 200);

        assert!(!html.contains("<img src=x"), "raw markup reached the page");
        assert!(!html.contains("</script><img"), "the script tag was not closed early");
        assert!(html.contains("&lt;img src=x"));
    }

    #[test]
    fn a_hostile_route_is_escaped_too() {
        let html = bar(&Collected::default(), "/<script>alert(1)</script>", 404);
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn an_empty_request_still_renders_a_usable_bar() {
        let html = bar(&Collected::default(), "/health", 204);

        assert!(html.contains("204 /health"));
        assert!(html.contains("rl-bar"));
        assert!(!html.contains("Repeated queries"));
    }

    #[test]
    fn the_limit_of_the_measurement_is_printed_rather_than_hidden() {
        // Work spawned off the request task is genuinely not counted, and a
        // number that quietly omits some of the work is worse than one that
        // says what it left out.
        let html = bar(&Collected::default(), "/", 200);
        assert!(html.contains("tokio::spawn"));
    }
}
