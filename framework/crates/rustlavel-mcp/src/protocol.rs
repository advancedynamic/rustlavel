//! The MCP message shapes, as the specification defines them.
//!
//! Only the wire vocabulary lives here — method names, the handshake, the way
//! a tool result is spelled. Both halves of the crate share it: the server
//! writes these shapes and the client reads them, so a mistake in one is
//! caught by a test of the other.

use crate::schema::Schema;
use rustlavel_core::Json;

/// The protocol revision this crate implements.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Revisions we still understand, newest first.
///
/// A client that asks for an older revision gets it back rather than a
/// failure: the message shapes we use are common to all three, and refusing
/// would break desktop clients that have not caught up.
pub const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// Every method name in one place, so a typo is a compile error.
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const INITIALIZED: &str = "notifications/initialized";
    pub const PING: &str = "ping";
    pub const TOOLS_LIST: &str = "tools/list";
    pub const TOOLS_CALL: &str = "tools/call";
    pub const RESOURCES_LIST: &str = "resources/list";
    pub const RESOURCES_READ: &str = "resources/read";
    pub const PROMPTS_LIST: &str = "prompts/list";
    pub const PROMPTS_GET: &str = "prompts/get";
}

/// Who is on the other end: `{"name": "rustlavel", "version": "0.1.0"}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Implementation {
    pub name: String,
    pub version: String,
}

impl Implementation {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Implementation { name: name.into(), version: version.into() }
    }

    pub fn to_json(&self) -> Json {
        Json::object([
            ("name", Json::from(self.name.clone())),
            ("version", Json::from(self.version.clone())),
        ])
    }

    pub fn from_json(value: &Json) -> Option<Implementation> {
        Some(Implementation::new(
            value.get("name")?.as_str()?,
            value.get("version").and_then(Json::as_str).unwrap_or("unknown"),
        ))
    }
}

/// One piece of a tool result or a prompt message.
///
/// Only text for now. The variant exists so images and embedded resources can
/// be added later without every caller changing shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Text(String),
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Content::Text(text.into())
    }

    pub fn as_text(&self) -> &str {
        match self {
            Content::Text(text) => text,
        }
    }

    pub fn to_json(&self) -> Json {
        match self {
            Content::Text(text) => Json::object([
                ("type", Json::from("text")),
                ("text", Json::from(text.clone())),
            ]),
        }
    }

    pub fn from_json(value: &Json) -> Option<Content> {
        match value.get("type")?.as_str()? {
            "text" => Some(Content::text(value.get("text")?.as_str()?)),
            _ => None,
        }
    }
}

/// What `tools/call` answers with.
///
/// A failing tool is *not* a JSON-RPC error: the model asked for something and
/// deserves to read why it did not work, so the failure travels as a normal
/// result with `isError` set. Protocol mistakes — an unknown tool, arguments
/// that do not match the schema — are the ones that become JSON-RPC errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub content: Vec<Content>,
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        ToolResult { content: vec![Content::text(text)], is_error: false }
    }

    pub fn error(text: impl Into<String>) -> Self {
        ToolResult { content: vec![Content::text(text)], is_error: true }
    }

    /// Turn whatever a handler returned into content.
    ///
    /// A string is already text; anything else is rendered as compact JSON,
    /// which is what a model can actually read back.
    pub fn from_value(value: Json) -> Self {
        match value {
            Json::String(text) => ToolResult::text(text),
            other => ToolResult::text(other.to_string()),
        }
    }

    /// Every text block joined — how a caller usually wants to read a result.
    pub fn text_content(&self) -> String {
        self.content.iter().map(Content::as_text).collect::<Vec<_>>().join("\n")
    }

    pub fn to_json(&self) -> Json {
        Json::object([
            ("content", Json::Array(self.content.iter().map(Content::to_json).collect())),
            ("isError", Json::from(self.is_error)),
        ])
    }

    pub fn from_json(value: &Json) -> Option<ToolResult> {
        let content = value
            .get("content")?
            .as_array()?
            .iter()
            .filter_map(Content::from_json)
            .collect();
        Some(ToolResult {
            content,
            is_error: value.get("isError").and_then(Json::as_bool).unwrap_or(false),
        })
    }
}

/// A tool as it appears in `tools/list` — the client's view of one.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    /// Left as raw JSON: a client must pass a foreign server's schema through
    /// untouched, whatever keywords it uses.
    pub input_schema: Json,
}

