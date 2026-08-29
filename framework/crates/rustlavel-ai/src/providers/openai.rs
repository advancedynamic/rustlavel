//! OpenAI's chat completions API.
//!
//! `POST {base}/v1/chat/completions` with a bearer token. The system prompt is
//! just the first message, tool arguments arrive as a JSON *string* that has to
//! be parsed again, and the stream is anonymous SSE frames ending in the
//! literal `[DONE]`.

use crate::completion::{Completion, StopReason, ToolCall, Usage};
use crate::config::{ApiKey, OPENAI_BASE_URL, OPENAI_DEFAULT_MODEL};
use crate::message::{Content, Role};
use crate::provider::{BoxFuture, Decoder, Provider, StreamDelta, TextStream, record_call, token_count};
use crate::request::Request;
use rustlavel_client::Client;
use rustlavel_core::{Error, Json, Result};
use std::time::Instant;

pub struct OpenAi {
    client: Client,
    key: ApiKey,
    base_url: String,
}

impl OpenAi {
    pub fn new(key: impl Into<ApiKey>) -> OpenAi {
        OpenAi { client: Client::new(), key: key.into(), base_url: OPENAI_BASE_URL.to_string() }
    }

    pub fn client(mut self, client: Client) -> OpenAi {
        self.client = client;
        self
    }

    /// Point at a proxy, a gateway, or one of the many OpenAI-compatible APIs.
    pub fn base_url(mut self, base_url: impl Into<String>) -> OpenAi {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url)
    }

    fn post(&self, body: Json) -> rustlavel_client::RequestBuilder {
        self.client.post(self.endpoint()).bearer(self.key.expose()).json(body)
    }
}

