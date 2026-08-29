//! The MCP server: what turns an application's features into an agent's tools.
//!
//! ```ignore
//! let server = Server::new("shop", "0.1.0")
//!     .instructions("Use `orders.total` before quoting a refund.")
//!     .tool(Tool::new("orders.total", "…", Schema::object().integer("id", "…"), handler))
//!     .resource(Resource::json("app://routes", "Routes", read_routes));
//! ```
//!
//! The dispatcher is transport-free and stateless: it takes JSON-RPC in and
//! gives JSON-RPC back. Stdio and HTTP are two thin shells around it, which is
//! why the whole protocol can be tested without a socket or a subprocess.

use crate::prompt::Prompt;
use crate::protocol::{PROTOCOL_VERSION, SUPPORTED_VERSIONS, Implementation, method};
use crate::resource::Resource;
use crate::rpc::{self, Frame, Id, Message, RpcError};
use crate::tool::Tool;
use rustlavel_core::events::Event;
use rustlavel_core::Json;
use std::collections::BTreeMap;
use std::time::Instant;

/// A registered MCP server.
///
/// Registrations live in `BTreeMap`s so `tools/list` comes back in a stable
/// order — an agent that re-reads the list should not see it shuffle.
pub struct Server {
    info: Implementation,
    instructions: Option<String>,
    tools: BTreeMap<String, Tool>,
    resources: BTreeMap<String, Resource>,
    prompts: BTreeMap<String, Prompt>,
}