impl ToolInfo {
    pub fn to_json(&self) -> Json {
        Json::object([
            ("name", Json::from(self.name.clone())),
            ("description", Json::from(self.description.clone())),
            ("inputSchema", self.input_schema.clone()),
        ])
    }

    pub fn from_json(value: &Json) -> Option<ToolInfo> {
        Some(ToolInfo {
            name: value.get("name")?.as_str()?.to_string(),
            description: value.get("description").and_then(Json::as_str).unwrap_or("").to_string(),
            input_schema: value.get("inputSchema").cloned().unwrap_or(Json::Null),
        })
    }

    /// Build the descriptor for a tool defined in this process.
    pub fn describe(name: &str, description: &str, schema: &Schema) -> ToolInfo {
        ToolInfo {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: schema.to_json(),
        }
    }
}

/// A resource as it appears in `resources/list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceInfo {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: String,
}

impl ResourceInfo {
    pub fn to_json(&self) -> Json {
        let mut pairs = vec![
            ("uri", Json::from(self.uri.clone())),
            ("name", Json::from(self.name.clone())),
            ("mimeType", Json::from(self.mime_type.clone())),
        ];
        if let Some(description) = &self.description {
            pairs.push(("description", Json::from(description.clone())));
        }
        Json::object(pairs)
    }

    pub fn from_json(value: &Json) -> Option<ResourceInfo> {
        Some(ResourceInfo {
            uri: value.get("uri")?.as_str()?.to_string(),
            name: value.get("name").and_then(Json::as_str).unwrap_or("").to_string(),
            description: value.get("description").and_then(Json::as_str).map(str::to_string),
            mime_type: value
                .get("mimeType")
                .and_then(Json::as_str)
                .unwrap_or("text/plain")
                .to_string(),
        })
    }
}

/// One argument a prompt accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptArgument {
    pub name: String,
    pub description: String,
    pub required: bool,
}

impl PromptArgument {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        PromptArgument { name: name.into(), description: description.into(), required: true }
    }

    pub fn optional(mut self) -> Self {
        self.required = false;
        self
    }

    pub fn to_json(&self) -> Json {
        Json::object([
            ("name", Json::from(self.name.clone())),
            ("description", Json::from(self.description.clone())),
            ("required", Json::from(self.required)),
        ])
    }

    pub fn from_json(value: &Json) -> Option<PromptArgument> {
        Some(PromptArgument {
            name: value.get("name")?.as_str()?.to_string(),
            description: value.get("description").and_then(Json::as_str).unwrap_or("").to_string(),
            required: value.get("required").and_then(Json::as_bool).unwrap_or(false),
        })
    }
}

/// A prompt as it appears in `prompts/list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptInfo {
    pub name: String,
    pub description: String,
    pub arguments: Vec<PromptArgument>,
}

impl PromptInfo {
    pub fn to_json(&self) -> Json {
        Json::object([
            ("name", Json::from(self.name.clone())),
            ("description", Json::from(self.description.clone())),
            ("arguments", Json::Array(self.arguments.iter().map(PromptArgument::to_json).collect())),
        ])
    }

    pub fn from_json(value: &Json) -> Option<PromptInfo> {
        Some(PromptInfo {
            name: value.get("name")?.as_str()?.to_string(),
            description: value.get("description").and_then(Json::as_str).unwrap_or("").to_string(),
            arguments: value
                .get("arguments")
                .and_then(Json::as_array)
                .unwrap_or(&[])
                .iter()
                .filter_map(PromptArgument::from_json)
                .collect(),
        })
    }
}

/// Who is speaking in a prompt message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }

    pub fn parse(value: &str) -> Option<Role> {
        match value {
            "user" => Some(Role::User),
            "assistant" => Some(Role::Assistant),
            _ => None,
        }
    }
}

/// One message of a rendered prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMessage {
    pub role: Role,
    pub content: Content,
}

impl PromptMessage {
    pub fn user(text: impl Into<String>) -> Self {
        PromptMessage { role: Role::User, content: Content::text(text) }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        PromptMessage { role: Role::Assistant, content: Content::text(text) }
    }

    pub fn to_json(&self) -> Json {
        Json::object([
            ("role", Json::from(self.role.as_str())),
            ("content", self.content.to_json()),
        ])
    }

