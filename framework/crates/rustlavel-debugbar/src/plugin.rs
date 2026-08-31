//! Turning the bar on, and the rules about when it may appear.

use crate::collector::Collector;
use crate::render;
use rustlavel_core::Config;
use rustlavel_http::plugin::{Plugin, Setup};
use rustlavel_http::{Middleware, Next, Request, Response};
use std::future::Future;
use std::pin::Pin;

/// The development overlay.
///
/// | Config key          | Meaning                                        |
/// |---------------------|------------------------------------------------|
/// | `debugbar.enabled`  | Show it at all. Defaults to false in production.|
#[derive(Default)]
pub struct DebugBar {
    even_in_production: bool,
}

impl DebugBar {
    pub fn new() -> DebugBar {
        DebugBar::default()
    }

    /// Show the bar in production too.
    ///
    /// Spelled out in full because of what it means: the bar writes SQL, cache
    /// keys and timings into the HTML of every page, where a browser, a
    /// screenshot and a bug report will all pick them up. There is no
    /// authentication in front of it, because it is not a page — it is part of
    /// yours.
    pub fn even_in_production(mut self) -> DebugBar {
        self.even_in_production = true;
        self
    }

    fn wanted(&self, config: &Config) -> bool {
        config.bool("debugbar.enabled", !config.is_production() || self.even_in_production)
    }
}

impl Plugin for DebugBar {
    fn name(&self) -> &'static str {
        "debugbar"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        if !self.wanted(setup.config) {
            return;
        }
        if setup.config.is_production() {
            rustlavel_core::warn!(
                "debugbar: on in production. Every page now carries its own SQL and timings, \
                 to anyone who views source."
            );
        }

        Collector::install();
        setup.router.middleware(Inject);
    }
}

/// Collects for the request, then writes the bar into the page it produced.
struct Inject;

impl Middleware for Inject {
    fn handle(&self, request: Request, next: Next) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        Box::pin(async move {
            let route = request.path().to_string();
            let (mut response, collected) = Collector::collect(next.run(request)).await;

            if !is_a_page(&response) {
                return response;
            }

            let body = response.body_string();
            let bar = render::bar(&collected, &route, response.status.code());

            // Before `</body>`, and only if there is one. A fragment returned to
            // a fetch() has no closing tag, and appending to it would put a
            // debug bar inside whatever element the page dropped it into.
            let Some(at) = body.to_lowercase().rfind("</body>") else {
                return response;
            };

            let mut page = String::with_capacity(body.len() + bar.len());
            page.push_str(&body[..at]);
            page.push_str(&bar);
            page.push_str(&body[at..]);

            response = response.with_html(page);
            response
        })
    }
}

/// Whether this response is a page a person is about to look at.
///
/// Deliberately narrow. A JSON API response with a debug bar spliced into it is
/// no longer JSON, and the client parsing it gets a syntax error instead of
/// data — which is a far worse afternoon than having no bar.
fn is_a_page(response: &Response) -> bool {
    if !response.status.is_success() && response.status != rustlavel_http::Status::NOT_FOUND {
        return false;
    }
    response
        .headers
        .get("content-type")
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/html"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_http::{Router, Status, TestClient};

    fn app() -> TestClient {
        Collector::install();
        let mut router = Router::new();
        router.middleware(Inject);

        router.get("/page", |_req: Request| async move {
            Response::html("<html><body><h1>Hi</h1></body></html>")
        });
        router.get("/api", |_req: Request| async move {
            Response::json(rustlavel_core::Json::object([("ok", true.into())]))
        });
        router.get("/fragment", |_req: Request| async move {
            Response::html("<li>a row with no body tag</li>")
        });
        router.get("/download", |_req: Request| async move {
            Response::text("id,name\n1,Ada").with_header("content-type", "text/csv")
        });
        router.get("/redirect", |_req: Request| async move { Response::see_other("/page") });

        TestClient::new(router)
    }

    #[tokio::test]
    async fn a_page_gets_the_bar_just_before_the_closing_body_tag() {
        let body = app().get("/page").await.assert_ok().body();

        assert!(body.contains("rl-bar"), "the bar should be on the page");
        let bar_at = body.find("rl-bar").unwrap();
        let close_at = body.rfind("</body>").unwrap();
        assert!(bar_at < close_at, "the bar goes inside the body, not after it");
        assert!(body.contains("<h1>Hi</h1>"), "the page itself survives");
    }

    #[tokio::test]
    async fn json_is_left_alone_because_a_bar_would_stop_it_parsing() {
        let response = app().get("/api").await.assert_ok();
        let body = response.body();

        assert!(!body.contains("rl-bar"), "a bar in JSON breaks every client of it");
        rustlavel_core::Json::parse(&body).expect("the response is still valid JSON");
    }

    #[tokio::test]
    async fn a_fragment_with_no_body_tag_is_left_alone() {
        // A partial returned to fetch() has no </body>. Appending the bar would
        // drop a fixed-position overlay inside whatever element received it.
        let body = app().get("/fragment").await.assert_ok().body();

        assert!(!body.contains("rl-bar"));
        assert_eq!(body, "<li>a row with no body tag</li>");
    }

    #[tokio::test]
    async fn a_download_is_left_alone() {
        let body = app().get("/download").await.assert_ok().body();

        assert!(!body.contains("rl-bar"), "a CSV with HTML in it is a corrupt CSV");
        assert_eq!(body, "id,name\n1,Ada");
    }

    #[tokio::test]
    async fn a_redirect_is_left_alone() {
        let response = app().get("/redirect").await.assert_status(303);
        assert!(!response.body().contains("rl-bar"));
    }

    #[test]
    fn it_is_off_in_production_unless_somebody_says_otherwise_in_full() {
        let production = Config::with_defaults();
        production.set("app.env", rustlavel_core::Json::from("production"));

        assert!(!DebugBar::new().wanted(&production), "off by default in production");
        assert!(DebugBar::new().even_in_production().wanted(&production));
    }

    #[test]
    fn it_is_on_in_development_without_being_asked() {
        let config = Config::with_defaults();
        assert!(DebugBar::new().wanted(&config));
    }

    #[test]
    fn config_can_turn_it_off_even_in_development() {
        let config = Config::with_defaults();
        config.set("debugbar.enabled", rustlavel_core::Json::Bool(false));
        assert!(!DebugBar::new().wanted(&config));
    }

    #[tokio::test]
    async fn a_404_page_still_gets_a_bar() {
        // The request you most want to inspect is often the one that failed.
        let mut router = Router::new();
        router.middleware(Inject);
        router.fallback(|_req: Request| async move {
            Response::html("<html><body>Not found</body></html>").with_status(Status::NOT_FOUND)
        });

        let body = TestClient::new(router).get("/nowhere").await.assert_status(404).body();
        assert!(body.contains("rl-bar"));
    }
}