impl Server {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Server {
            info: Implementation::new(name, version),
            instructions: None,
            tools: BTreeMap::new(),
            resources: BTreeMap::new(),
            prompts: BTreeMap::new(),
        }
    }

    /// Advice shown to the model once, at the start of a session.
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    pub fn tool(mut self, tool: Tool) -> Self {
        self.tools.insert(tool.name.clone(), tool);
        self
    }

    pub fn resource(mut self, resource: Resource) -> Self {
        self.resources.insert(resource.uri.clone(), resource);
        self
    }

    pub fn prompt(mut self, prompt: Prompt) -> Self {
        self.prompts.insert(prompt.name.clone(), prompt);
        self
    }

    pub fn name(&self) -> &str {
        &self.info.name
    }

    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// What this server advertises during the handshake.
    ///
    /// Only the capabilities that are actually backed by a registration are
    /// announced: a client that sees `tools` and gets an empty list has been
    /// told something untrue.
    pub fn capabilities(&self) -> Json {
        let mut pairs: Vec<(&str, Json)> = Vec::new();
        if !self.tools.is_empty() {
            pairs.push(("tools", Json::object([("listChanged", Json::from(false))])));
        }
        if !self.resources.is_empty() {
            pairs.push((
                "resources",
                Json::object([
                    ("listChanged", Json::from(false)),
                    ("subscribe", Json::from(false)),
                ]),
            ));
        }
        if !self.prompts.is_empty() {
            pairs.push(("prompts", Json::object([("listChanged", Json::from(false))])));
        }
        Json::object(pairs)
    }

    /// Handle one complete transport frame, batches included.
    ///
    /// `None` means there is nothing to send back — the frame held only
    /// notifications, which JSON-RPC answers with silence.
    pub async fn handle_text(&self, text: &str) -> Option<String> {
        let frame = match Frame::parse(text) {
            Ok(frame) => frame,
            // A frame we could not even parse has no id to answer against.
            Err(error) => {
                return Some(rpc::Response::failure(None, error).to_json().to_string());
            }
        };

        let batched = frame.is_batch();
        let mut answers = Vec::new();
        for item in frame.items() {
            if let Some(answer) = self.handle_value(item).await {
                answers.push(answer);
            }
        }
        rpc::encode(answers, batched)
    }

    /// Handle one decoded message, returning the answer it deserves.
    pub async fn handle_value(&self, value: &Json) -> Option<Json> {
        match Message::from_json(value) {
            Ok(Message::Request(request)) => {
                let id = request.id.clone();
                Some(match self.dispatch(&request.method, request.params).await {
                    Ok(result) => rpc::Response::success(id, result).to_json(),
                    Err(error) => rpc::Response::failure(Some(id), error).to_json(),
                })
            }
            // Notifications are acknowledged by doing the work and saying
            // nothing. `notifications/initialized` is the only one we expect.
            Ok(Message::Notification(_)) => None,
            // A response arriving at a server is a client bug, not ours to
            // answer — answering it would start a loop.
            Ok(Message::Response(_)) => None,
            Err(error) => {
                // Salvage the id if the frame had a usable one, so a client can
                // still correlate the complaint with what it sent.
                let id = value.get("id").and_then(Id::from_json);
                Some(rpc::Response::failure(id, error).to_json())
            }
        }
    }

    /// Route one method to its handler.
    pub async fn dispatch(&self, method: &str, params: Json) -> Result<Json, RpcError> {
        match method {
            method::INITIALIZE => Ok(self.initialize(&params)),
            method::PING => Ok(Json::Object(Default::default())),
            method::TOOLS_LIST => Ok(Json::object([(
                "tools",
                Json::Array(self.tools.values().map(|tool| tool.info().to_json()).collect()),
            )])),
            method::TOOLS_CALL => self.call_tool(&params).await,
            method::RESOURCES_LIST => Ok(Json::object([(
                "resources",
                Json::Array(
                    self.resources.values().map(|resource| resource.info().to_json()).collect(),
                ),
            )])),
            method::RESOURCES_READ => self.read_resource(&params).await,
            method::PROMPTS_LIST => Ok(Json::object([(
                "prompts",
                Json::Array(self.prompts.values().map(|prompt| prompt.info().to_json()).collect()),
            )])),
            method::PROMPTS_GET => self.get_prompt(&params).await,
            unknown => Err(RpcError::method_not_found(unknown)),
        }
    }

    /// The handshake.
    ///
    /// The version we answer with is the client's when we understand it, and
    /// ours otherwise: the specification asks the server to name a version it
    /// supports and let the client decide whether to continue.
    fn initialize(&self, params: &Json) -> Json {
        let asked = params.get("protocolVersion").and_then(Json::as_str).unwrap_or("");
        let version = if SUPPORTED_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSION };

        let mut pairs = vec![
            ("protocolVersion", Json::from(version)),
            ("capabilities", self.capabilities()),
            ("serverInfo", self.info.to_json()),
        ];
        if let Some(instructions) = &self.instructions {
            pairs.push(("instructions", Json::from(instructions.clone())));
        }
        Json::object(pairs)
    }

    async fn call_tool(&self, params: &Json) -> Result<Json, RpcError> {
        let Some(name) = params.get("name").and_then(Json::as_str) else {
            return Err(RpcError::invalid_params("`name` is required"));
        };
        let arguments = params
            .as_object()
            .and_then(|map| map.get("arguments"))
            .cloned()
            .unwrap_or(Json::Null);

        let started = Instant::now();
        let outcome = self.run_tool(name, arguments).await;

        // Telescope wants every call, the failures most of all.
        record_call(name, started, &outcome);

        outcome
    }

    async fn run_tool(&self, name: &str, arguments: Json) -> Result<Json, RpcError> {
        // An unknown tool is a protocol mistake, not a tool failure, so it
        // comes back as a JSON-RPC error rather than an `isError` result.
        let Some(tool) = self.tools.get(name) else {
            return Err(RpcError::invalid_params(format!("unknown tool `{name}`"))
                .with_data(Json::object([(
                    "available",
                    Json::Array(self.tools.keys().cloned().map(Json::from).collect()),
                )])));
        };

        // Arguments come from a language model. They are checked against the
        // declared schema here, before any application code sees them.
        if let Err(problems) = tool.schema.validate(&arguments) {
            return Err(RpcError::invalid_params(problems.join("; ")).with_data(Json::object([(
                "errors",
                Json::Array(problems.into_iter().map(Json::from).collect()),
            )])));
        }

        Ok(tool.call(arguments).await.to_json())
    }

    async fn read_resource(&self, params: &Json) -> Result<Json, RpcError> {
        let Some(uri) = params.get("uri").and_then(Json::as_str) else {
            return Err(RpcError::invalid_params("`uri` is required"));
        };
        let Some(resource) = self.resources.get(uri) else {
            return Err(RpcError::resource_not_found(uri));
        };

        resource.contents().await.map_err(RpcError::internal_error)
    }

    async fn get_prompt(&self, params: &Json) -> Result<Json, RpcError> {
        let Some(name) = params.get("name").and_then(Json::as_str) else {
            return Err(RpcError::invalid_params("`name` is required"));
        };
        let Some(prompt) = self.prompts.get(name) else {
            return Err(RpcError::invalid_params(format!("unknown prompt `{name}`")));
        };

        let arguments = params
            .as_object()
            .and_then(|map| map.get("arguments"))
            .cloned()
            .unwrap_or(Json::Null);

        let missing = prompt.missing_arguments(&arguments);
        if !missing.is_empty() {
            return Err(RpcError::invalid_params(missing.join("; ")));
        }

        prompt.render(arguments).await.map_err(RpcError::internal_error)
    }
}

