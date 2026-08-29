//! What gets asked: one request shape every provider is translated from.

use crate::message::{Conversation, Message, Role};
use crate::tool::Tool;

/// A single call to a model.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Request {
    pub model: String,
    /// Instructions that frame the conversation. Kept apart from the messages
    /// because Anthropic wants it as a top-level field, and because a system
    /// prompt is configuration rather than dialogue.
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub tools: Vec<Tool>,
    pub stop_sequences: Vec<String>,
}

impl Request {
    pub fn new(model: impl Into<String>) -> Request {
        Request { model: model.into(), ..Request::default() }
    }

    pub fn model(mut self, model: impl Into<String>) -> Request {
        self.model = model.into();
        self
    }

    pub fn system(mut self, system: impl Into<String>) -> Request {
        self.system = Some(system.into());
        self
    }

    pub fn message(mut self, message: Message) -> Request {
        self.messages.push(message);
        self
    }

    pub fn messages(mut self, messages: impl IntoIterator<Item = Message>) -> Request {
        self.messages.extend(messages);
        self
    }

    pub fn conversation(self, conversation: Conversation) -> Request {
        self.messages(conversation.into_messages())
    }

    pub fn user(self, text: impl Into<String>) -> Request {
        self.message(Message::user(text))
    }

    pub fn assistant(self, text: impl Into<String>) -> Request {
        self.message(Message::assistant(text))
    }

    /// How adventurous the model may be, from 0.0 to 1.0.
    pub fn temperature(mut self, temperature: f64) -> Request {
        self.temperature = Some(temperature);
        self
    }

    pub fn max_tokens(mut self, max_tokens: u32) -> Request {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn tool(mut self, tool: Tool) -> Request {
        self.tools.push(tool);
        self
    }

    pub fn tools(mut self, tools: impl IntoIterator<Item = Tool>) -> Request {
        self.tools.extend(tools);
        self
    }

    pub fn stop(mut self, sequence: impl Into<String>) -> Request {
        self.stop_sequences.push(sequence.into());
        self
    }

    /// The system prompt as the provider should see it.
    ///
    /// A caller can set `system` directly *or* push `Role::System` messages
    /// into the conversation; both are honoured, joined in order, so no
    /// instruction is silently dropped just because it arrived the other way.
    pub fn effective_system(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(system) = &self.system {
            parts.push(system.clone());
        }
        for message in &self.messages {
            if message.role == Role::System {
                parts.push(message.text());
            }
        }
        if parts.is_empty() {
            return None;
        }
        Some(parts.join("\n\n"))
    }

    /// The messages minus the system ones, which every provider carries
    /// separately or not at all.
    pub fn dialogue(&self) -> impl Iterator<Item = &Message> {
        self.messages.iter().filter(|message| message.role != Role::System)
    }

    /// The last thing the user said, for logs and fakes that match on it.
    pub fn last_user_text(&self) -> Option<String> {
        self.messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(Message::text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool;

    #[test]
    fn a_request_is_built_in_one_expression() {
        let request = Request::new("claude-sonnet-5")
            .system("Be brief.")
            .user("Why is the sky blue?")
            .temperature(0.2)
            .max_tokens(1024)
            .stop("END")
            .tool(Tool::new("search", "Search the web"));

        assert_eq!(request.model, "claude-sonnet-5");
        assert_eq!(request.temperature, Some(0.2));
        assert_eq!(request.max_tokens, Some(1024));
        assert_eq!(request.stop_sequences, vec!["END".to_string()]);
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.last_user_text().as_deref(), Some("Why is the sky blue?"));
    }

    #[test]
    fn system_messages_and_the_system_field_are_both_honoured() {
        let request = Request::new("claude-sonnet-5")
            .system("Be brief.")
            .message(Message::system("Answer in English."))
            .user("Hello");

        assert_eq!(request.effective_system().unwrap(), "Be brief.\n\nAnswer in English.");
        // The dialogue the provider sends is the conversation without them.
        assert_eq!(request.dialogue().count(), 1);
    }

    #[test]
    fn a_request_without_instructions_has_no_system_prompt() {
        assert!(Request::new("claude-sonnet-5").user("Hi").effective_system().is_none());
    }

    #[test]
    fn a_conversation_can_be_handed_over_whole() {
        let conversation = Conversation::new().user("One").assistant("Two").user("Three");
        let request = Request::new("claude-sonnet-5").conversation(conversation);

        assert_eq!(request.messages.len(), 3);
        assert_eq!(request.last_user_text().as_deref(), Some("Three"));
    }
}
