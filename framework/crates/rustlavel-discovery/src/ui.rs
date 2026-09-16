//! What the registry looks like, for a person.
//!
//! Two renderings of one set of facts: [`registry_json`], which is the contract
//! any dashboard reads, and [`page`], a self-contained HTML page for a registry
//! running on its own. The auth-kit dashboard reads the first and renders it in
//! the kit's own layout, so the two never drift — there is one place that
//! decides what "stale" means.
//!
//! **Everything here is escaped, and that is not a formality.** A service name,
//! a hostname and a metadata value all arrive from whatever registered, and
//! anything that can reach the registry can choose them. A dashboard that
//! interpolated them raw would let a registration put script on the page of
//! whoever is looking into an incident.

use crate::instance::{Instance, now};
use crate::registry::{Mode, Registry};
use rustlavel_core::Json;

/// The registry as a dashboard wants it: grouped, counted, and told which
/// instances are stale.
pub fn registry_json(registry: &Registry) -> Json {
    let mode = registry.mode();
    let at = now();
    let instances = registry.all();

    let mut names: Vec<&str> = instances.iter().map(|held| held.service.as_str()).collect();
    names.sort_unstable();
    names.dedup();

    let services: Vec<Json> = names
        .iter()
        .map(|name| {
            let mine: Vec<&Instance> =
                instances.iter().filter(|held| held.service == *name).collect();
            let up = mine.iter().filter(|held| held.status.takes_traffic()).count();

            Json::object([
                ("name", Json::from(*name)),
                ("up", Json::from(up as f64)),
                ("total", Json::from(mine.len() as f64)),
                (
                    "instances",
                    Json::Array(mine.iter().map(|held| instance_json(held, at)).collect()),
                ),
            ])
        })
        .collect();

    let stale = instances.iter().filter(|held| !held.fresh_at(at)).count();
    let up = instances.iter().filter(|held| held.status.takes_traffic()).count();

    Json::object([
        ("mode", Json::from(mode_name(&mode))),
        (
            "counts",
            Json::object([
                ("services", Json::from(names.len() as f64)),
                ("instances", Json::from(instances.len() as f64)),
                ("up", Json::from(up as f64)),
                ("stale", Json::from(stale as f64)),
            ]),
        ),
        ("services", Json::Array(services)),
    ])
}

fn instance_json(instance: &Instance, at: u64) -> Json {
    Json::object([
        ("id", Json::from(instance.id.as_str())),
        ("host", Json::from(instance.host.as_str())),
        ("port", Json::from(instance.port as f64)),
        ("url", Json::from(instance.url())),
        ("status", Json::from(instance.status.as_str())),
        ("zone", instance.zone().map(Json::from).unwrap_or(Json::Null)),
        ("fresh", Json::from(instance.fresh_at(at))),
        // Seconds, not a timestamp: a dashboard would only subtract it from its
        // own clock, and its own clock is not the one that stamped this.
        ("silentFor", Json::from(at.saturating_sub(instance.last_seen) as f64)),
        ("renewInterval", Json::from(instance.renew_interval as f64)),
    ])
}

fn mode_name(mode: &Mode) -> &'static str {
    match mode {
        Mode::Evicting => "EVICTING",
        Mode::SelfPreservation => "SELF_PRESERVATION",
    }
}

/// Escape text for HTML.
///
/// Every value on this page came from something that registered, so all of it
/// goes through here.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(character),
        }
    }
    out
}

