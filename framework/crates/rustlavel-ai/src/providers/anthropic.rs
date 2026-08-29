//! Anthropic's Messages API.
//!
//! `POST {base}/v1/messages`, authenticated with `x-api-key` rather than a
//! bearer token, and pinned to the `anthropic-version` the wire format below
//! was written against. The system prompt is a top-level field, not a message,
//! and streaming arrives as *named* server-sent events.

use crate::completion::{Completion, StopReason, ToolCall, Usage};
use crate::config::{ANTHROPIC_BASE_URL, ANTHROPIC_DEFAULT_MODEL, ApiKey};
use crate::message::{Content, Role};
use crate::provider::{BoxFuture, Decoder, Provider, StreamDelta, TextStream, record_call, token_count};
use crate::request::Request;
use rustlavel_client::Client;
use rustlavel_core::{Error, Json, Result};
use std::time::Instant;

/// The API version this translation was written for. Anthropic keeps old
/// versions working, so pinning is what keeps the parser honest.
pub const API_VERSION: &str = "2023-06-01";

/// Anthropic requires `max_tokens`, so an unset one has to become something.
const DEFAULT_MAX_TOKENS: u32 = 4096;

pub struct Anthropic {
    client: Client,
    key: ApiKey,
    base_url: String,
}

impl Anthropic {
    pub fn new(key: impl Into<ApiKey>) -> Anthropic {
        Anthropic {
            client: Client::new(),
            key: key.into(),
            base_url: ANTHROPIC_BASE_URL.to_string(),
        }
    }

    /// Use a specific client — a faked one in tests, a retrying one in
    /// production.
    pub fn client(mut self, client: Client) -> Anthropic {
        self.client = client;
        self
    }

    /// Point at a proxy or a gateway.
    pub fn base_url(mut self, base_url: impl Into<String>) -> Anthropic {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/messages", self.base_url)
    }

    fn post(&self, body: Json) -> rustlavel_client::RequestBuilder {
        self.client
            .post(self.endpoint())
            .header("x-api-key", self.key.expose())
            .header("anthropic-version", API_VERSION)
            .json(body)
    }
}

impl Provider for Anthropic {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn default_model(&self) -> &'static str {
        ANTHROPIC_DEFAULT_MODEL
    }

    fn complete<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move {
            let started = Instant::now();
            let response = self
                .post(body(request, false))
                .send()
                .await
                .and_then(rustlavel_client::ClientResponse::error_for_status)
                // Anthropic quotes the offending key back in some 401 bodies.
                .map_err(|error| self.key.scrub_error(error))?;

            let completion = parse(&response.json()?)?;
            record_call(self.name(), &completion.model, completion.usage, started.elapsed(), false);
            Ok(completion)
        })
    }

    fn stream<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<TextStream>> {
        Box::pin(async move {
            let body = self
                .post(body(request, true))
                .accept_events()
                .stream()
                .await
                .map_err(|error| self.key.scrub_error(error))?;

            Ok(TextStream::events(body, decode as Decoder).measured(self.name(), &request.model))
        })
    }
}

/// Build the request body.
pub fn body(request: &Request, stream: bool) -> Json {
    let mut object = vec![
        ("model", Json::from(request.model.as_str())),
        // Required by the API, unlike everywhere else.
        (
            "max_tokens",
            Json::from(request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        ),
        ("messages", Json::Array(request.dialogue().map(message).collect())),
    ];

    if let Some(system) = request.effective_system() {
        object.push(("system", Json::from(system)));
    }
    if let Some(temperature) = request.temperature {
        object.push(("temperature", Json::from(temperature)));
    }
    if !request.stop_sequences.is_empty() {
        object.push((
            "stop_sequences",
            Json::Array(request.stop_sequences.iter().map(|s| Json::from(s.as_str())).collect()),
        ));
    }
    if !request.tools.is_empty() {
        object.push((
            "tools",
            Json::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        Json::object([
                            ("name", Json::from(tool.name.as_str())),
                            ("description", Json::from(tool.description.as_str())),
                            ("input_schema", tool.schema_json()),
                        ])
                    })
                    .collect(),
            ),
        ));
    }
    if stream {
        object.push(("stream", Json::Bool(true)));
    }

    Json::object(object)
}

