//! The test client.
//!
//! Requests go straight through the router — no socket, no port, no waiting —
//! so an HTTP test costs about as much as calling a function:
//!
//! ```ignore
//! let client = TestClient::new(router);
//! client.get("/users").await.assert_ok().assert_see("Ada");
//! ```

use crate::method::Method;
use crate::request::Request;
use crate::response::Response;
use crate::router::Router;
use rustlavel_core::{Context, Json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct TestClient {
    router: Arc<Router>,
    context: Context,
    /// Cookies the "browser" is holding, so a session survives between
    /// requests the way it does for a real client. Without this, every test of
    /// anything stateful has to shuttle `set-cookie` by hand.
    jar: Arc<std::sync::Mutex<BTreeMap<String, String>>>,
}

impl TestClient {
    pub fn new(mut router: Router) -> Self {
        router.finalize();
        TestClient {
            router: Arc::new(router),
            context: Context::default(),
            jar: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
        }
    }

    /// Use an application context, so handlers can resolve real services.
    pub fn with_context(mut self, context: Context) -> Self {
        self.context = context;
        self
    }

    pub async fn send(&self, mut request: Request) -> TestResponse {
        // Send what the jar holds, unless the caller set its own header.
        if !request.headers().contains("cookie") {
            let jar = self.jar.lock().expect("cookie jar poisoned");
            if !jar.is_empty() {
                let header = jar
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                request.headers_mut().set("cookie", header);
            }
        }

        let request = request.with_context(self.context.clone());
        let response = self.router.dispatch(request).await;
        self.remember(&response);
        TestResponse { response }
    }

    /// Record any `Set-Cookie` the response carried.
    fn remember(&self, response: &Response) {
        let mut jar = self.jar.lock().expect("cookie jar poisoned");

        for header in response.headers.get_all("set-cookie") {
            let Some(pair) = header.split(';').next() else { continue };
            let Some((name, value)) = pair.split_once('=') else { continue };
            let name = name.trim().to_string();

            // Max-Age=0 is how a cookie is expired; drop it rather than keeping
            // an empty value that would look like a live session.
            if header.contains("Max-Age=0") {
                jar.remove(&name);
            } else {
                jar.insert(name, crate::url::decode(value.trim()));
            }
        }
    }

    /// The cookies this client is currently holding.
    pub fn cookies(&self) -> BTreeMap<String, String> {
        self.jar.lock().expect("cookie jar poisoned").clone()
    }

    /// Forget every cookie — the equivalent of a fresh browser.
    pub fn clear_cookies(&self) {
        self.jar.lock().expect("cookie jar poisoned").clear();
    }

    pub async fn get(&self, path: &str) -> TestResponse {
        self.send(Request::new(Method::Get, path)).await
    }

    pub async fn delete(&self, path: &str) -> TestResponse {
        self.send(Request::new(Method::Delete, path)).await
    }

    pub async fn post(&self, path: &str, form: &[(&str, &str)]) -> TestResponse {
        self.send(Request::new(Method::Post, path).with_form(form)).await
    }

    pub async fn post_json(&self, path: &str, body: Json) -> TestResponse {
        self.send(Request::new(Method::Post, path).with_json(body)).await
    }

    pub async fn put_json(&self, path: &str, body: Json) -> TestResponse {
        self.send(Request::new(Method::Put, path).with_json(body)).await
    }
}

/// A response with assertions attached, each returning `self` so they chain.
pub struct TestResponse {
    pub response: Response,
}

impl TestResponse {
    pub fn status(&self) -> u16 {
        self.response.status.code()
    }

    pub fn body(&self) -> String {
        self.response.body_string()
    }

    /// The raw body, for a response that is not text — a compressed one, say.
    pub fn body_bytes(&self) -> &[u8] {
        &self.response.body
    }

    pub fn json(&self) -> Json {
        Json::parse(&self.body()).unwrap_or_else(|e| {
            panic!("response body is not valid JSON ({e}); body was:\n{}", self.body())
        })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.response.headers.get(name)
    }

    #[track_caller]
    pub fn assert_status(self, expected: u16) -> Self {
        assert_eq!(
            self.status(),
            expected,
            "expected status {expected}, got {}. Body:\n{}",
            self.status(),
            self.body()
        );
        self
    }

    #[track_caller]
    pub fn assert_ok(self) -> Self {
        assert!(
            self.response.status.is_success(),
            "expected a 2xx status, got {}. Body:\n{}",
            self.status(),
            self.body()
        );
        self
    }

    #[track_caller]
    pub fn assert_not_found(self) -> Self {
        self.assert_status(404)
    }

    /// Assert the body contains this text.
    #[track_caller]
    pub fn assert_see(self, needle: &str) -> Self {
        assert!(
            self.body().contains(needle),
            "expected the body to contain {needle:?}. Body:\n{}",
            self.body()
        );
        self
    }

    #[track_caller]
    pub fn assert_dont_see(self, needle: &str) -> Self {
        assert!(
            !self.body().contains(needle),
            "expected the body not to contain {needle:?}. Body:\n{}",
            self.body()
        );
        self
    }

    #[track_caller]
    pub fn assert_header(self, name: &str, expected: &str) -> Self {
        assert_eq!(self.header(name), Some(expected), "header `{name}` did not match");
        self
    }

    #[track_caller]
    pub fn assert_redirect(self, location: &str) -> Self {
        assert!(
            self.response.status.is_redirect(),
            "expected a redirect, got {}",
            self.status()
        );
        self.assert_header("location", location)
    }

    /// Assert a dotted path in the JSON body equals a value.
    #[track_caller]
    pub fn assert_json(self, path: &str, expected: impl Into<Json>) -> Self {
        let body = self.json();
        let found = body.get(path);
        let expected = expected.into();
        assert_eq!(
            found,
            Some(&expected),
            "at JSON path `{path}` expected {}, found {}",
            expected,
            found.map_or("nothing".to_string(), Json::to_string)
        );
        self
    }

    #[track_caller]
    pub fn assert_json_missing(self, path: &str) -> Self {
        assert!(self.json().get(path).is_none(), "expected no value at JSON path `{path}`");
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router() -> Router {
        let mut router = Router::new();
        router.get("/", |_req: Request| async { "Hello, Ada" });
        router.get("/api", |_req: Request| async {
            Json::object([("name", "Rustlavel".into()), ("stars", 42.into())])
        });
        router.post("/users", |mut req: Request| async move {
            let name = req.input("name").unwrap_or_default();
            (201, Json::object([("created", Json::from(name))]))
        });
        router.get("/old", |_req: Request| async { Response::redirect("/new") });
        router
    }

    #[tokio::test]
    async fn asserts_on_html_responses() {
        TestClient::new(router()).get("/").await.assert_ok().assert_see("Ada").assert_dont_see("Bob");
    }

    #[tokio::test]
    async fn asserts_on_json_paths() {
        TestClient::new(router())
            .get("/api")
            .await
            .assert_ok()
            .assert_json("name", "Rustlavel")
            .assert_json("stars", 42)
            .assert_json_missing("missing");
    }

    #[tokio::test]
    async fn posts_forms_and_json() {
        let client = TestClient::new(router());

        client.post("/users", &[("name", "ada")]).await.assert_status(201).assert_json("created", "ada");
        client
            .post_json("/users", Json::object([("name", "grace".into())]))
            .await
            .assert_json("created", "grace");
    }

    #[tokio::test]
    async fn cookies_persist_between_requests_like_a_browser() {
        let mut router = Router::new();
        router.post("/login", |_req: Request| async {
            Response::text("in").with_cookie(crate::Cookie::new("session", "abc123"))
        });
        router.get("/me", |req: Request| async move {
            req.cookie("session").unwrap_or_else(|| "anonymous".to_string())
        });
        router.post("/logout", |_req: Request| async {
            Response::text("out").without_cookie("session")
        });

        let client = TestClient::new(router);

        client.get("/me").await.assert_see("anonymous");

        client.post("/login", &[]).await.assert_ok();
        assert_eq!(client.cookies().get("session").map(String::as_str), Some("abc123"));
        client.get("/me").await.assert_see("abc123");

        client.post("/logout", &[]).await.assert_ok();
        client.get("/me").await.assert_see("anonymous");
    }

    #[tokio::test]
    async fn an_explicit_cookie_header_overrides_the_jar() {
        let mut router = Router::new();
        router.get("/me", |req: Request| async move {
            req.cookie("session").unwrap_or_default()
        });
        let client = TestClient::new(router);

        client
            .send(Request::new(Method::Get, "/me").with_header("cookie", "session=explicit"))
            .await
            .assert_see("explicit");
    }

    #[tokio::test]
    async fn asserts_redirects_and_missing_routes() {
        let client = TestClient::new(router());

        client.get("/old").await.assert_redirect("/new");
        client.get("/nowhere").await.assert_not_found();
    }
}