/// A dashboard for a registry running on its own.
///
/// One file, no assets, no JavaScript: it has to render when the estate it is
/// describing is on fire, and a page that needs a CDN is a page that does not.
pub fn page(registry: &Registry) -> String {
    let at = now();
    let mode = registry.mode();
    let instances = registry.all();

    let mut names: Vec<&str> = instances.iter().map(|held| held.service.as_str()).collect();
    names.sort_unstable();
    names.dedup();

    let banner = match mode {
        Mode::SelfPreservation => {
            "<p class=\"banner\"><strong>Self-preservation.</strong> Too much of the registry \
             went quiet at once to believe it, so nothing is being evicted and the list below \
             includes instances that may be gone.</p>"
        }
        Mode::Evicting => "",
    };

    let mut body = String::new();
    for name in &names {
        let mut rows = String::new();
        let mut mine: Vec<&Instance> =
            instances.iter().filter(|held| held.service == *name).collect();
        mine.sort_by(|a, b| a.id.cmp(&b.id));

        for instance in &mine {
            let silent = at.saturating_sub(instance.last_seen);
            let state = match (instance.status.takes_traffic(), instance.fresh_at(at)) {
                (true, true) => ("up", "taking traffic"),
                (true, false) => ("stale", "silent"),
                (false, _) => ("down", instance.status.as_str()),
            };

            rows.push_str(&format!(
                "<tr><td><span class=\"dot {}\"></span>{}</td><td><code>{}</code></td>\
                 <td><code>{}</code></td><td>{}</td><td>{silent}s ago</td></tr>",
                state.0,
                escape(state.1),
                escape(&instance.id),
                escape(&instance.url()),
                escape(instance.zone().unwrap_or("—")),
            ));
        }

        let up = mine.iter().filter(|held| held.status.takes_traffic()).count();
        body.push_str(&format!(
            "<section><h2>{} <span class=\"count\">{up} of {} up</span></h2>\
             <table><thead><tr><th>State</th><th>Instance</th><th>Address</th><th>Zone</th>\
             <th>Last heard</th></tr></thead><tbody>{rows}</tbody></table></section>",
            escape(name),
            mine.len(),
        ));
    }

    if names.is_empty() {
        body.push_str(
            "<p class=\"empty\">Nothing is registered. A service registers itself on the way \
             up — if one is running and not listed here, it is pointed somewhere else.</p>",
        );
    }

    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Service registry</title>
<style>
:root {{ color-scheme: light dark; --bg: #fbfbfa; --fg: #1c1c1a; --muted: #6b6b63;
         --line: #e4e4de; --card: #fff; --up: #2f7d4f; --stale: #b06d17; --down: #a33a2c; }}
@media (prefers-color-scheme: dark) {{
  :root {{ --bg: #16161a; --fg: #e8e8e4; --muted: #96968c; --line: #2c2c32; --card: #1e1e23; }}
}}
* {{ box-sizing: border-box; }}
body {{ margin: 0; padding: 2rem 1rem; background: var(--bg); color: var(--fg);
        font: 15px/1.5 ui-sans-serif, system-ui, -apple-system, sans-serif; }}
main {{ max-width: 60rem; margin: 0 auto; }}
h1 {{ font-size: 1.4rem; margin: 0 0 .25rem; }}
.meta {{ color: var(--muted); margin: 0 0 1.5rem; }}
.banner {{ background: color-mix(in srgb, var(--stale) 12%, var(--card));
           border: 1px solid var(--stale); border-radius: 8px; padding: .75rem 1rem; }}
section {{ background: var(--card); border: 1px solid var(--line); border-radius: 10px;
           padding: 1rem 1.25rem; margin-bottom: 1rem; overflow-x: auto; }}
h2 {{ font-size: 1rem; margin: 0 0 .75rem; display: flex; gap: .75rem; align-items: baseline; }}
.count {{ color: var(--muted); font-weight: 400; font-size: .85rem; }}
table {{ border-collapse: collapse; width: 100%; font-size: .9rem; }}
th {{ text-align: left; color: var(--muted); font-weight: 500; font-size: .78rem;
      text-transform: uppercase; letter-spacing: .04em; padding-bottom: .4rem; }}
td {{ padding: .45rem 1rem .45rem 0; border-top: 1px solid var(--line);
      font-variant-numeric: tabular-nums; }}
code {{ font: .85em ui-monospace, SFMono-Regular, Menlo, monospace; }}
.dot {{ display: inline-block; width: .55rem; height: .55rem; border-radius: 50%;
        margin-right: .5rem; }}
.dot.up {{ background: var(--up); }} .dot.stale {{ background: var(--stale); }}
.dot.down {{ background: var(--down); }}
.empty {{ color: var(--muted); }}
</style></head>
<body><main>
<h1>Service registry</h1>
<p class="meta">{services} services, {total} instances, {up} taking traffic.</p>
{banner}
{body}
</main></body></html>"#,
        services = names.len(),
        total = instances.len(),
        up = instances.iter().filter(|held| held.status.takes_traffic()).count(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{Instance, Status};

    fn filled() -> Registry {
        let registry = Registry::new();
        registry.register(Instance::new("orders", "10.0.0.1", 8080).meta("zone", "jakarta-a"));
        registry.register(
            Instance::new("orders", "10.0.0.2", 8080).status(Status::OutOfService),
        );
        registry.register(Instance::new("billing", "10.0.0.3", 8080));
        registry
    }

    #[test]
    fn the_document_groups_and_counts() {
        let document = registry_json(&filled());

        assert_eq!(document.get("counts.services").and_then(Json::as_f64), Some(2.0));
        assert_eq!(document.get("counts.instances").and_then(Json::as_f64), Some(3.0));
        assert_eq!(document.get("counts.up").and_then(Json::as_f64), Some(2.0));
        assert_eq!(document.get("mode").and_then(Json::as_str), Some("EVICTING"));

        // Sorted, so the dashboard does not reshuffle itself on every reload.
        assert_eq!(document.get("services.0.name").and_then(Json::as_str), Some("BILLING"));
        assert_eq!(document.get("services.1.name").and_then(Json::as_str), Some("ORDERS"));
        assert_eq!(document.get("services.1.up").and_then(Json::as_f64), Some(1.0));
        assert_eq!(document.get("services.1.total").and_then(Json::as_f64), Some(2.0));
    }

    /// A dashboard has no business subtracting a timestamp it did not stamp
    /// from a clock that did not stamp it.
    #[test]
    fn silence_is_reported_in_seconds_rather_than_as_a_timestamp() {
        let registry = Registry::new();
        let mut instance = Instance::new("orders", "10.0.0.1", 8080).renew_every(30);
        instance.last_seen = now().saturating_sub(120);
        registry.register(instance);

        let document = registry_json(&registry);
        assert_eq!(document.get("services.0.instances.0.fresh").and_then(Json::as_bool), Some(false));
        assert!(document.get("services.0.instances.0.silentFor").and_then(Json::as_f64).unwrap() >= 120.0);
    }

    #[test]
    fn the_page_lists_what_is_registered() {
        let html = page(&filled());

        assert!(html.contains("ORDERS"), "{html}");
        assert!(html.contains("http://10.0.0.1:8080"), "{html}");
        assert!(html.contains("jakarta-a"), "{html}");
        assert!(html.contains("OUT_OF_SERVICE"), "{html}");
    }

    /// Anything that can reach the registry chooses these strings. A dashboard
    /// that interpolated them raw would put script on the page of whoever is
    /// looking into an incident.
    #[test]
    fn a_registration_cannot_put_script_on_the_dashboard() {
        let registry = Registry::new();
        registry.register(
            Instance::new("<script>alert(1)</script>", "10.0.0.1", 8080)
                .id("\"><img src=x onerror=alert(1)>")
                .meta("zone", "<b>jakarta</b>"),
        );

        let html = page(&registry);

        // The check is for a tag that the browser would act on, not for the
        // text: `onerror=alert(1)` survives escaping as literal characters and
        // is harmless there. What must never appear is the `<` that starts a
        // tag, or the `"` that would close an attribute.
        assert!(!html.contains("<script>alert(1)"), "an unescaped service name");
        assert!(!html.contains("<img src=x"), "an unescaped instance id");
        assert!(!html.contains("<b>jakarta</b>"), "an unescaped zone");

        // And it is still readable — escaping is not redaction.
        assert!(html.contains("&lt;SCRIPT&gt;"), "the name was lost: {html}");
        assert!(html.contains("&lt;img src=x"), "the id was lost: {html}");
    }

    /// The banner is the difference between a five-minute diagnosis and an
    /// afternoon spent wondering why a dead instance is getting requests.
    #[test]
    fn self_preservation_is_said_out_loud() {
        let registry = Registry::new();
        for n in 0..10 {
            let mut instance =
                Instance::new("orders", format!("10.0.0.{n}"), 8080).renew_every(30);
            instance.last_seen = now().saturating_sub(300);
            registry.register(instance);
        }

        assert!(page(&registry).contains("Self-preservation"));
        assert_eq!(
            registry_json(&registry).get("mode").and_then(Json::as_str),
            Some("SELF_PRESERVATION")
        );
    }

    #[test]
    fn an_empty_registry_says_so_rather_than_rendering_nothing() {
        let html = page(&Registry::new());
        assert!(html.contains("Nothing is registered"), "{html}");
    }
}
