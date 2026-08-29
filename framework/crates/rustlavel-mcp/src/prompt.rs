//! A prompt: a reusable message template the user picks, not the model.
//!
//! MCP treats prompts as user-initiated — a slash command in a desktop client
//! — which is why they are a separate registry from tools rather than a tool
//! that happens to return text.

use crate::protocol::{PromptArgument, PromptInfo, PromptMessage};
use rustlavel_core::{Json, Result};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type PromptFuture = Pin<Box<dyn Future<Output = Result<Vec<PromptMessage>>> + Send>>;

/// The work behind a prompt. Implemented for every
/// `async fn(Json) -> Result<Vec<PromptMessage>>`.
pub trait PromptRenderer: Send + Sync + 'static {
    fn render(&self, arguments: Json) -> PromptFuture;
}

impl<F, Fut> PromptRenderer for F
where
    F: Fn(Json) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Vec<PromptMessage>>> + Send + 'static,
{
    fn render(&self, arguments: Json) -> PromptFuture {
        Box::pin(self(arguments))
    }
}

/// One registered prompt.
#[derive(Clone)]
pub struct Prompt {
    pub name: String,
    pub description: String,
    pub arguments: Vec<PromptArgument>,
    renderer: Arc<dyn PromptRenderer>,
}

impl Prompt {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        renderer: impl PromptRenderer,
    ) -> Self {
        Prompt {
            name: name.into(),
            description: description.into(),
            arguments: Vec::new(),
            renderer: Arc::new(renderer),
        }
    }

    pub fn argument(mut self, argument: PromptArgument) -> Self {
        self.arguments.push(argument);
        self
    }

    pub fn info(&self) -> PromptInfo {
        PromptInfo {
            name: self.name.clone(),
            description: self.description.clone(),
            arguments: self.arguments.clone(),
        }
    }

    /// Every declared argument that is required but absent.
    ///
    /// Prompts carry a plain argument list rather than a JSON schema, so the
    /// check is presence only — there is no declared type to check against.
    pub fn missing_arguments(&self, arguments: &Json) -> Vec<String> {
        self.arguments
            .iter()
            .filter(|argument| argument.required)
            .filter(|argument| {
                // Looked up through the map, not the dotted-path helper: an
                // argument name is allowed to contain a dot.
                arguments
                    .as_object()
                    .and_then(|map| map.get(&argument.name))
                    .is_none_or(Json::is_null)
            })
            .map(|argument| format!("`{}` is required", argument.name))
            .collect()
    }

    /// Render, surviving a panicking renderer.
    pub async fn render(&self, arguments: Json) -> Result<Json> {
        rustlavel_http::panic::install_hook();

        let renderer = Arc::clone(&self.renderer);
        let rendered =
            match rustlavel_http::panic::catch(async move { renderer.render(arguments).await })
                .await
            {
                Ok(result) => result?,
                Err(message) => {
                    rustlavel_core::error!("panic rendering MCP prompt `{}`: {message}", self.name);
                    return Err(rustlavel_core::Error::msg(format!(
                        "`{}` panicked: {message}",
                        self.name
                    )));
                }
            };

        Ok(Json::object([
            ("description", Json::from(self.description.clone())),
            ("messages", Json::Array(rendered.iter().map(PromptMessage::to_json).collect())),
        ]))
    }
}

impl std::fmt::Debug for Prompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prompt").field("name", &self.name).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review_prompt() -> Prompt {
        Prompt::new("review", "Review one order", |args: Json| async move {
            let id = args.get("order_id").and_then(Json::as_str).unwrap_or("?");
            Ok(vec![PromptMessage::user(format!("Please review order {id}."))])
        })
        .argument(PromptArgument::new("order_id", "Which order to review"))
    }

    #[tokio::test]
    async fn a_prompt_renders_into_the_messages_shape() {
        let rendered = review_prompt()
            .render(Json::object([("order_id", Json::from("42"))]))
            .await
            .unwrap();

        assert_eq!(rendered.get("description").unwrap().as_str(), Some("Review one order"));
        assert_eq!(
            rendered.get("messages.0.content.text").unwrap().as_str(),
            Some("Please review order 42.")
        );
        assert_eq!(rendered.get("messages.0.role").unwrap().as_str(), Some("user"));
    }

    #[test]
    fn a_missing_required_argument_is_reported_before_rendering() {
        let prompt = review_prompt();

        assert!(prompt.missing_arguments(&Json::object([("order_id", Json::from("1"))])).is_empty());
        assert_eq!(prompt.missing_arguments(&Json::Null), ["`order_id` is required"]);
    }

    #[test]
    fn a_prompt_lists_its_arguments() {
        let info = review_prompt().info();

        assert_eq!(info.arguments.len(), 1);
        assert_eq!(info.to_json().get("arguments.0.required").unwrap().as_bool(), Some(true));
    }
}
