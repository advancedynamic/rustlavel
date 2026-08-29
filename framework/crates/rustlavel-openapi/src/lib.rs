//! rustlavel-openapi: API documentation, generated from the routes themselves.
//!
//! The router already knows every path, method, and parameter; the route
//! builder carries the prose. Nothing has to be repeated in a separate file,
//! which is the reason hand-written API docs go stale.
//!
//! ```ignore
//! App::new()?
//!     .routes(routes::api::routes)
//!     .plugin(OpenApi::new("Orders API", "1.0"))
//! ```

pub mod docs;

use rustlavel_core::{Config, Json};
use rustlavel_http::{Request, Response, Route, Router};

/// What the generated document says about the API as a whole.
#[derive(Debug, Clone)]
pub struct Info {
    pub title: String,
    pub version: String,
    pub description: Option<String>,
    /// The base URL clients should call.
    pub server: Option<String>,
    /// Paths under this prefix are documented; everything else is skipped.
    ///
    /// Defaults to `/api`, because a browser-facing page is not an API and
    /// documenting it produces noise nobody reads.
    pub prefix: String,
}

impl Default for Info {
    fn default() -> Self {
        Info {
            title: "API".into(),
            version: "1.0.0".into(),
            description: None,
            server: None,
            prefix: "/api".into(),
        }
    }
}

