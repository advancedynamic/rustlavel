//! Ollama, for models running on the developer's own machine.
//!
//! `POST {base}/api/chat` with no authentication at all. Sampling settings live
//! under `options`, tool calls come back with no id of their own, and a stream
//! is newline-delimited JSON objects rather than server-sent events.

use crate::completion::{Completion, StopReason, ToolCall, Usage};
use crate::config::{OLLAMA_BASE_URL, OLLAMA_DEFAULT_MODEL};
use crate::message::{Content, Role};
use crate::provider::{BoxFuture, Decoder, Provider, StreamDelta, TextStream, record_call, token_count};
use crate::request::Request;
use rustlavel_client::Client;
use rustlavel_core::{Error, Json, Result};
use std::time::Instant;

pub struct Ollama {
    client: Client,
    base_url: String,
}

impl Default for Ollama {
    fn default() -> Ollama {
        Ollama { client: Client::new(), base_url: OLLAMA_BASE_URL.to_string() }
    }
}

impl Ollama {
    pub fn new() -> Ollama {
        Ollama::default()
    }

    pub fn client(mut self, client: Client) -> Ollama {
        self.client = client;
        self
    }

    /// Point at another machine on the network.
    pub fn base_url(mut self, base_url: impl Into<String>) -> Ollama {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/api/chat", self.base_url)
    }
}

impl Provider for Ollama {
    fn name(&self) -> &'static str {
        "ollama"
    }

    fn default_model(&self) -> &'static str {
        OLLAMA_DEFAULT_MODEL
    }

    fn complete<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move {
            let started = Instant::now();
            let response = self
                .client
                .post(self.endpoint())
                .json(body(request, false))
                .send()
                .await
                .and_then(rustlavel_client::ClientResponse::error_for_status)?;

            let completion = parse(&response.json()?)?;
            record_call(self.name(), &completion.model, completion.usage, started.elapsed(), false);
            Ok(completion)
        })
    }

    fn stream<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<TextStream>> {
        Box::pin(async move {
            let body = self.client.post(self.endpoint()).json(body(request, true)).stream().await?;

            Ok(TextStream::lines(body, decode as Decoder).measured(self.name(), &request.model))
        })
    }
}

