//! Wiring the template engine into requests and responses.
//!
//! The engine itself knows nothing about HTTP; this is the thin layer that
//! lets a handler answer with a rendered view.

use rustlavel_core::{Config, Result};
use rustlavel_http::{Request, Response};
use rustlavel_view::{Context, Engine};

/// Build an engine from the application's configuration.
///
/// Templates reload from disk outside production, so editing one is visible on
/// the next request without a recompile.
pub fn engine_from_config(config: &Config, root: &std::path::Path) -> Engine {
    let directory = config.string("view.root", rustlavel_view::DEFAULT_ROOT);
    let reload = config.bool("view.reload", !config.is_production());

    Engine::new(root.join(directory)).with_reload(reload)
}

/// Rendering, available on every request once the engine is registered.
///
/// A trait rather than an inherent method because `Request` lives in the HTTP
/// crate, which knows nothing about views.
pub trait Views {
    /// Render a template into an HTML response.
    fn view(&self, name: &str, context: &Context) -> Result<Response>;

    /// Render a template to a string, for an email body or a fragment.
    fn render(&self, name: &str, context: &Context) -> Result<String>;
}

impl Views for Request {
    fn view(&self, name: &str, context: &Context) -> Result<Response> {
        Ok(Response::html(self.render(name, context)?))
    }

    fn render(&self, name: &str, context: &Context) -> Result<String> {
        let engine = self.state::<Engine>().ok_or_else(|| {
            rustlavel_core::Error::msg(
                "no template engine is registered. Call `.views()` on the App, or create a \
                 `resources/views` directory so it is picked up automatically.",
            )
        })?;
        engine.render(name, context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::App;
    use rustlavel_core::Json;

    fn fixture_views(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rustlavel-view-wiring-{name}"));
        let views = root.join("resources/views");
        std::fs::create_dir_all(&views).unwrap();
        std::fs::write(
            views.join("greeting.rl.html"),
            "<h1>Hello, {{ name }}</h1>",
        )
        .unwrap();
        root
    }

    #[tokio::test]
    async fn a_handler_can_answer_with_a_rendered_view() {
        let root = fixture_views("render");
        let client = App::bare()
            .views(Engine::new(root.join("resources/views")))
            .routes(|r| {
                r.get("/", |req: Request| async move {
                    req.view("greeting", &Context::new().with("name", "Ada"))
                });
            })
            .test_client();

        client.get("/").await.assert_ok().assert_see("Hello, Ada");
    }

    #[tokio::test]
    async fn template_output_is_escaped() {
        let root = fixture_views("escaping");
        let client = App::bare()
            .views(Engine::new(root.join("resources/views")))
            .routes(|r| {
                r.get("/", |req: Request| async move {
                    req.view("greeting", &Context::new().with("name", "<script>alert(1)</script>"))
                });
            })
            .test_client();

        client
            .get("/")
            .await
            .assert_ok()
            .assert_dont_see("<script>")
            .assert_see("&lt;script&gt;");
    }

    #[tokio::test]
    async fn a_missing_engine_explains_itself() {
        let client = App::bare()
            .routes(|r| {
                r.get("/", |req: Request| async move {
                    req.view("anything", &Context::new())
                });
            })
            .test_client();

        rustlavel_http::error_page::set_debug(true);
        let response = client.get("/").await.assert_status(500);
        assert!(response.body().contains("no template engine is registered"));
        rustlavel_http::error_page::set_debug(false);
    }

    #[test]
    fn configuration_controls_the_root_and_reloading() {
        let config = Config::new();
        config.set("view.root", "templates");
        config.set("app.env", "production");

        let engine = engine_from_config(&config, std::path::Path::new("/app"));

        assert_eq!(engine.root(), std::path::Path::new("/app/templates"));
        assert!(!engine.reloads(), "production should not stat templates on every render");
    }

    #[test]
    fn context_accepts_json_values() {
        let context = Context::new().with("count", 3).with("tags", Json::from(vec!["a", "b"]));

        assert_eq!(context.get("count").and_then(Json::as_i64), Some(3));
        assert_eq!(context.get("tags").and_then(Json::as_array).map(<[Json]>::len), Some(2));
    }
}