/// One message in Anthropic's shape.
///
/// Tool results are a `user` turn here, not a role of their own — the API only
/// knows `user` and `assistant`.
fn message(message: &crate::message::Message) -> Json {
    let role = match message.role {
        Role::Assistant => "assistant",
        _ => "user",
    };

    let content: Vec<Json> = message
        .content
        .iter()
        .map(|part| match part {
            Content::Text(text) => Json::object([
                ("type", Json::from("text")),
                ("text", Json::from(text.as_str())),
            ]),
            Content::ToolUse { id, name, input } => Json::object([
                ("type", Json::from("tool_use")),
                ("id", Json::from(id.as_str())),
                ("name", Json::from(name.as_str())),
                ("input", input.clone()),
            ]),
            Content::ToolResult { id, output, is_error, .. } => Json::object([
                ("type", Json::from("tool_result")),
                ("tool_use_id", Json::from(id.as_str())),
                ("content", Json::from(text_of(output))),
                ("is_error", Json::Bool(*is_error)),
            ]),
        })
        .collect();

    Json::object([("role", Json::from(role)), ("content", Json::Array(content))])
}

/// Tool results travel as text, so a JSON answer is serialised and a string
/// answer is sent as itself rather than as a quoted string.
fn text_of(value: &Json) -> String {
    match value {
        Json::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Parse a completed response.
pub fn parse(body: &Json) -> Result<Completion> {
    if let Some(message) = body.get("error.message").and_then(Json::as_str) {
        return Err(Error::msg(format!("anthropic: {message}")));
    }

    let content = body
        .get("content")
        .and_then(Json::as_array)
        .ok_or_else(|| Error::msg("anthropic: the response has no `content` block"))?;

    let mut text = String::new();
    let mut tool_calls = Vec::new();

    for part in content {
        match part.get("type").and_then(Json::as_str) {
            Some("text") => text.push_str(part.get("text").and_then(Json::as_str).unwrap_or("")),
            Some("tool_use") => tool_calls.push(ToolCall::new(
                part.get("id").and_then(Json::as_str).unwrap_or_default(),
                part.get("name").and_then(Json::as_str).unwrap_or_default(),
                part.get("input").cloned().unwrap_or(Json::Null),
            )),
            // Thinking blocks and anything added later are skipped rather than
            // treated as an error: a new block type is not a failure.
            _ => {}
        }
    }

    Ok(Completion {
        text,
        tool_calls,
        stop_reason: body
            .get("stop_reason")
            .and_then(Json::as_str)
            .map_or(StopReason::EndTurn, StopReason::parse),
        usage: Usage::new(
            token_count(body, "usage.input_tokens"),
            token_count(body, "usage.output_tokens"),
        ),
        model: body.get("model").and_then(Json::as_str).unwrap_or_default().to_string(),
    })
}

/// Decode one server-sent event from a streamed message.
///
/// The events are named, and only two of them matter to a text stream:
/// `content_block_delta` carries the words, `message_delta` carries the final
/// output token count and the stop reason.
pub fn decode(name: Option<&str>, data: &str) -> Result<StreamDelta> {
    let value = Json::parse(data)?;
    let name = name.or_else(|| value.get("type").and_then(Json::as_str));

    Ok(match name {
        Some("content_block_delta") => match value.get("delta.type").and_then(Json::as_str) {
            // `input_json_delta` streams a tool's arguments; a text stream has
            // nothing useful to show for a half-built argument object.
            Some("input_json_delta") => StreamDelta::nothing(),
            _ => StreamDelta::text(value.get("delta.text").and_then(Json::as_str).unwrap_or("")),
        },
        Some("message_start") => StreamDelta::nothing().with_usage(Usage::new(
            token_count(&value, "message.usage.input_tokens"),
            token_count(&value, "message.usage.output_tokens"),
        )),
        Some("message_delta") => {
            let mut delta = StreamDelta::nothing()
                .with_usage(Usage::new(0, token_count(&value, "usage.output_tokens")));
            if let Some(reason) = value.get("delta.stop_reason").and_then(Json::as_str) {
                delta = delta.with_stop_reason(StopReason::parse(reason));
            }
            delta
        }
        Some("message_stop") => StreamDelta::done(),
        Some("error") => {
            let message = value.get("error.message").and_then(Json::as_str).unwrap_or("unknown");
            return Err(Error::msg(format!("anthropic stream: {message}")));
        }
        _ => StreamDelta::nothing(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use crate::tool::Tool;
    use rustlavel_client::fake::{Fake, FakeResponse};

    fn answer() -> Json {
        Json::parse(
            r#"{
                "id": "msg_01",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-5",
                "content": [{"type": "text", "text": "The sky scatters blue light."}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 14, "output_tokens": 9}
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn the_system_prompt_is_a_field_rather_than_a_message() {
        let request = Request::new("claude-sonnet-5")
            .system("Be brief.")
            .message(Message::system("Answer in English."))
            .user("Why is the sky blue?")
            .temperature(0.2)
            .max_tokens(256)
            .stop("END");

        let body = body(&request, false);

        assert_eq!(body.get("model").unwrap().as_str(), Some("claude-sonnet-5"));
        assert_eq!(body.get("system").unwrap().as_str(), Some("Be brief.\n\nAnswer in English."));
        assert_eq!(body.get("max_tokens").unwrap().as_i64(), Some(256));
        assert_eq!(body.get("temperature").unwrap().as_f64(), Some(0.2));
        assert_eq!(body.get("stop_sequences.0").unwrap().as_str(), Some("END"));
        assert!(body.get("stream").is_none());

        // Only the dialogue is in `messages`.
        let messages = body.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].get("role").unwrap().as_str(), Some("user"));
        assert_eq!(messages[0].get("content.0.type").unwrap().as_str(), Some("text"));
        assert_eq!(
            messages[0].get("content.0.text").unwrap().as_str(),
            Some("Why is the sky blue?")
        );
    }

    #[test]
    fn max_tokens_is_supplied_because_the_api_insists_on_it() {
        let body = body(&Request::new("claude-sonnet-5").user("Hi"), false);
        assert_eq!(body.get("max_tokens").unwrap().as_i64(), Some(4096));
    }

    #[test]
    fn tools_become_an_input_schema() {
        let request = Request::new("claude-sonnet-5")
            .user("Weather?")
            .tool(Tool::new("get_weather", "Look up the weather").string("city", "The city"));

        let body = body(&request, false);

        assert_eq!(body.get("tools.0.name").unwrap().as_str(), Some("get_weather"));
        assert_eq!(body.get("tools.0.description").unwrap().as_str(), Some("Look up the weather"));
        assert_eq!(
            body.get("tools.0.input_schema.properties.city.type").unwrap().as_str(),
            Some("string")
        );
    }

    #[test]
    fn a_tool_result_is_replayed_as_a_user_turn() {
        let request = Request::new("claude-sonnet-5")
            .user("Weather?")
            .message(
                Message::assistant("Checking.").with(Content::ToolUse {
                    id: "toolu_1".into(),
                    name: "get_weather".into(),
                    input: Json::object([("city", "Oslo".into())]),
                }),
            )
            .message(Message::tool_result(
                "toolu_1",
                "get_weather",
                Json::object([("degrees", 7.into())]),
            ));

        let body = body(&request, false);
        let messages = body.get("messages").unwrap().as_array().unwrap();

        assert_eq!(messages[1].get("role").unwrap().as_str(), Some("assistant"));
        assert_eq!(messages[1].get("content.1.type").unwrap().as_str(), Some("tool_use"));
        assert_eq!(messages[1].get("content.1.input.city").unwrap().as_str(), Some("Oslo"));

        assert_eq!(messages[2].get("role").unwrap().as_str(), Some("user"));
        assert_eq!(messages[2].get("content.0.type").unwrap().as_str(), Some("tool_result"));
        assert_eq!(messages[2].get("content.0.tool_use_id").unwrap().as_str(), Some("toolu_1"));
        assert_eq!(
            messages[2].get("content.0.content").unwrap().as_str(),
            Some("{\"degrees\":7}")
        );
        assert_eq!(messages[2].get("content.0.is_error").unwrap().as_bool(), Some(false));
    }

    #[test]
    fn parses_text_usage_and_the_stop_reason() {
        let completion = parse(&answer()).unwrap();

        assert_eq!(completion.text, "The sky scatters blue light.");
        assert_eq!(completion.model, "claude-sonnet-5");
        assert_eq!(completion.stop_reason, StopReason::EndTurn);
        assert_eq!(completion.usage, Usage::new(14, 9));
        assert!(!completion.wants_tools());
    }

    #[test]
    fn parses_a_tool_call_and_skips_block_types_it_does_not_know() {
        let body = Json::parse(
            r#"{
                "model": "claude-sonnet-5",
                "content": [
                    {"type": "thinking", "thinking": "hidden"},
                    {"type": "text", "text": "Let me look."},
                    {"type": "tool_use", "id": "toolu_9", "name": "get_weather",
                     "input": {"city": "Oslo"}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 20, "output_tokens": 30}
            }"#,
        )
        .unwrap();

        let completion = parse(&body).unwrap();

        assert_eq!(completion.text, "Let me look.");
        assert_eq!(completion.stop_reason, StopReason::ToolUse);
        assert_eq!(completion.tool_calls.len(), 1);
        assert_eq!(completion.tool_calls[0].id, "toolu_9");
        assert_eq!(completion.tool_calls[0].argument("city").unwrap().as_str(), Some("Oslo"));
        assert_eq!(completion.usage.total(), 50);
    }

    #[test]
    fn an_error_payload_becomes_an_error() {
        let body = Json::parse(r#"{"type":"error","error":{"message":"overloaded"}}"#).unwrap();
        assert!(parse(&body).unwrap_err().to_string().contains("overloaded"));
    }

    #[test]
    fn named_events_are_decoded_and_bookkeeping_is_ignored() {
        assert_eq!(
            decode(Some("content_block_delta"), r#"{"delta":{"type":"text_delta","text":"Hi"}}"#)
                .unwrap(),
            StreamDelta::text("Hi")
        );
        assert_eq!(
            decode(Some("content_block_delta"), r#"{"delta":{"type":"input_json_delta"}}"#).unwrap(),
            StreamDelta::nothing()
        );
        assert_eq!(
            decode(Some("message_start"), r#"{"message":{"usage":{"input_tokens":11}}}"#)
                .unwrap()
                .usage,
            Some(Usage::new(11, 0))
        );
        assert!(decode(Some("message_stop"), "{}").unwrap().done);
        assert!(decode(Some("ping"), "{}").unwrap().text.is_none());

        let final_delta =
            decode(Some("message_delta"), r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#)
                .unwrap();
        assert_eq!(final_delta.usage, Some(Usage::new(0, 42)));
        assert_eq!(final_delta.stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn a_streamed_error_frame_stops_the_stream() {
        let error = decode(Some("error"), r#"{"error":{"message":"overloaded"}}"#).unwrap_err();
        assert!(error.to_string().contains("overloaded"));
    }

    #[tokio::test]
    async fn completes_over_a_faked_socket_with_the_right_headers() {
        let provider = Anthropic::new("sk-ant-test")
            .client(Client::new().faking(
                Fake::new().on("api.anthropic.com/v1/messages", FakeResponse::json(answer())),
            ));

        let completion = provider
            .complete(&Request::new("claude-sonnet-5").system("Be brief.").user("Why?"))
            .await
            .unwrap();

        assert_eq!(completion.text, "The sky scatters blue light.");

        let sent = &provider.client.fake().unwrap().recorded()[0];
        assert_eq!(sent.url, "https://api.anthropic.com/v1/messages");
        assert_eq!(sent.headers.get("x-api-key"), Some("sk-ant-test"));
        assert_eq!(sent.headers.get("anthropic-version"), Some("2023-06-01"));
        assert_eq!(sent.headers.get("content-type"), Some("application/json"));
        assert_eq!(sent.json().unwrap().get("system").unwrap().as_str(), Some("Be brief."));
    }

    #[tokio::test]
    async fn assembles_a_streamed_answer_from_named_events() {
        let events = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":14}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"The sky \"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"is blue.\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":9}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );

        let provider = Anthropic::new("sk-ant-test").client(
            Client::new().faking(Fake::new().fallback(FakeResponse::text(events))),
        );

        let request = Request::new("claude-sonnet-5").user("Why is the sky blue?");
        let mut stream = provider.stream(&request).await.unwrap();

        assert_eq!(stream.next().await.unwrap().as_deref(), Some("The sky "));
        assert_eq!(stream.next().await.unwrap().as_deref(), Some("is blue."));
        assert!(stream.next().await.unwrap().is_none());
        assert_eq!(stream.usage(), Usage::new(14, 9));
        assert_eq!(stream.stop_reason(), Some(&StopReason::EndTurn));

        let sent = &provider.client.fake().unwrap().recorded()[0];
        assert_eq!(sent.json().unwrap().get("stream").unwrap().as_bool(), Some(true));
        assert_eq!(sent.headers.get("accept"), Some("text/event-stream"));
    }

    #[tokio::test]
    async fn a_key_echoed_back_in_an_error_body_is_scrubbed() {
        let provider = Anthropic::new("sk-ant-super-secret").client(
            Client::new().faking(Fake::new().fallback(
                FakeResponse::text("{\"error\":\"invalid key sk-ant-super-secret\"}").status(401),
            )),
        );

        let error = provider
            .complete(&Request::new("claude-sonnet-5").user("Hi"))
            .await
            .unwrap_err()
            .to_string();

        assert!(!error.contains("sk-ant-super-secret"), "{error}");
        assert!(error.contains("[redacted]"), "{error}");
        assert!(error.contains("401"), "{error}");
    }
}