/// Report a tool call on the instrumentation bus, so Telescope can show it.
///
/// Every attempt is reported, including one that never reached a handler:
/// an agent repeatedly calling a tool that does not exist is exactly the kind
/// of thing the dashboard should surface.
fn record_call(name: &str, started: Instant, outcome: &Result<Json, RpcError>) {
    if !rustlavel_core::events::has_subscribers() {
        return;
    }

    let (ok, error) = match outcome {
        Err(rpc) => (false, Some(rpc.message.clone())),
        Ok(result) => match result.get("isError").and_then(Json::as_bool) {
            Some(true) => (
                false,
                result.get("content.0.text").and_then(Json::as_str).map(str::to_string),
            ),
            _ => (true, None),
        },
    };

    let mut event = Event::new("mcp.call").with("tool", name).with("ok", ok).took(started.elapsed());
    if let Some(error) = error {
        event = event.with("error", error);
    }
    event.dispatch();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PromptArgument, PromptMessage, ToolResult};
    use crate::schema::Schema;
    use rustlavel_core::Error;

    fn server() -> Server {
        Server::new("test-app", "0.1.0")
            .instructions("Call `greet` to say hello.")
            .tool(Tool::new(
                "greet",
                "Greet somebody by name",
                Schema::object().string("name", "Who to greet"),
                |args: Json| async move {
                    let name = args.get("name").and_then(Json::as_str).unwrap_or("world");
                    Ok(Json::from(format!("Hello, {name}!")))
                },
            ))
            .tool(Tool::new("explodes", "Always panics", Schema::object(), |_: Json| async {
                panic!("held wrong");
            }))
            .tool(Tool::new("fails", "Always errors", Schema::object(), |_: Json| async {
                Err(Error::msg("upstream is down"))
            }))
            .resource(Resource::json("app://routes", "Routes", || async {
                Ok(r#"["GET /"]"#.to_string())
            }))
            .prompt(
                Prompt::new("review", "Review an order", |args: Json| async move {
                    let id = args.get("order_id").and_then(Json::as_str).unwrap_or("?");
                    Ok(vec![PromptMessage::user(format!("Review order {id}"))])
                })
                .argument(PromptArgument::new("order_id", "Which order")),
            )
    }

    /// Send a request and unwrap the successful result, failing loudly if the
    /// server answered with an error instead.
    async fn call(server: &Server, method: &str, params: Json) -> Json {
        server.dispatch(method, params).await.unwrap_or_else(|error| {
            panic!("{method} failed: {error}");
        })
    }

    #[tokio::test]
    async fn the_handshake_reports_the_protocol_version_and_capabilities() {
        let result = call(
            &server(),
            method::INITIALIZE,
            Json::object([("protocolVersion", Json::from(PROTOCOL_VERSION))]),
        )
        .await;

        assert_eq!(result.get("protocolVersion").unwrap().as_str(), Some("2025-06-18"));
        assert_eq!(result.get("serverInfo.name").unwrap().as_str(), Some("test-app"));
        assert_eq!(result.get("serverInfo.version").unwrap().as_str(), Some("0.1.0"));
        assert!(result.get("capabilities.tools").is_some());
        assert!(result.get("capabilities.resources").is_some());
        assert_eq!(
            result.get("instructions").unwrap().as_str(),
            Some("Call `greet` to say hello.")
        );
    }

    #[tokio::test]
    async fn an_older_protocol_version_is_answered_in_kind() {
        let older = call(
            &server(),
            method::INITIALIZE,
            Json::object([("protocolVersion", Json::from("2024-11-05"))]),
        )
        .await;
        assert_eq!(older.get("protocolVersion").unwrap().as_str(), Some("2024-11-05"));

        // Something we have never heard of gets our own version back instead.
        let unknown = call(
            &server(),
            method::INITIALIZE,
            Json::object([("protocolVersion", Json::from("1999-01-01"))]),
        )
        .await;
        assert_eq!(unknown.get("protocolVersion").unwrap().as_str(), Some(PROTOCOL_VERSION));
    }

    #[tokio::test]
    async fn a_server_with_nothing_registered_advertises_nothing() {
        let bare = Server::new("bare", "0.1.0");
        assert!(bare.capabilities().as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn tools_list_returns_every_tool_with_its_schema() {
        let result = call(&server(), method::TOOLS_LIST, Json::Null).await;
        let tools = result.get("tools").unwrap().as_array().unwrap();

        // Sorted by name, so the listing never shuffles between calls.
        let names: Vec<&str> =
            tools.iter().map(|t| t.get("name").unwrap().as_str().unwrap()).collect();
        assert_eq!(names, ["explodes", "fails", "greet"]);

        let greet = tools.iter().find(|t| t.get("name").unwrap().as_str() == Some("greet")).unwrap();
        assert_eq!(greet.get("description").unwrap().as_str(), Some("Greet somebody by name"));
        assert_eq!(greet.get("inputSchema.properties.name.type").unwrap().as_str(), Some("string"));
    }

    #[tokio::test]
    async fn tools_call_runs_the_handler_and_returns_content() {
        let result = call(
            &server(),
            method::TOOLS_CALL,
            Json::object([
                ("name", Json::from("greet")),
                ("arguments", Json::object([("name", Json::from("Ada"))])),
            ]),
        )
        .await;

        assert_eq!(result.get("isError").unwrap().as_bool(), Some(false));
        assert_eq!(result.get("content.0.type").unwrap().as_str(), Some("text"));
        assert_eq!(result.get("content.0.text").unwrap().as_str(), Some("Hello, Ada!"));
    }

    #[tokio::test]
    async fn an_unknown_tool_is_a_protocol_error_listing_what_exists() {
        let error = server()
            .dispatch(method::TOOLS_CALL, Json::object([("name", Json::from("nope"))]))
            .await
            .unwrap_err();

        assert_eq!(error.code, rpc::codes::INVALID_PARAMS);
        assert!(error.message.contains("unknown tool `nope`"));
        assert!(error.data.unwrap().to_string().contains("greet"));
    }

    #[tokio::test]
    async fn an_argument_of_the_wrong_type_never_reaches_the_handler() {
        let error = server()
            .dispatch(
                method::TOOLS_CALL,
                Json::object([
                    ("name", Json::from("greet")),
                    ("arguments", Json::object([("name", Json::from(42))])),
                ]),
            )
            .await
            .unwrap_err();

        assert_eq!(error.code, rpc::codes::INVALID_PARAMS);
        assert!(error.message.contains("`name` must be a string, found a number"));
        assert_eq!(
            error.data.unwrap().get("errors.0").unwrap().as_str(),
            Some("`name` must be a string, found a number")
        );
    }

    #[tokio::test]
    async fn a_missing_required_argument_is_rejected() {
        let error = server()
            .dispatch(method::TOOLS_CALL, Json::object([("name", Json::from("greet"))]))
            .await
            .unwrap_err();

        assert!(error.message.contains("`name` is required"));
    }

    #[tokio::test]
    async fn tools_call_without_a_name_is_invalid_params() {
        let error = server().dispatch(method::TOOLS_CALL, Json::Null).await.unwrap_err();
        assert_eq!(error.code, rpc::codes::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn a_panicking_tool_becomes_an_error_result_rather_than_a_crash() {
        let result =
            call(&server(), method::TOOLS_CALL, Json::object([("name", Json::from("explodes"))]))
                .await;

        assert_eq!(result.get("isError").unwrap().as_bool(), Some(true));
        assert!(result.get("content.0.text").unwrap().as_str().unwrap().contains("held wrong"));

        // The dispatcher is still alive and answering afterwards.
        let after = call(&server(), method::TOOLS_LIST, Json::Null).await;
        assert_eq!(after.get("tools").unwrap().as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_failing_tool_reports_through_is_error_not_a_protocol_error() {
        let result =
            call(&server(), method::TOOLS_CALL, Json::object([("name", Json::from("fails"))])).await;

        let parsed = ToolResult::from_json(&result).unwrap();
        assert!(parsed.is_error);
        assert!(parsed.text_content().contains("upstream is down"));
    }

    #[tokio::test]
    async fn resources_list_and_read_round_trip() {
        let listed = call(&server(), method::RESOURCES_LIST, Json::Null).await;
        assert_eq!(listed.get("resources.0.uri").unwrap().as_str(), Some("app://routes"));

        let read = call(
            &server(),
            method::RESOURCES_READ,
            Json::object([("uri", Json::from("app://routes"))]),
        )
        .await;
        assert_eq!(read.get("contents.0.text").unwrap().as_str(), Some(r#"["GET /"]"#));
    }

    #[tokio::test]
    async fn an_unknown_resource_uses_the_reserved_code() {
        let error = server()
            .dispatch(method::RESOURCES_READ, Json::object([("uri", Json::from("app://ghost"))]))
            .await
            .unwrap_err();

        assert_eq!(error.code, rpc::codes::RESOURCE_NOT_FOUND);
    }

    #[tokio::test]
    async fn prompts_list_and_get_round_trip() {
        let listed = call(&server(), method::PROMPTS_LIST, Json::Null).await;
        assert_eq!(listed.get("prompts.0.name").unwrap().as_str(), Some("review"));

        let got = call(
            &server(),
            method::PROMPTS_GET,
            Json::object([
                ("name", Json::from("review")),
                ("arguments", Json::object([("order_id", Json::from("7"))])),
            ]),
        )
        .await;
        assert_eq!(got.get("messages.0.content.text").unwrap().as_str(), Some("Review order 7"));
    }

    #[tokio::test]
    async fn a_prompt_missing_a_required_argument_is_rejected() {
        let error = server()
            .dispatch(method::PROMPTS_GET, Json::object([("name", Json::from("review"))]))
            .await
            .unwrap_err();

        assert_eq!(error.code, rpc::codes::INVALID_PARAMS);
        assert!(error.message.contains("`order_id` is required"));
    }

    #[tokio::test]
    async fn an_unknown_method_is_method_not_found() {
        let error = server().dispatch("tools/teleport", Json::Null).await.unwrap_err();
        assert_eq!(error.code, rpc::codes::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn ping_answers_with_an_empty_result() {
        assert_eq!(call(&server(), method::PING, Json::Null).await.to_string(), "{}");
    }

    // --- Frame handling, the layer transports actually call. ---

    #[tokio::test]
    async fn a_request_frame_comes_back_correlated_by_id() {
        let answer = server()
            .handle_text(r#"{"jsonrpc":"2.0","id":"abc","method":"tools/list"}"#)
            .await
            .unwrap();
        let value = Json::parse(&answer).unwrap();

        assert_eq!(value.get("jsonrpc").unwrap().as_str(), Some("2.0"));
        assert_eq!(value.get("id").unwrap().as_str(), Some("abc"));
        assert!(value.get("result.tools").is_some());
    }

    #[tokio::test]
    async fn a_notification_frame_is_answered_with_silence() {
        let answer =
            server().handle_text(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await;
        assert_eq!(answer, None);
    }

    #[tokio::test]
    async fn a_malformed_frame_becomes_a_parse_error_with_a_null_id() {
        let answer = server().handle_text("{ this is not json").await.unwrap();
        let value = Json::parse(&answer).unwrap();

        assert_eq!(value.get("error.code").unwrap().as_i64(), Some(rpc::codes::PARSE_ERROR));
        assert!(value.get("id").unwrap().is_null());
    }

    #[tokio::test]
    async fn an_invalid_message_keeps_the_id_it_arrived_with() {
        let answer = server().handle_text(r#"{"id":5,"method":"ping"}"#).await.unwrap();
        let value = Json::parse(&answer).unwrap();

        assert_eq!(value.get("error.code").unwrap().as_i64(), Some(rpc::codes::INVALID_REQUEST));
        assert_eq!(value.get("id").unwrap().as_i64(), Some(5));
    }

    #[tokio::test]
    async fn a_batch_is_answered_by_a_batch_of_the_same_requests() {
        let answer = server()
            .handle_text(
                r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},
                    {"jsonrpc":"2.0","method":"notifications/initialized"},
                    {"jsonrpc":"2.0","id":2,"method":"tools/list"},
                    "garbage"]"#,
            )
            .await
            .unwrap();

        let answers = Json::parse(&answer).unwrap();
        let items = answers.as_array().unwrap();

        // Three answers: the notification is silent, the garbage still gets one.
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].get("id").unwrap().as_i64(), Some(1));
        assert_eq!(items[1].get("id").unwrap().as_i64(), Some(2));
        assert_eq!(items[2].get("error.code").unwrap().as_i64(), Some(rpc::codes::INVALID_REQUEST));
    }

    #[tokio::test]
    async fn a_batch_of_only_notifications_is_answered_with_silence() {
        let answer = server()
            .handle_text(r#"[{"jsonrpc":"2.0","method":"notifications/initialized"}]"#)
            .await;
        assert_eq!(answer, None);
    }

    #[tokio::test]
    async fn an_empty_batch_is_an_invalid_request() {
        let answer = server().handle_text("[]").await.unwrap();
        let value = Json::parse(&answer).unwrap();
        assert_eq!(value.get("error.code").unwrap().as_i64(), Some(rpc::codes::INVALID_REQUEST));
    }

    #[tokio::test]
    async fn a_tool_call_is_reported_on_the_instrumentation_bus() {
        use rustlavel_core::events::{Event, subscribe};
        use std::sync::{Arc, Mutex};

        /// What one observed call recorded: whether it succeeded, and how long
        /// the bus said it took.
        type Recorded = Vec<(bool, Option<f64>)>;

        // The event bus is process-wide, so this test filters by a tool name no
        // other test uses rather than clearing subscribers out from under them.
        let seen: Arc<Mutex<Recorded>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        subscribe(move |event: &Event| {
            if event.kind != "mcp.call" {
                return;
            }
            if event.field("tool").and_then(Json::as_str) != Some("watched") {
                return;
            }
            let ok = event.field("ok").and_then(Json::as_bool).unwrap_or(false);
            sink.lock().unwrap().push((ok, event.duration_ms()));
        });

        let server = Server::new("events", "0.1.0").tool(Tool::new(
            "watched",
            "Observed by the event test",
            Schema::object(),
            |_: Json| async { Ok(Json::from("done")) },
        ));

        server
            .dispatch(method::TOOLS_CALL, Json::object([("name", Json::from("watched"))]))
            .await
            .unwrap();
        let _ = server
            .dispatch(method::TOOLS_CALL, Json::object([("name", Json::from("watched-typo"))]))
            .await;

        let recorded = seen.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1, "only the real call names the watched tool");
        assert!(recorded[0].0, "the call succeeded");
        assert!(recorded[0].1.is_some(), "the event carries a duration");
    }

    #[tokio::test]
    async fn a_failing_call_is_reported_as_not_ok() {
        use rustlavel_core::events::{Event, subscribe};
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        subscribe(move |event: &Event| {
            if event.kind == "mcp.call"
                && event.field("tool").and_then(Json::as_str) == Some("watched-failure")
            {
                sink.lock().unwrap().push(event.field("ok").and_then(Json::as_bool).unwrap_or(true));
            }
        });

        let server = Server::new("events", "0.1.0").tool(Tool::new(
            "watched-failure",
            "Observed by the event test",
            Schema::object(),
            |_: Json| async { Err(Error::msg("nope")) },
        ));

        server
            .dispatch(method::TOOLS_CALL, Json::object([("name", Json::from("watched-failure"))]))
            .await
            .unwrap();

        assert_eq!(seen.lock().unwrap().clone(), [false]);
    }
}
