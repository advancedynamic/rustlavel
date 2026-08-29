//! A tool: something an application lets an agent do.
//!
//! ```ignore
//! Tool::new(
//!     "orders.total",
//!     "Total value of one customer's orders",
//!     Schema::object().integer("customer_id", "Which customer"),
//!     |args: Json| async move { Ok(Json::from(total_for(args.get("customer_id")))) },
//! )
//! ```

use crate::protocol::{ToolInfo, ToolResult};
use crate::schema::Schema;
use rustlavel_core::{Json, Result};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// What a tool handler returns once boxed.
pub type ToolFuture = Pin<Box<dyn Future<Output = Result<Json>> + Send>>;

/// The work behind a tool.
///
/// Implemented for every `async fn(Json) -> Result<Json>`, so an application
/// writes a closure and never names this trait.
pub trait ToolHandler: Send + Sync + 'static {
    fn call(&self, arguments: Json) -> ToolFuture;
}

impl<F, Fut> ToolHandler for F
where
    F: Fn(Json) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Json>> + Send + 'static,
{
    fn call(&self, arguments: Json) -> ToolFuture {
        Box::pin(self(arguments))
    }
}

/// One registered tool.
#[derive(Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub schema: Schema,
    handler: Arc<dyn ToolHandler>,
}

impl Tool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        schema: Schema,
        handler: impl ToolHandler,
    ) -> Self {
        Tool {
            name: name.into(),
            description: description.into(),
            schema,
            handler: Arc::new(handler),
        }
    }

    /// The `tools/list` entry for this tool.
    pub fn info(&self) -> ToolInfo {
        ToolInfo::describe(&self.name, &self.description, &self.schema)
    }

    /// Run the handler, surviving whatever it does.
    ///
    /// A tool is application code reached by an agent over a socket. If it
    /// panics, the process must not go down with it and the connection must not
    /// drop — the agent gets an error result and can try something else. The
    /// panic is caught with the same machinery the HTTP layer uses for a
    /// panicking route handler.
    ///
    /// Arguments are *not* validated here; [`crate::Server`] does that before
    /// calling, so a handler can trust what it is handed.
    pub async fn call(&self, arguments: Json) -> ToolResult {
        rustlavel_http::panic::install_hook();

        let handler = Arc::clone(&self.handler);
        let outcome =
            rustlavel_http::panic::catch(async move { handler.call(arguments).await }).await;

        match outcome {
            Ok(Ok(value)) => ToolResult::from_value(value),
            Ok(Err(error)) => ToolResult::error(format!("`{}` failed: {error}", self.name)),
            Err(message) => {
                let at = rustlavel_http::panic::take_location()
                    .map(|l| format!(" at {}:{}", l.file, l.line))
                    .unwrap_or_default();
                rustlavel_core::error!("panic in MCP tool `{}`{at}: {message}", self.name);
                ToolResult::error(format!("`{}` panicked: {message}", self.name))
            }
        }
    }
}

impl std::fmt::Debug for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tool").field("name", &self.name).field("schema", &self.schema).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_core::Error;

    fn echo_tool() -> Tool {
        Tool::new(
            "echo",
            "Repeat a message back",
            Schema::object().string("message", "What to repeat"),
            |args: Json| async move {
                Ok(Json::from(args.get("message").and_then(Json::as_str).unwrap_or("").to_string()))
            },
        )
    }

    #[tokio::test]
    async fn a_tool_returns_its_value_as_text_content() {
        let result = echo_tool().call(Json::object([("message", Json::from("hi"))])).await;

        assert!(!result.is_error);
        assert_eq!(result.text_content(), "hi");
    }

    #[tokio::test]
    async fn a_handler_error_becomes_an_error_result_not_a_failure() {
        let tool = Tool::new("boom", "Always fails", Schema::object(), |_: Json| async {
            Err(Error::msg("the database is asleep"))
        });

        let result = tool.call(Json::Null).await;
        assert!(result.is_error);
        assert!(result.text_content().contains("the database is asleep"));
    }

    #[tokio::test]
    async fn a_panicking_handler_is_caught_and_reported() {
        let tool = Tool::new("panics", "Always panics", Schema::object(), |_: Json| async {
            panic!("index out of bounds");
        });

        let result = tool.call(Json::Null).await;
        assert!(result.is_error);
        assert!(result.text_content().contains("index out of bounds"));
    }

    #[tokio::test]
    async fn a_panic_after_an_await_point_is_caught_too() {
        let tool = Tool::new("late", "Panics later", Schema::object(), |_: Json| async {
            tokio::task::yield_now().await;
            panic!("after yielding");
        });

        assert!(tool.call(Json::Null).await.is_error);
    }

    #[test]
    fn a_tool_describes_itself_for_tools_list() {
        let info = echo_tool().info();

        assert_eq!(info.name, "echo");
        assert_eq!(info.input_schema.get("required").unwrap().to_string(), r#"["message"]"#);
    }
}