/// Build the request body.
pub fn body(request: &Request, stream: bool) -> Json {
    let mut messages: Vec<Json> = Vec::new();
    if let Some(system) = request.effective_system() {
        messages.push(Json::object([
            ("role", Json::from("system")),
            ("content", Json::from(system)),
        ]));
    }
    messages.extend(request.dialogue().map(message));

    let mut object = vec![
        ("model", Json::from(request.model.as_str())),
        ("messages", Json::Array(messages)),
        // Ollama streams by default, so the non-streaming case has to say so.
        ("stream", Json::Bool(stream)),
    ];

    // Sampling settings are nested under `options` here rather than sitting at
    // the top level, and an empty `options` upsets older builds.
    let mut options: Vec<(&str, Json)> = Vec::new();
    if let Some(temperature) = request.temperature {
        options.push(("temperature", Json::from(temperature)));
    }
    if let Some(max_tokens) = request.max_tokens {
        options.push(("num_predict", Json::from(max_tokens)));
    }
    if !request.stop_sequences.is_empty() {
        options.push((
            "stop",
            Json::Array(request.stop_sequences.iter().map(|s| Json::from(s.as_str())).collect()),
        ));
    }
    if !options.is_empty() {
        object.push(("options", Json::object(options)));
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

    Json::object(object)
}

fn message(message: &crate::message::Message) -> Json {
    if message.role == Role::Tool
        && let Some(Content::ToolResult { output, .. }) = message.content.first()
    {
        return Json::object([
            ("role", Json::from("tool")),
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
        .map(|(_, name, input)| {
            Json::object([(
                "function",
                Json::object([
                    ("name", Json::from(name)),
                    // An object rather than a string, unlike OpenAI.
                    ("arguments", input.clone()),
                ]),
            )])
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
    if let Some(message) = body.get("error").and_then(Json::as_str) {
        return Err(Error::msg(format!("ollama: {message}")));
    }

    let text = body.get("message.content").and_then(Json::as_str).unwrap_or("").to_string();

    let mut tool_calls = Vec::new();
    if let Some(calls) = body.get("message.tool_calls").and_then(Json::as_array) {
        for (index, call) in calls.iter().enumerate() {
            // Ollama gives tool calls no id, so one is invented. It only has to
            // be stable within this conversation for the results to match up.
            tool_calls.push(ToolCall::new(
                format!("call_{index}"),
                call.get("function.name").and_then(Json::as_str).unwrap_or_default(),
                call.get("function.arguments").cloned().unwrap_or(Json::Null),
            ));
        }
    }

    let stop_reason = if !tool_calls.is_empty() {
        StopReason::ToolUse
    } else {
        body.get("done_reason")
            .and_then(Json::as_str)
            .map_or(StopReason::EndTurn, StopReason::parse)
    };

    Ok(Completion {
        text,
        tool_calls,
        stop_reason,
        usage: Usage::new(
            token_count(body, "prompt_eval_count"),
            token_count(body, "eval_count"),
        ),
        model: body.get("model").and_then(Json::as_str).unwrap_or_default().to_string(),
    })
}

/// Decode one line of a streamed answer.
pub fn decode(_name: Option<&str>, line: &str) -> Result<StreamDelta> {
    let value = Json::parse(line)?;
    if let Some(message) = value.get("error").and_then(Json::as_str) {
        return Err(Error::msg(format!("ollama stream: {message}")));
    }

    let mut delta =
        StreamDelta::text(value.get("message.content").and_then(Json::as_str).unwrap_or(""));

    // The last object repeats the counts for the whole call and says `done`.
    if value.get("done").and_then(Json::as_bool) == Some(true) {
        delta.done = true;
        delta = delta.with_usage(Usage::new(
            token_count(&value, "prompt_eval_count"),
            token_count(&value, "eval_count"),
        ));
        if let Some(reason) = value.get("done_reason").and_then(Json::as_str) {
            delta = delta.with_stop_reason(StopReason::parse(reason));
        }
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
                "model": "llama3.2",
                "created_at": "2026-08-29T10:00:00Z",
                "message": {"role": "assistant", "content": "Rayleigh scattering."},
                "done": true,
                "done_reason": "stop",
                "prompt_eval_count": 14,
                "eval_count": 9
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn sampling_settings_are_nested_under_options() {
        let request = Request::new("llama3.2")
            .system("Be brief.")
            .user("Why is the sky blue?")
            .temperature(0.2)
            .max_tokens(256)
            .stop("END");

        let body = body(&request, false);

        assert_eq!(body.get("stream").unwrap().as_bool(), Some(false));
        assert_eq!(body.get("options.temperature").unwrap().as_f64(), Some(0.2));
        assert_eq!(body.get("options.num_predict").unwrap().as_i64(), Some(256));
        assert_eq!(body.get("options.stop.0").unwrap().as_str(), Some("END"));
        assert_eq!(body.get("messages.0.role").unwrap().as_str(), Some("system"));
        assert_eq!(body.get("messages.1.content").unwrap().as_str(), Some("Why is the sky blue?"));
    }

    #[test]
    fn a_request_without_sampling_settings_sends_no_options_at_all() {
        assert!(body(&Request::new("llama3.2").user("Hi"), true).get("options").is_none());
    }

    #[test]
    fn tool_arguments_go_out_as_an_object_rather_than_a_string() {
        let request = Request::new("llama3.2")
            .user("Weather?")
            .tool(Tool::new("get_weather", "Look up the weather").string("city", "The city"))
            .message(Message::assistant("").with(Content::ToolUse {
                id: "call_0".into(),
                name: "get_weather".into(),
                input: Json::object([("city", "Oslo".into())]),
            }))
            .message(Message::tool_result("call_0", "get_weather", Json::from("7 degrees")));

        let body = body(&request, false);

        assert_eq!(body.get("tools.0.function.name").unwrap().as_str(), Some("get_weather"));
        assert_eq!(
            body.get("messages.1.tool_calls.0.function.arguments.city").unwrap().as_str(),
            Some("Oslo")
        );
        assert_eq!(body.get("messages.2.role").unwrap().as_str(), Some("tool"));
        assert_eq!(body.get("messages.2.content").unwrap().as_str(), Some("7 degrees"));
    }

    #[test]
    fn parses_text_usage_and_the_done_reason() {
        let completion = parse(&answer()).unwrap();

        assert_eq!(completion.text, "Rayleigh scattering.");
        assert_eq!(completion.model, "llama3.2");
        assert_eq!(completion.stop_reason, StopReason::EndTurn);
        assert_eq!(completion.usage, Usage::new(14, 9));
    }

    #[test]
    fn tool_calls_without_ids_are_given_stable_ones() {
        let body = Json::parse(
            r#"{
                "model": "llama3.2",
                "message": {"role": "assistant", "content": "",
                    "tool_calls": [
                        {"function": {"name": "get_weather", "arguments": {"city": "Oslo"}}},
                        {"function": {"name": "get_tides", "arguments": {"port": "Bergen"}}}
                    ]},
                "done": true,
                "prompt_eval_count": 20,
                "eval_count": 30
            }"#,
        )
        .unwrap();

        let completion = parse(&body).unwrap();

        assert_eq!(completion.stop_reason, StopReason::ToolUse);
        assert_eq!(completion.tool_calls[0].id, "call_0");
        assert_eq!(completion.tool_calls[1].id, "call_1");
        assert_eq!(completion.tool_calls[0].argument("city").unwrap().as_str(), Some("Oslo"));
    }

    #[test]
    fn an_error_payload_becomes_an_error() {
        let body = Json::parse(r#"{"error":"model \"llama3.2\" not found"}"#).unwrap();
        assert!(parse(&body).unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn completes_over_a_faked_socket_without_any_authentication() {
        let provider = Ollama::new().client(Client::new().faking(
            Fake::new().on("localhost:11434/api/chat", FakeResponse::json(answer())),
        ));

        let completion = provider.complete(&Request::new("llama3.2").user("Why?")).await.unwrap();

        assert_eq!(completion.text, "Rayleigh scattering.");

        let sent = &provider.client.fake().unwrap().recorded()[0];
        assert_eq!(sent.url, "http://localhost:11434/api/chat");
        assert!(sent.headers.get("authorization").is_none());
        assert_eq!(sent.json().unwrap().get("stream").unwrap().as_bool(), Some(false));
    }

    #[tokio::test]
    async fn assembles_a_streamed_answer_from_newline_delimited_json() {
        let lines = concat!(
            "{\"message\":{\"role\":\"assistant\",\"content\":\"Rayleigh \"},\"done\":false}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"scattering.\"},\"done\":false}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,",
            "\"done_reason\":\"stop\",\"prompt_eval_count\":14,\"eval_count\":9}\n",
        );

        let provider =
            Ollama::new().client(Client::new().faking(Fake::new().fallback(FakeResponse::text(lines))));

        let request = Request::new("llama3.2").user("Why is the sky blue?");
        let mut stream = provider.stream(&request).await.unwrap();

        assert_eq!(stream.next().await.unwrap().as_deref(), Some("Rayleigh "));
        assert_eq!(stream.next().await.unwrap().as_deref(), Some("scattering."));
        assert!(stream.next().await.unwrap().is_none());
        assert_eq!(stream.usage(), Usage::new(14, 9));
        assert_eq!(stream.stop_reason(), Some(&StopReason::EndTurn));

        let sent = &provider.client.fake().unwrap().recorded()[0];
        assert_eq!(sent.json().unwrap().get("stream").unwrap().as_bool(), Some(true));
    }

    #[test]
    fn a_streamed_error_line_stops_the_stream() {
        let error = decode(None, r#"{"error":"out of memory"}"#).unwrap_err();
        assert!(error.to_string().contains("out of memory"));
    }
}
