//! rustlavel-mcp: the Model Context Protocol, both directions.
//!
//! An application uses this crate to be an MCP **server** — handing an agent
//! its own features as tools and resources — and to be an MCP **client**,
//! using servers somebody else wrote. Both halves speak JSON-RPC 2.0 written
//! from scratch on [`rustlavel_core::Json`]; there is no MCP SDK underneath.
//!
//! # Serving
//!
//! Registrations go in one place, and a transport is chosen at the edge:
//!
//! ```ignore
//! fn mcp() -> Server {
//!     Server::new("shop", env!("CARGO_PKG_VERSION"))
//!         .instructions("Use `orders.total` before quoting a refund.")
//!         .tool(Tool::new(
//!             "orders.total",
//!             "Total value of one customer's orders",
//!             Schema::object().integer("customer_id", "Which customer"),
//!             |args: Json| async move { Ok(Json::from(total_for(&args))) },
//!         ))
//! }
//!
//! // Over HTTP, as part of the application:
//! App::new().plugin(Mcp::new(mcp()))
//!
//! // Or over stdio, for a desktop client that launches the binary:
//! stdio::serve_stdio(Arc::new(mcp())).await?
//! ```
//!
//! # Consuming
//!
//! ```ignore
//! let mut client = McpClient::spawn("some-mcp-server", &[])?;
//! client.initialize().await?;
//! for tool in client.list_tools().await? {
//!     println!("{}: {}", tool.name, tool.description);
//! }
//! ```
//!
//! # What the design insists on
//!
//! * **Schemas are built, not typed.** A tool declares its input through
//!   [`Schema`], and the same declaration is what an agent reads *and* what
//!   the server enforces — the two cannot drift apart.
//! * **Arguments are untrusted.** They arrive from a language model. Every
//!   `tools/call` is validated against the declared schema before any
//!   application code runs.
//! * **A tool cannot take the process down.** Handlers run inside a panic
//!   guard; a panic becomes an `isError` result the agent can read.
//! * **Every call is instrumented.** `tools/call` dispatches an `mcp.call`
//!   event — tool, duration, outcome — so Telescope can show it.

pub mod client;
pub mod http;
pub mod prompt;
pub mod protocol;
pub mod resource;
pub mod rpc;
pub mod schema;
pub mod server;
pub mod stdio;
pub mod tool;

pub use client::McpClient;
pub use http::Mcp;
pub use prompt::Prompt;
pub use protocol::{
    Content, Implementation, PROTOCOL_VERSION, PromptArgument, PromptInfo, PromptMessage,
    ResourceInfo, Role, ServerInfo, ToolInfo, ToolResult,
};
pub use resource::Resource;
pub use rpc::{Id, RpcError};
pub use schema::{Field, FieldType, Schema};
pub use server::Server;
pub use tool::Tool;

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_core::Json;
    use rustlavel_http::Plugin;
    use std::sync::Arc;

    /// The shape of a real `main.rs`, exercised end to end: one server
    /// definition, reached over both transports at once.
    #[tokio::test]
    async fn one_server_definition_serves_stdio_and_http_alike() {
        let server = Arc::new(Server::new("both", "0.1.0").tool(Tool::new(
            "double",
            "Double a number",
            Schema::object().integer("n", "The number to double"),
            |args: Json| async move {
                Ok(Json::from(args.get("n").and_then(Json::as_i64).unwrap_or(0) * 2))
            },
        )));

        // Over stdio, through the client half.
        let (client_side, server_side) = tokio::io::duplex(8 * 1024);
        let (read, write) = tokio::io::split(server_side);
        tokio::spawn(stdio::serve(Arc::clone(&server), read, write));

        let (client_read, client_write) = tokio::io::split(client_side);
        let mut mcp = McpClient::over_pipe(client_read, client_write);
        mcp.initialize().await.unwrap();
        let over_stdio =
            mcp.call_tool("double", Json::object([("n", Json::from(21))])).await.unwrap();

        // Over HTTP, through the router the same application serves pages from.
        let mut router = rustlavel_http::Router::new();
        let config = rustlavel_core::Config::with_defaults();
        let mut context = Some(rustlavel_core::Context::builder().config(config.clone()));
        let mut setup = rustlavel_http::Setup {
            router: &mut router,
            config: &config,
            context: &mut context,
        };
        Box::new(Mcp::shared(Arc::clone(&server))).register(&mut setup);

        let over_http = rustlavel_http::TestClient::new(router)
            .post_json(
                "/mcp",
                rpc::Request::new(
                    1i64,
                    "tools/call",
                    Json::object([
                        ("name", Json::from("double")),
                        ("arguments", Json::object([("n", Json::from(21))])),
                    ]),
                )
                .to_json(),
            )
            .await
            .assert_ok()
            .json();

        assert_eq!(over_stdio.text_content(), "42");
        assert_eq!(over_http.get("result.content.0.text").unwrap().as_str(), Some("42"));
    }
}