    pub fn from_json(value: &Json) -> Option<PromptMessage> {
        Some(PromptMessage {
            role: Role::parse(value.get("role")?.as_str()?)?,
            content: Content::from_json(value.get("content")?)?,
        })
    }
}

/// What `initialize` answers with — the client's picture of the server.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerInfo {
    pub protocol_version: String,
    /// Left as raw JSON: capabilities grow every revision, and a client should
    /// pass through what it does not recognise rather than dropping it.
    pub capabilities: Json,
    pub server: Implementation,
    pub instructions: Option<String>,
}

impl ServerInfo {
    pub fn to_json(&self) -> Json {
        let mut pairs = vec![
            ("protocolVersion", Json::from(self.protocol_version.clone())),
            ("capabilities", self.capabilities.clone()),
            ("serverInfo", self.server.to_json()),
        ];
        if let Some(instructions) = &self.instructions {
            pairs.push(("instructions", Json::from(instructions.clone())));
        }
        Json::object(pairs)
    }

    pub fn from_json(value: &Json) -> Option<ServerInfo> {
        Some(ServerInfo {
            protocol_version: value.get("protocolVersion")?.as_str()?.to_string(),
            capabilities: value.get("capabilities").cloned().unwrap_or(Json::Null),
            server: Implementation::from_json(value.get("serverInfo")?)?,
            instructions: value.get("instructions").and_then(Json::as_str).map(str::to_string),
        })
    }

    /// Whether the server said it serves tools.
    pub fn supports(&self, capability: &str) -> bool {
        self.capabilities.get(capability).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_result_uses_the_content_array_the_specification_defines() {
        let result = ToolResult::text("22°C and clear");

        assert_eq!(
            result.to_json().to_string(),
            r#"{"content":[{"text":"22°C and clear","type":"text"}],"isError":false}"#
        );
    }

    #[test]
    fn a_failed_tool_result_sets_is_error() {
        let result = ToolResult::error("the upstream API is down");

        assert_eq!(result.to_json().get("isError").unwrap().as_bool(), Some(true));
        assert_eq!(ToolResult::from_json(&result.to_json()).unwrap(), result);
    }

    #[test]
    fn a_handler_returning_structured_json_becomes_readable_text() {
        let result = ToolResult::from_value(Json::object([("temp", Json::from(22))]));
        assert_eq!(result.text_content(), r#"{"temp":22}"#);

        // A plain string is handed over as-is, not quoted again.
        assert_eq!(ToolResult::from_value(Json::from("plain")).text_content(), "plain");
    }

    #[test]
    fn a_tool_descriptor_carries_name_description_and_input_schema() {
        let schema = Schema::object().string("city", "The city to look up");
        let info = ToolInfo::describe("weather", "Look up the weather", &schema);
        let json = info.to_json();

        assert_eq!(json.get("name").unwrap().as_str(), Some("weather"));
        assert_eq!(json.get("description").unwrap().as_str(), Some("Look up the weather"));
        assert_eq!(json.get("inputSchema.type").unwrap().as_str(), Some("object"));
        assert_eq!(ToolInfo::from_json(&json).unwrap(), info);
    }

    #[test]
    fn server_info_round_trips_through_the_handshake_shape() {
        let info = ServerInfo {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: Json::object([("tools", Json::Object(Default::default()))]),
            server: Implementation::new("shop", "1.2.3"),
            instructions: Some("Use `orders` to look up an order.".into()),
        };

        let decoded = ServerInfo::from_json(&info.to_json()).unwrap();
        assert_eq!(decoded, info);
        assert!(decoded.supports("tools"));
        assert!(!decoded.supports("prompts"));
    }

    #[test]
    fn prompt_messages_round_trip() {
        let message = PromptMessage::user("Summarize order 7");
        let json = message.to_json();

        assert_eq!(json.get("role").unwrap().as_str(), Some("user"));
        assert_eq!(json.get("content.type").unwrap().as_str(), Some("text"));
        assert_eq!(PromptMessage::from_json(&json).unwrap(), message);
    }

    #[test]
    fn the_protocol_version_is_the_one_this_crate_claims() {
        assert_eq!(PROTOCOL_VERSION, "2025-06-18");
        assert_eq!(SUPPORTED_VERSIONS[0], PROTOCOL_VERSION);
    }
}
