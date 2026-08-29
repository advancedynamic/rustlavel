//! What a model gives back: text, tool calls, why it stopped, what it cost.

use crate::message::{Content, Message, Role};
use rustlavel_core::Json;

/// How many tokens a call consumed.
///
/// Providers disagree about the names (`input_tokens`, `prompt_tokens`,
/// `prompt_eval_count`) but not the meaning, so they are normalised here —
/// this is what Telescope charts and what a bill is made of.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

impl Usage {
    pub fn new(input_tokens: u32, output_tokens: u32) -> Usage {
        Usage { input_tokens, output_tokens }
    }

    pub fn total(&self) -> u32 {
        self.input_tokens + self.output_tokens
    }

    /// Add another call's usage, for a multi-round tool loop that should be
    /// billed as one operation.
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

/// One tool the model wants run, with the arguments it chose.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// The provider's id for this call. It has to be echoed back with the
    /// result so the model can match answer to question.
    pub id: String,
    pub name: String,
    pub arguments: Json,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Json) -> ToolCall {
        ToolCall { id: id.into(), name: name.into(), arguments }
    }

    /// One named argument, by dotted path.
    pub fn argument(&self, path: &str) -> Option<&Json> {
        self.arguments.get(path)
    }
}

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// It finished what it had to say.
    EndTurn,
    /// It ran into `max_tokens` — the text is probably truncated.
    MaxTokens,
    /// It hit one of the caller's stop sequences.
    StopSequence,
    /// It is waiting for tool results.
    ToolUse,
    /// Something provider-specific; kept verbatim rather than guessed at.
    Other(String),
}

impl StopReason {
    /// Map a provider's word onto ours.
    ///
    /// The vocabularies genuinely overlap (`end_turn`/`stop`, `max_tokens`/
    /// `length`), so one table serves all three providers.
    pub fn parse(text: &str) -> StopReason {
        match text {
            "end_turn" | "stop" | "stop_sequence_reached" => StopReason::EndTurn,
            "max_tokens" | "length" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            "tool_use" | "tool_calls" | "function_call" => StopReason::ToolUse,
            other => StopReason::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::MaxTokens => "max_tokens",
            StopReason::StopSequence => "stop_sequence",
            StopReason::ToolUse => "tool_use",
            StopReason::Other(other) => other,
        }
    }

    pub fn is_tool_use(&self) -> bool {
        matches!(self, StopReason::ToolUse)
    }
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One answer from a model.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    pub usage: Usage,
    /// The model that actually answered, which is not always the one asked
    /// for — providers alias and pin.
    pub model: String,
}

impl Default for Completion {
    fn default() -> Completion {
        Completion {
            text: String::new(),
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            model: String::new(),
        }
    }
}

impl Completion {
    /// A plain text answer, mostly for fakes and tests.
    pub fn text(text: impl Into<String>) -> Completion {
        Completion { text: text.into(), ..Completion::default() }
    }

    pub fn with_tool_call(mut self, call: ToolCall) -> Completion {
        self.tool_calls.push(call);
        self.stop_reason = StopReason::ToolUse;
        self
    }

    pub fn with_usage(mut self, usage: Usage) -> Completion {
        self.usage = usage;
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Completion {
        self.model = model.into();
        self
    }

    pub fn wants_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// Replay this answer as an assistant turn.
    ///
    /// The tool loop has to put the assistant's own words *and* its tool
    /// requests back into the conversation, or the provider rejects the
    /// tool results as answers to a question nobody asked.
    pub fn to_message(&self) -> Message {
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(Content::Text(self.text.clone()));
        }
        for call in &self.tool_calls {
            content.push(Content::ToolUse {
                id: call.id.clone(),
                name: call.name.clone(),
                input: call.arguments.clone(),
            });
        }
        Message::new(Role::Assistant, content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_adds_up_across_rounds() {
        let mut usage = Usage::new(10, 5);
        usage.add(Usage::new(3, 4));

        assert_eq!(usage, Usage::new(13, 9));
        assert_eq!(usage.total(), 22);
    }

    #[test]
    fn stop_reasons_from_every_provider_map_onto_one_vocabulary() {
        assert_eq!(StopReason::parse("end_turn"), StopReason::EndTurn);
        assert_eq!(StopReason::parse("stop"), StopReason::EndTurn);
        assert_eq!(StopReason::parse("length"), StopReason::MaxTokens);
        assert_eq!(StopReason::parse("tool_calls"), StopReason::ToolUse);
        assert!(StopReason::parse("tool_use").is_tool_use());
        assert_eq!(StopReason::parse("weird"), StopReason::Other("weird".into()));
        assert_eq!(StopReason::parse("weird").to_string(), "weird");
    }

    #[test]
    fn a_completion_replays_as_an_assistant_turn_with_its_tool_requests() {
        let completion = Completion::text("Checking the weather.").with_tool_call(ToolCall::new(
            "toolu_1",
            "get_weather",
            Json::object([("city", "Oslo".into())]),
        ));

        assert!(completion.wants_tools());
        assert_eq!(completion.stop_reason, StopReason::ToolUse);

        let message = completion.to_message();
        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.content.len(), 2);
        assert_eq!(message.text(), "Checking the weather.");
        assert_eq!(message.tool_uses().count(), 1);
    }

    #[test]
    fn an_empty_answer_replays_without_an_empty_text_part() {
        let completion = Completion::default().with_tool_call(ToolCall::new("id", "t", Json::Null));

        assert_eq!(completion.to_message().content.len(), 1);
    }

    #[test]
    fn a_tool_call_reads_its_arguments_by_path() {
        let call = ToolCall::new(
            "call_1",
            "search",
            Json::object([("query", Json::object([("text", "rust".into())]))]),
        );

        assert_eq!(call.argument("query.text").unwrap().as_str(), Some("rust"));
        assert!(call.argument("missing").is_none());
    }
}