impl Provider for OpenAi {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn default_model(&self) -> &'static str {
        OPENAI_DEFAULT_MODEL
    }

    fn complete<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move {
            let started = Instant::now();
            let response = self
                .post(body(request, false))
                .send()
                .await
                .and_then(rustlavel_client::ClientResponse::error_for_status)
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
    let mut messages: Vec<Json> = Vec::new();
    if let Some(system) = request.effective_system() {
        // Not a field here: the instructions are the first message.
        messages.push(Json::object([
            ("role", Json::from("system")),
            ("content", Json::from(system)),
        ]));
    }
    messages.extend(request.dialogue().map(message));

    let mut object = vec![
        ("model", Json::from(request.model.as_str())),
        ("messages", Json::Array(messages)),
    ];

    if let Some(temperature) = request.temperature {
        object.push(("temperature", Json::from(temperature)));
    }
    if let Some(max_tokens) = request.max_tokens {
        object.push(("max_tokens", Json::from(max_tokens)));
    }
    if !request.stop_sequences.is_empty() {
        object.push((
            "stop",
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
                            ("type", Json::from("function")),
                            (
                                "function",
                                Json::object([
                                    ("name", Json::from(tool.name.as_str())),
                                    ("description", Json::from(tool.description.as_str())),
                                    ("parameters", tool.schema_json()),
                                ]),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ));
    }
    if stream {
        object.push(("stream", Json::Bool(true)));
        // Without this the stream reports no tokens at all, and an AI call
        // nobody can cost is an AI call nobody can budget for.
        object.push((
            "stream_options",
            Json::object([("include_usage", Json::Bool(true))]),
        ));
    }

    Json::object(object)
}

fn message(message: &crate::message::Message) -> Json {
    // A tool result is its own role here, carrying the id it answers.
    if message.role == Role::Tool
        && let Some(Content::ToolResult { id, output, .. }) = message.content.first()
    {
        return Json::object([
            ("role", Json::from("tool")),
            ("tool_call_id", Json::from(id.as_str())),
            ("content", Json::from(text_of(output))),
        ]);
    }

    let role = match message.role {
        Role::Assistant => "assistant",
        Role::System => "system",
        _ => "user",
    };

    let mut object = vec![
        ("role", Json::from(role)),
        ("content", Json::from(message.text())),
    ];

    let tool_calls: Vec<Json> = message
        .tool_uses()
        .map(|(id, name, input)| {
            Json::object([
                ("id", Json::from(id)),
                ("type", Json::from("function")),
                (
                    "function",
                    Json::object([
                        ("name", Json::from(name)),
                        // Arguments are a string on this wire, in both directions.
                        ("arguments", Json::from(input.to_string())),
                    ]),
                ),
            ])
        })
        .collect();

    if !tool_calls.is_empty() {
        object.push(("tool_calls", Json::Array(tool_calls)));
    }

    Json::object(object)
}

fn text_of(value: &Json) -> String {
    match value {
        Json::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Parse a completed response.
pub fn parse(body: &Json) -> Result<Completion> {
    if let Some(message) = body.get("error.message").and_then(Json::as_str) {
        return Err(Error::msg(format!("openai: {message}")));
    }

    let choice = body
        .get("choices.0")
        .ok_or_else(|| Error::msg("openai: the response has no choices"))?;

    let text = choice.get("message.content").and_then(Json::as_str).unwrap_or("").to_string();

    let mut tool_calls = Vec::new();
    if let Some(calls) = choice.get("message.tool_calls").and_then(Json::as_array) {
        for call in calls {
            let name = call.get("function.name").and_then(Json::as_str).unwrap_or_default();
            tool_calls.push(ToolCall::new(
                call.get("id").and_then(Json::as_str).unwrap_or_default(),
                name,
                parse_arguments(call.get("function.arguments"), name)?,
            ));
        }
    }

    Ok(Completion {
        text,
        tool_calls,
        stop_reason: choice
            .get("finish_reason")
            .and_then(Json::as_str)
            .map_or(StopReason::EndTurn, StopReason::parse),
        usage: Usage::new(
            token_count(body, "usage.prompt_tokens"),
            token_count(body, "usage.completion_tokens"),
        ),
        model: body.get("model").and_then(Json::as_str).unwrap_or_default().to_string(),
    })
}

/// Tool arguments arrive as a string of JSON, so they need parsing twice.
fn parse_arguments(value: Option<&Json>, tool: &str) -> Result<Json> {
    match value {
        Some(Json::String(text)) if text.trim().is_empty() => Ok(Json::object::<&str, _>([])),
        Some(Json::String(text)) => Json::parse(text).map_err(|error| {
            Error::msg(format!("openai: the arguments for `{tool}` are not valid JSON: {error}"))
        }),
        // Some compatible servers send an object rather than a string.
        Some(other) => Ok(other.clone()),
        None => Ok(Json::object::<&str, _>([])),
    }
}

/// Decode one frame of a streamed answer.
pub fn decode(_name: Option<&str>, data: &str) -> Result<StreamDelta> {
    if data.trim() == "[DONE]" {
        return Ok(StreamDelta::done());
    }

    let value = Json::parse(data)?;
    if let Some(message) = value.get("error.message").and_then(Json::as_str) {
        return Err(Error::msg(format!("openai stream: {message}")));
    }

    let mut delta = StreamDelta::text(
        value.get("choices.0.delta.content").and_then(Json::as_str).unwrap_or(""),
    );

    // The usage frame arrives after the last choice, with an empty choices list.
    if value.get("usage").is_some_and(|usage| !usage.is_null()) {
        delta = delta.with_usage(Usage::new(
            token_count(&value, "usage.prompt_tokens"),
            token_count(&value, "usage.completion_tokens"),
        ));
    }
    if let Some(reason) = value.get("choices.0.finish_reason").and_then(Json::as_str) {
        delta = delta.with_stop_reason(StopReason::parse(reason));
    }

    Ok(delta)
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
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "model": "gpt-4o-mini",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "Rayleigh scattering."},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 14, "completion_tokens": 9, "total_tokens": 23}
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn the_system_prompt_is_the_first_message() {
        let request = Request::new("gpt-4o-mini")
            .system("Be brief.")
            .user("Why is the sky blue?")
            .temperature(0.2)
            .max_tokens(256)
            .stop("END");

        let body = body(&request, false);
        let messages = body.get("messages").unwrap().as_array().unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].get("role").unwrap().as_str(), Some("system"));
        assert_eq!(messages[0].get("content").unwrap().as_str(), Some("Be brief."));
        assert_eq!(messages[1].get("role").unwrap().as_str(), Some("user"));
        assert_eq!(body.get("temperature").unwrap().as_f64(), Some(0.2));
        assert_eq!(body.get("max_tokens").unwrap().as_i64(), Some(256));
        assert_eq!(body.get("stop.0").unwrap().as_str(), Some("END"));
    }

    #[test]
    fn tools_are_wrapped_in_a_function_envelope() {
        let request = Request::new("gpt-4o-mini")
            .user("Weather?")
            .tool(Tool::new("get_weather", "Look up the weather").string("city", "The city"));

        let body = body(&request, false);

        assert_eq!(body.get("tools.0.type").unwrap().as_str(), Some("function"));
        assert_eq!(body.get("tools.0.function.name").unwrap().as_str(), Some("get_weather"));
        assert_eq!(
            body.get("tools.0.function.parameters.properties.city.type").unwrap().as_str(),
            Some("string")
        );
    }

    #[test]
    fn a_tool_result_is_replayed_as_a_tool_role_message() {
        let request = Request::new("gpt-4o-mini")
            .user("Weather?")
            .message(Message::assistant("").with(Content::ToolUse {
                id: "call_1".into(),
                name: "get_weather".into(),
                input: Json::object([("city", "Oslo".into())]),
            }))
            .message(Message::tool_result(
                "call_1",
                "get_weather",
                Json::object([("degrees", 7.into())]),
            ));

        let body = body(&request, false);
        let messages = body.get("messages").unwrap().as_array().unwrap();

        assert_eq!(messages[1].get("role").unwrap().as_str(), Some("assistant"));
        assert_eq!(messages[1].get("tool_calls.0.id").unwrap().as_str(), Some("call_1"));
        // The arguments go back out as a string, exactly as they came in.
        assert_eq!(
            messages[1].get("tool_calls.0.function.arguments").unwrap().as_str(),
            Some("{\"city\":\"Oslo\"}")
        );

        assert_eq!(messages[2].get("role").unwrap().as_str(), Some("tool"));
        assert_eq!(messages[2].get("tool_call_id").unwrap().as_str(), Some("call_1"));
        assert_eq!(messages[2].get("content").unwrap().as_str(), Some("{\"degrees\":7}"));
    }

    #[test]
    fn streaming_asks_for_the_token_counts_too() {
        let body = body(&Request::new("gpt-4o-mini").user("Hi"), true);

        assert_eq!(body.get("stream").unwrap().as_bool(), Some(true));
        assert_eq!(body.get("stream_options.include_usage").unwrap().as_bool(), Some(true));
    }

    #[test]
    fn parses_text_usage_and_the_finish_reason() {
        let completion = parse(&answer()).unwrap();

        assert_eq!(completion.text, "Rayleigh scattering.");
        assert_eq!(completion.model, "gpt-4o-mini");
        assert_eq!(completion.stop_reason, StopReason::EndTurn);
        assert_eq!(completion.usage, Usage::new(14, 9));
    }

    #[test]
    fn parses_a_tool_call_whose_arguments_are_a_string_of_json() {
        let body = Json::parse(
            r#"{
                "model": "gpt-4o-mini",
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_9",
                            "type": "function",
                            "function": {"name": "get_weather",
                                         "arguments": "{\"city\": \"Oslo\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 20, "completion_tokens": 30}
            }"#,
        )
        .unwrap();

        let completion = parse(&body).unwrap();

        assert_eq!(completion.text, "");
        assert_eq!(completion.stop_reason, StopReason::ToolUse);
        assert_eq!(completion.tool_calls[0].id, "call_9");
        assert_eq!(completion.tool_calls[0].argument("city").unwrap().as_str(), Some("Oslo"));
    }

    #[test]
    fn unparseable_tool_arguments_name_the_tool() {
        let error = parse_arguments(Some(&Json::from("{not json")), "get_weather").unwrap_err();
        assert!(error.to_string().contains("get_weather"), "{error}");

        // Empty and absent arguments are an empty object, not a failure.
        assert_eq!(parse_arguments(Some(&Json::from("")), "t").unwrap(), Json::object::<&str, _>([]));
        assert_eq!(parse_arguments(None, "t").unwrap(), Json::object::<&str, _>([]));
    }

    #[test]
    fn an_error_payload_becomes_an_error() {
        let body = Json::parse(r#"{"error":{"message":"rate limited"}}"#).unwrap();
        assert!(parse(&body).unwrap_err().to_string().contains("rate limited"));
    }

    #[test]
    fn the_done_marker_ends_the_stream() {
        assert!(decode(None, "[DONE]").unwrap().done);
        assert_eq!(
            decode(None, r#"{"choices":[{"delta":{"content":"Hi"}}]}"#).unwrap().text.as_deref(),
            Some("Hi")
        );
        assert!(decode(None, r#"{"choices":[{"delta":{"role":"assistant"}}]}"#).unwrap().text.as_deref() == Some(""));
    }

    #[tokio::test]
    async fn completes_over_a_faked_socket_with_bearer_auth() {
        let provider = OpenAi::new("sk-openai-test").client(Client::new().faking(
            Fake::new().on("api.openai.com/v1/chat/completions", FakeResponse::json(answer())),
        ));

        let completion =
            provider.complete(&Request::new("gpt-4o-mini").user("Why?")).await.unwrap();

        assert_eq!(completion.text, "Rayleigh scattering.");

        let sent = &provider.client.fake().unwrap().recorded()[0];
        assert_eq!(sent.url, "https://api.openai.com/v1/chat/completions");
        assert_eq!(sent.headers.get("authorization"), Some("Bearer sk-openai-test"));
    }

    #[tokio::test]
    async fn assembles_a_streamed_answer_and_its_usage() {
        let events = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Rayleigh \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"scattering.\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":14,\"completion_tokens\":9}}\n\n",
            "data: [DONE]\n\n",
        );

        let provider = OpenAi::new("sk-openai-test")
            .client(Client::new().faking(Fake::new().fallback(FakeResponse::text(events))));

        let request = Request::new("gpt-4o-mini").user("Why is the sky blue?");
        let mut stream = provider.stream(&request).await.unwrap();

        assert_eq!(stream.collect().await.unwrap(), "Rayleigh scattering.");
        assert_eq!(stream.usage(), Usage::new(14, 9));
        assert_eq!(stream.stop_reason(), Some(&StopReason::EndTurn));
    }

    #[tokio::test]
    async fn a_key_echoed_back_in_an_error_body_is_scrubbed() {
        let provider = OpenAi::new("sk-openai-super-secret").client(Client::new().faking(
            Fake::new().fallback(
                FakeResponse::text("{\"error\":\"bad key sk-openai-super-secret\"}").status(401),
            ),
        ));

        let error = provider
            .complete(&Request::new("gpt-4o-mini").user("Hi"))
            .await
            .unwrap_err()
            .to_string();

        assert!(!error.contains("super-secret"), "{error}");
        assert!(error.contains("[redacted]"), "{error}");
    }
}
