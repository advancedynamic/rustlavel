//! The conversation model, kept deliberately free of any provider's shape.
//!
//! Every provider is translated to and from these types, so an application
//! that switches from Anthropic to Ollama changes one line of configuration
//! and nothing else.

use rustlavel_core::Json;

/// Who a message came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Instructions that frame the whole conversation.
    System,
    User,
    Assistant,
    /// The result of a tool the assistant asked to run.
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }

    pub fn parse(text: &str) -> Option<Role> {
        match text {
            "system" => Some(Role::System),
            "user" => Some(Role::User),
            "assistant" => Some(Role::Assistant),
            "tool" => Some(Role::Tool),
            _ => None,
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One part of a message.
///
/// A message is a list of parts rather than a string because a single
/// assistant turn can say something *and* ask for two tools, and every
/// provider needs those parts kept apart to replay the turn back to it.
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    Text(String),
    /// The assistant asking for a tool to be run.
    ToolUse { id: String, name: String, input: Json },
    /// What the tool answered, fed back on the next turn.
    ToolResult { id: String, name: String, output: Json, is_error: bool },
}

impl Content {
    pub fn text(text: impl Into<String>) -> Content {
        Content::Text(text.into())
    }

    /// The text of this part, or `None` for a tool part.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Content::Text(text) => Some(text),
            _ => None,
        }
    }
}

/// One turn in a conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
}

impl Message {
    pub fn new(role: Role, content: Vec<Content>) -> Message {
        Message { role, content }
    }

    pub fn system(text: impl Into<String>) -> Message {
        Message::new(Role::System, vec![Content::text(text)])
    }

    pub fn user(text: impl Into<String>) -> Message {
        Message::new(Role::User, vec![Content::text(text)])
    }

    pub fn assistant(text: impl Into<String>) -> Message {
        Message::new(Role::Assistant, vec![Content::text(text)])
    }

    /// The answer from a tool the assistant asked for.
    pub fn tool_result(id: impl Into<String>, name: impl Into<String>, output: Json) -> Message {
        Message::new(
            Role::Tool,
            vec![Content::ToolResult {
                id: id.into(),
                name: name.into(),
                output,
                is_error: false,
            }],
        )
    }

    /// A tool that failed.
    ///
    /// Handed back to the model rather than raised, because a model that hears
    /// "that city does not exist" usually corrects itself, and a model that
    /// hears nothing at all just hangs the loop.
    pub fn tool_error(
        id: impl Into<String>,
        name: impl Into<String>,
        message: impl Into<String>,
    ) -> Message {
        Message::new(
            Role::Tool,
            vec![Content::ToolResult {
                id: id.into(),
                name: name.into(),
                output: Json::from(message.into()),
                is_error: true,
            }],
        )
    }

    pub fn with(mut self, part: Content) -> Message {
        self.content.push(part);
        self
    }

    /// Every text part joined, which is what a caller usually means by
    /// "what did it say".
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(Content::as_text)
            .collect::<Vec<_>>()
            .join("")
    }

    /// The tool requests in this turn, if any.
    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Json)> {
        self.content.iter().filter_map(|part| match part {
            Content::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

/// An ordered list of messages, the thing you actually send.
///
/// Thin on purpose: the value is in having one type both the front door and
/// the tool loop can append to without arguing about ownership.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Conversation {
    messages: Vec<Message>,
}

impl Conversation {
    pub fn new() -> Conversation {
        Conversation::default()
    }

    pub fn system(mut self, text: impl Into<String>) -> Conversation {
        self.messages.push(Message::system(text));
        self
    }

    pub fn user(mut self, text: impl Into<String>) -> Conversation {
        self.messages.push(Message::user(text));
        self
    }

    pub fn assistant(mut self, text: impl Into<String>) -> Conversation {
        self.messages.push(Message::assistant(text));
        self
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub fn extend(&mut self, messages: impl IntoIterator<Item = Message>) {
        self.messages.extend(messages);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn last(&self) -> Option<&Message> {
        self.messages.last()
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }
}

impl From<Conversation> for Vec<Message> {
    fn from(conversation: Conversation) -> Vec<Message> {
        conversation.messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_joins_its_text_parts_and_keeps_tool_parts_apart() {
        let message = Message::assistant("Let me check. ")
            .with(Content::text("One moment."))
            .with(Content::ToolUse {
                id: "toolu_1".into(),
                name: "get_weather".into(),
                input: Json::object([("city", "Oslo".into())]),
            });

        assert_eq!(message.text(), "Let me check. One moment.");

        let uses: Vec<_> = message.tool_uses().collect();
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].0, "toolu_1");
        assert_eq!(uses[0].1, "get_weather");
        assert_eq!(uses[0].2.get("city").unwrap().as_str(), Some("Oslo"));
    }

    #[test]
    fn a_failing_tool_becomes_a_message_rather_than_an_error() {
        let message = Message::tool_error("toolu_1", "get_weather", "no such city");

        assert_eq!(message.role, Role::Tool);
        match &message.content[0] {
            Content::ToolResult { is_error, output, .. } => {
                assert!(is_error);
                assert_eq!(output.as_str(), Some("no such city"));
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    #[test]
    fn a_conversation_builds_up_in_order() {
        let mut conversation = Conversation::new().system("Be brief.").user("Hello");
        conversation.push(Message::assistant("Hi."));

        assert_eq!(conversation.len(), 3);
        assert_eq!(conversation.messages()[0].role, Role::System);
        assert_eq!(conversation.last().unwrap().text(), "Hi.");
        assert!(!conversation.is_empty());
    }

    #[test]
    fn roles_round_trip_through_their_wire_names() {
        for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
            assert_eq!(role.to_string(), role.as_str());
        }
        assert_eq!(Role::parse("wizard"), None);
    }
}