impl Info {
    pub fn from_config(config: &Config) -> Info {
        Info {
            title: config.string("openapi.title", &config.string("app.name", "API")),
            version: config.string("openapi.version", "1.0.0"),
            description: non_empty(config.string("openapi.description", "")),
            server: non_empty(config.string("app.url", "")),
            prefix: config.string("openapi.prefix", "/api"),
        }
    }
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

/// Build an OpenAPI 3.1 document from a router.
pub fn document(router: &Router, info: &Info) -> Json {
    let mut paths: std::collections::BTreeMap<String, Json> = std::collections::BTreeMap::new();

    for route in router.routes() {
        if !route.pattern.starts_with(&info.prefix) {
            continue;
        }
        // A wildcard route matches an open-ended family of paths; OpenAPI has
        // no way to say that, so documenting one would be a lie.
        if route.pattern.contains(":*") {
            continue;
        }

        let entry = paths.entry(route.pattern.clone()).or_insert_with(|| Json::Object(Default::default()));
        if let Json::Object(operations) = entry {
            operations.insert(route.method.as_str().to_lowercase(), operation(route));
        }
    }

    let mut root = vec![
        ("openapi", Json::from("3.1.0")),
        (
            "info",
            Json::object(
                [
                    Some(("title", Json::from(info.title.as_str()))),
                    Some(("version", Json::from(info.version.as_str()))),
                    info.description.as_ref().map(|d| ("description", Json::from(d.as_str()))),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
            ),
        ),
        ("paths", Json::Object(paths.into_iter().collect())),
    ];

    if let Some(server) = &info.server {
        root.push(("servers", Json::Array(vec![Json::object([("url", Json::from(server.as_str()))])])));
    }

    Json::object(root)
}

fn operation(route: &Route) -> Json {
    let mut fields = vec![(
        "responses",
        responses(route),
    )];

    if let Some(summary) = &route.summary {
        fields.push(("summary", Json::from(summary.as_str())));
    }
    if let Some(name) = &route.name {
        // The route's name is stable and unique, which is exactly what an
        // operationId has to be for a generated client to use it.
        fields.push(("operationId", Json::from(name.as_str())));
    }
    if let Some(tag) = &route.tag {
        fields.push(("tags", Json::Array(vec![Json::from(tag.as_str())])));
    }
    if route.deprecated {
        fields.push(("deprecated", Json::from(true)));
    }

    let parameters = parameters(route);
    if !parameters.is_empty() {
        fields.push(("parameters", Json::Array(parameters)));
    }

    Json::object(fields)
}

fn parameters(route: &Route) -> Vec<Json> {
    let described = |name: &str| {
        route
            .parameters
            .iter()
            .find(|(parameter, _)| parameter == name)
            .map(|(_, description)| description.clone())
    };

    let path_names = route.parameter_names();
    let mut out: Vec<Json> = path_names
        .iter()
        .map(|name| {
            parameter(name, "path", true, described(name))
        })
        .collect();

    // Anything documented that is not in the path is a query parameter.
    for (name, description) in &route.parameters {
        if path_names.iter().any(|path_name| path_name == name) {
            continue;
        }
        out.push(parameter(name, "query", false, Some(description.clone())));
    }

    out
}

fn parameter(name: &str, location: &str, required: bool, description: Option<String>) -> Json {
    let mut fields = vec![
        ("name", Json::from(name)),
        ("in", Json::from(location)),
        ("required", Json::from(required)),
        ("schema", Json::object([("type", Json::from("string"))])),
    ];
    if let Some(description) = description {
        fields.push(("description", Json::from(description)));
    }
    Json::object(fields)
}

fn responses(route: &Route) -> Json {
    if route.responses.is_empty() {
        // Every operation must document at least one response, so an
        // undocumented route still produces a valid document.
        return Json::object([(
            "200",
            Json::object([("description", Json::from("Successful response"))]),
        )]);
    }

    Json::Object(
        route
            .responses
            .iter()
            .map(|(status, description)| {
                (
                    status.to_string(),
                    Json::object([("description", Json::from(description.as_str()))]),
                )
            })
            .collect(),
    )
}

/// The routes that serve the document and the documentation page.
///
/// Registered *after* the application's own routes, because a document
/// generated before them would describe an empty API. That ordering is why
/// this is a function the `App` calls at the end rather than a plugin: a plugin
/// cannot see what is registered after it.
pub fn mount(router: &mut Router, info: &Info, path: &str) {
    let body = document(router, info).to_string();
    let page = docs::page(info, path);

    let document_path = path.to_string();
    router.get(&document_path, move |_request: Request| {
        let body = body.clone();
        async move {
            Response::ok().with_header("content-type", "application/json").with_body(body)
        }
    });

    // `/openapi.json` documents the API; `/openapi` is where a human reads it.
    let page_path = match document_path.strip_suffix(".json") {
        Some(stem) => stem.to_string(),
        None => format!("{document_path}/docs"),
    };
    router.get(&page_path, move |_request: Request| {
        let page = page.clone();
        async move { Response::html(page) }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_http::Request;

    async fn ok(_req: Request) -> &'static str {
        "ok"
    }

    fn router() -> Router {
        let mut router = Router::new();
        router.get("/", ok).describe("The home page");
        router
            .get("/api/users", ok)
            .name("users.index")
            .describe("List users")
            .tag("Users")
            .param("page", "Which page to return")
            .responds(200, "A page of users");
        router
            .get("/api/users/{id}", ok)
            .name("users.show")
            .describe("Fetch one user")
            .tag("Users")
            .param("id", "The user's id")
            .responds(200, "The user")
            .responds(404, "No such user");
        router.post("/api/users", ok).name("users.store").tag("Users").responds(201, "Created");
        router.get("/api/legacy", ok).deprecated();
        router.get("/api/files/{path:*}", ok);
        router.finalize();
        router
    }

    fn info() -> Info {
        Info { title: "Orders API".into(), version: "2.1".into(), ..Info::default() }
    }

    #[test]
    fn documents_only_the_api_prefix() {
        let document = document(&router(), &info());
        let paths = document.get("paths").unwrap().as_object().unwrap();

        assert!(paths.contains_key("/api/users"));
        assert!(!paths.contains_key("/"), "a browser page is not an API");
    }

    #[test]
    fn a_wildcard_route_is_left_out() {
        let document = document(&router(), &info());
        let paths = document.get("paths").unwrap().as_object().unwrap();

        // OpenAPI cannot express "everything under here", so claiming to would
        // be a lie rather than documentation.
        assert!(paths.keys().all(|path| !path.contains(":*")));
    }

    #[test]
    fn methods_on_one_path_share_an_entry() {
        let document = document(&router(), &info());
        let users = document.get("paths./api/users").unwrap().as_object().unwrap();

        assert!(users.contains_key("get"));
        assert!(users.contains_key("post"));
    }

    #[test]
    fn a_route_name_becomes_the_operation_id() {
        let document = document(&router(), &info());

        assert_eq!(
            document.get("paths./api/users/{id}.get.operationId").unwrap().as_str(),
            Some("users.show")
        );
    }

    #[test]
    fn path_parameters_are_required_and_query_parameters_are_not() {
        let document = document(&router(), &info());

        let show = document.get("paths./api/users/{id}.get.parameters").unwrap().as_array().unwrap();
        assert_eq!(show[0].get("name").unwrap().as_str(), Some("id"));
        assert_eq!(show[0].get("in").unwrap().as_str(), Some("path"));
        assert_eq!(show[0].get("required").unwrap().as_bool(), Some(true));
        assert_eq!(show[0].get("description").unwrap().as_str(), Some("The user's id"));

        let index = document.get("paths./api/users.get.parameters").unwrap().as_array().unwrap();
        assert_eq!(index[0].get("in").unwrap().as_str(), Some("query"));
        assert_eq!(index[0].get("required").unwrap().as_bool(), Some(false));
    }

    #[test]
    fn documented_responses_are_carried_over() {
        let document = document(&router(), &info());
        let responses = document.get("paths./api/users/{id}.get.responses").unwrap();

        assert_eq!(responses.get("200.description").unwrap().as_str(), Some("The user"));
        assert_eq!(responses.get("404.description").unwrap().as_str(), Some("No such user"));
    }

    #[test]
    fn an_undocumented_route_still_produces_a_valid_operation() {
        let document = document(&router(), &info());
        let legacy = document.get("paths./api/legacy.get").unwrap();

        // OpenAPI requires at least one response per operation.
        assert!(legacy.get("responses.200").is_some());
        assert_eq!(legacy.get("deprecated").unwrap().as_bool(), Some(true));
    }

    #[test]
    fn the_document_carries_the_api_identity() {
        let document = document(&router(), &info());

        assert_eq!(document.get("openapi").unwrap().as_str(), Some("3.1.0"));
        assert_eq!(document.get("info.title").unwrap().as_str(), Some("Orders API"));
        assert_eq!(document.get("info.version").unwrap().as_str(), Some("2.1"));
    }

    #[tokio::test]
    async fn the_document_and_the_page_are_served() {
        use rustlavel_http::TestClient;

        let mut router = router();
        mount(&mut router, &info(), "/openapi.json");

        let client = TestClient::new(router);

        client
            .get("/openapi.json")
            .await
            .assert_ok()
            .assert_header("content-type", "application/json")
            .assert_json("info.title", "Orders API");

        client.get("/openapi").await.assert_ok().assert_see("Orders API");
    }

    #[test]
    fn configuration_supplies_the_identity() {
        let config = Config::new();
        config.set("app.name", "Shop");
        config.set("app.url", "https://shop.example.com");
        config.set("openapi.version", "3.4");

        let info = Info::from_config(&config);
        assert_eq!(info.title, "Shop");
        assert_eq!(info.version, "3.4");

        let document = document(&router(), &info);
        assert_eq!(
            document.get("servers.0.url").unwrap().as_str(),
            Some("https://shop.example.com")
        );
    }
}
