//! The streamable HTTP transport, mounted on the application's own router.
//!
//! One line in `main.rs` and the application is an MCP server:
//!
//! ```ignore
//! App::new().plugin(Mcp::new(mcp_server()))
//! ```
//!
//! The endpoint is deliberately stateless — no session id is issued, no state
//! is kept between requests — because the dispatcher itself is. That keeps the
//! transport a translation layer: HTTP body in, JSON-RPC frame out, and any
//! instance of the application can answer any request.

use crate::server::Server;
use rustlavel_http::{Plugin, Request, Response, Setup, Status};
use std::sync::Arc;

/// The default mount point, and what every MCP client tries first.
pub const DEFAULT_PATH: &str = "/mcp";

/// The plugin that exposes a [`Server`] over HTTP.
pub struct Mcp {
    path: String,
    server: Arc<Server>,
}

impl Mcp {
    pub fn new(server: Server) -> Self {
        Mcp::shared(Arc::new(server))
    }

    /// Mount a server the application also holds a handle to — the same
    /// registrations served over stdio and HTTP at once.
    pub fn shared(server: Arc<Server>) -> Self {
        Mcp { path: DEFAULT_PATH.to_string(), server }
    }

    /// Mount somewhere other than `/mcp`.
    pub fn at(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Plugin for Mcp {
    fn name(&self) -> &'static str {
        "mcp"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        let server = Arc::clone(&self.server);

        // Only POST is registered. A client that opens the endpoint with GET is
        // asking for a server-initiated event stream, and the specification
        // says a server that does not offer one answers 405 — which is exactly
        // what the router does for an unregistered verb on a known path.
        setup.router.post(&self.path, move |request: Request| {
            let server = Arc::clone(&server);
            async move { handle(server, request).await }
        });
    }
}

/// Answer one HTTP request carrying a JSON-RPC frame.
async fn handle(server: Arc<Server>, request: Request) -> Response {
    // Lossy is right here: bytes that are not UTF-8 are not JSON either, and
    // the replacement characters produce the parse error the client should see.
    let body = request.body_string();

    match server.handle_text(&body).await {
        Some(answer) => Response::ok()
            .with_header("content-type", "application/json")
            .with_header("mcp-protocol-version", crate::protocol::PROTOCOL_VERSION)
            .with_body(answer),
        // Nothing to say: the frame held only notifications. The specification
        // asks for 202 with an empty body rather than an empty JSON document.
        None => Response::new(Status(202)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PROTOCOL_VERSION;
    use crate::schema::Schema;
    use crate::tool::Tool;
    use rustlavel_core::{Config, Context, Json};
    use rustlavel_http::{Router, TestClient};

    fn mcp_server() -> Server {
        Server::new("http-test", "0.1.0")
            .tool(Tool::new(
                "add",
                "Add two numbers",
                Schema::object().number("a", "First").number("b", "Second"),
                |args: Json| async move {
                    let a = args.get("a").and_then(Json::as_f64).unwrap_or_default();
                    let b = args.get("b").and_then(Json::as_f64).unwrap_or_default();
                    Ok(Json::from(a + b))
                },
            ))
            .resource(crate::resource::Resource::text("app://motd", "Message", || async {
                Ok("be excellent".to_string())
            }))
    }

    /// Register the plugin the way an application does, then hand back a test
    /// client — the whole path from `main.rs` to a response, minus the socket.
    fn client_for(plugin: Mcp) -> TestClient {
        let mut router = Router::new();
        let config = Config::with_defaults();
        let mut context = Some(Context::builder().config(config.clone()));
        let mut setup = Setup { router: &mut router, config: &config, context: &mut context };

        Box::new(plugin).register(&mut setup);
        TestClient::new(router)
    }

    fn frame(id: i64, method: &str, params: Json) -> Json {
        crate::rpc::Request::new(id, method, params).to_json()
    }

    #[tokio::test]
    async fn the_plugin_mounts_at_slash_mcp_by_default() {
        let client = client_for(Mcp::new(mcp_server()));

        client
            .post_json("/mcp", frame(1, "tools/list", Json::Null))
            .await
            .assert_ok()
            .assert_json("result.tools.0.name", "add")
            .assert_header("mcp-protocol-version", PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn the_mount_point_can_be_moved() {
        let client = client_for(Mcp::new(mcp_server()).at("/agent/mcp"));

        client.post_json("/agent/mcp", frame(1, "ping", Json::Null)).await.assert_ok();
        client.post_json("/mcp", frame(1, "ping", Json::Null)).await.assert_not_found();
    }

    #[tokio::test]
    async fn a_whole_handshake_and_call_runs_over_http() {
        let client = client_for(Mcp::new(mcp_server()));

        client
            .post_json(
                "/mcp",
                frame(
                    1,
                    "initialize",
                    Json::object([
                        ("protocolVersion", Json::from(PROTOCOL_VERSION)),
                        ("clientInfo", Json::object([("name", Json::from("test"))])),
                    ]),
                ),
            )
            .await
            .assert_ok()
            .assert_json("result.serverInfo.name", "http-test")
            .assert_json("result.protocolVersion", PROTOCOL_VERSION);

        client
            .post_json(
                "/mcp",
                frame(
                    2,
                    "tools/call",
                    Json::object([
                        ("name", Json::from("add")),
                        (
                            "arguments",
                            Json::object([("a", Json::from(2)), ("b", Json::from(3))]),
                        ),
                    ]),
                ),
            )
            .await
            .assert_ok()
            .assert_json("id", 2)
            .assert_json("result.content.0.text", "5")
            .assert_json("result.isError", false);
    }

    #[tokio::test]
    async fn a_notification_gets_an_accepted_status_and_no_body() {
        let client = client_for(Mcp::new(mcp_server()));

        let response = client
            .post_json("/mcp", crate::rpc::Notification::new("notifications/initialized", Json::Null).to_json())
            .await
            .assert_status(202);

        assert!(response.body().is_empty());
    }

    #[tokio::test]
    async fn a_resource_read_comes_back_over_http() {
        let client = client_for(Mcp::new(mcp_server()));

        client
            .post_json(
                "/mcp",
                frame(1, "resources/read", Json::object([("uri", Json::from("app://motd"))])),
            )
            .await
            .assert_ok()
            .assert_json("result.contents.0.text", "be excellent");
    }

    #[tokio::test]
    async fn an_invalid_argument_answers_200_with_a_json_rpc_error() {
        // The HTTP status describes the transport, not the call: the frame was
        // delivered fine, and the complaint lives in the JSON-RPC error object.
        let client = client_for(Mcp::new(mcp_server()));

        client
            .post_json(
                "/mcp",
                frame(
                    3,
                    "tools/call",
                    Json::object([
                        ("name", Json::from("add")),
                        (
                            "arguments",
                            Json::object([("a", Json::from("two")), ("b", Json::from(3))]),
                        ),
                    ]),
                ),
            )
            .await
            .assert_ok()
            .assert_json("id", 3)
            .assert_json("error.code", -32602)
            .assert_json_missing("result");
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_becomes_a_parse_error() {
        let client = client_for(Mcp::new(mcp_server()));

        client
            .send(
                rustlavel_http::Request::new(rustlavel_http::Method::Post, "/mcp")
                    .with_header("content-type", "application/json")
                    .with_body("{ nope"),
            )
            .await
            .assert_ok()
            .assert_json("error.code", -32700);
    }

    #[tokio::test]
    async fn a_get_on_the_endpoint_is_method_not_allowed() {
        // No server-initiated stream is offered, which the specification says
        // to report as 405 rather than a hanging connection.
        client_for(Mcp::new(mcp_server())).get("/mcp").await.assert_status(405);
    }

    #[tokio::test]
    async fn a_panicking_tool_over_http_answers_rather_than_500ing() {
        let server = Server::new("http-test", "0.1.0").tool(Tool::new(
            "explodes",
            "Panics",
            Schema::object(),
            |_: Json| async { panic!("boom over http") },
        ));

        client_for(Mcp::new(server))
            .post_json(
                "/mcp",
                frame(1, "tools/call", Json::object([("name", Json::from("explodes"))])),
            )
            .await
            .assert_ok()
            .assert_json("result.isError", true)
            .assert_see("boom over http");
    }

    #[tokio::test]
    async fn a_batch_posted_as_one_body_comes_back_as_one_array() {
        let client = client_for(Mcp::new(mcp_server()));
        let batch = Json::Array(vec![frame(1, "ping", Json::Null), frame(2, "tools/list", Json::Null)]);

        let response = client.post_json("/mcp", batch).await.assert_ok();
        assert_eq!(response.json().as_array().unwrap().len(), 2);
    }
}
