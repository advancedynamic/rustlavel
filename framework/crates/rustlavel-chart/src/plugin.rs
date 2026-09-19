//! Serving the library, from this application's own origin.

use rustlavel_core::Config;
use rustlavel_http::plugin::{Plugin, Setup};
use rustlavel_http::{Request, Response};

/// Chart.js itself, vendored. MIT; the licence travels beside it in
/// `assets/LICENSE-chart.js.md` and the copyright banner is still at the top
/// of the file.
pub const CHART_JS: &str = include_str!("../assets/chart.umd.js");

/// The script that reads `data-chart` and draws it.
pub const INIT_JS: &str = include_str!("../assets/rustlavel-chart.js");

pub const CHART_JS_PATH: &str = "/vendor/chart/chart.umd.js";
pub const INIT_JS_PATH: &str = "/vendor/chart/rustlavel-chart.js";

/// The version vendored here, so a page can cache-bust on it and a bug report
/// can name it.
pub const CHART_JS_VERSION: &str = "4.5.1";

/// Mounts the two scripts.
///
/// ```ignore
/// App::new()?.plugin(Charts::default())
/// ```
///
/// ```html
/// <script src="/vendor/chart/chart.umd.js" defer></script>
/// <script src="/vendor/chart/rustlavel-chart.js" defer></script>
/// ```
///
/// **From this origin, not a CDN.** `default-src 'self'` — the policy the auth
/// kit ships — refuses `cdn.jsdelivr.net`, and rightly: a CDN is a third party
/// who can change what your users execute. Vendoring is also what makes the
/// version above a fact rather than whatever the CDN served this morning.
pub struct Charts {
    prefix: String,
}

impl Default for Charts {
    fn default() -> Charts {
        Charts { prefix: "/vendor/chart".to_string() }
    }
}

impl Charts {
    /// Mount somewhere else. The paths in [`CHART_JS_PATH`] and
    /// [`INIT_JS_PATH`] are the defaults, so a template that hard-codes them
    /// and a plugin that was moved will disagree — which is why both are
    /// constants rather than strings written twice.
    pub fn at(prefix: impl Into<String>) -> Charts {
        Charts { prefix: prefix.into().trim_end_matches('/').to_string() }
    }
}

impl Plugin for Charts {
    fn name(&self) -> &'static str {
        "charts"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        let immutable = immutable_caching(setup.config);

        setup.router.get(&format!("{}/chart.umd.js", self.prefix), move |_: Request| async move {
            script(CHART_JS, immutable)
        });
        setup.router.get(&format!("{}/rustlavel-chart.js", self.prefix), move |_: Request| async move {
            script(INIT_JS, immutable)
        });
    }
}

/// A vendored file at a fixed version never changes, so in production it is
/// worth a year. In development it is not: an edit to the init script that the
/// browser refuses to re-fetch is an afternoon.
fn immutable_caching(config: &Config) -> bool {
    config.is_production()
}

fn script(body: &'static str, immutable: bool) -> Response {
    let response = Response::ok()
        .with_body(body)
        .with_header("content-type", "text/javascript; charset=utf-8");
    if immutable {
        return response.with_header("cache-control", "public, max-age=31536000, immutable");
    }
    response.with_header("cache-control", "no-cache")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vendored_library_is_the_version_the_constant_claims() {
        let banner: String = CHART_JS.lines().take(6).collect::<Vec<_>>().join("\n");
        assert!(
            banner.contains(&format!("Chart.js v{CHART_JS_VERSION}")),
            "the vendored file is not v{CHART_JS_VERSION}: {banner}"
        );
        assert!(banner.contains("MIT"), "the copyright banner was stripped: {banner}");
    }

    /// MIT requires the notice to travel with the copy.
    #[test]
    fn the_licence_travels_with_the_library() {
        let licence = include_str!("../assets/LICENSE-chart.js.md");
        assert!(licence.contains("MIT License"), "{licence}");
        assert!(licence.contains("Chart.js Contributors"));
    }

    /// The whole point of vendoring: nothing here reaches a third-party origin.
    #[test]
    fn nothing_is_fetched_from_a_cdn() {
        for script in [INIT_JS, CHART_JS] {
            for host in ["cdn.jsdelivr.net", "unpkg.com", "cdnjs.cloudflare.com"] {
                assert!(!script.contains(host), "a vendored script still reaches {host}");
            }
        }
    }

    /// The paths a template writes and the paths the plugin mounts are the
    /// same string, or the scripts 404 and every chart is a blank rectangle.
    #[test]
    fn the_default_mount_matches_the_published_paths() {
        let charts = Charts::default();
        assert_eq!(format!("{}/chart.umd.js", charts.prefix), CHART_JS_PATH);
        assert_eq!(format!("{}/rustlavel-chart.js", charts.prefix), INIT_JS_PATH);
    }

    #[test]
    fn a_custom_mount_drops_a_trailing_slash() {
        assert_eq!(Charts::at("/static/charts/").prefix, "/static/charts");
    }
}
