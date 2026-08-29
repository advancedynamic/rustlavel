//! rustlavel-ai: one API for talking to language models.
//!
//! Laravel 13 shipped an AI SDK as a first-party package; this is the same idea
//! in Rust. Anthropic, OpenAI and Ollama sit behind one [`Provider`] trait, so
//! the difference between them is a line of configuration rather than a
//! rewrite, and [`Ai::fake()`] means an application's tests need neither a key
//! nor a network.
//!
//! The common case is one line:
//!
//! ```ignore
//! let answer = Ai::from_config(&config)?.prompt("Summarise this article").text().await?;
//! ```
//!
//! and so is streaming it:
//!
//! ```ignore
//! let mut stream = ai.prompt("Tell me a story").stream().await?;
//! while let Some(delta) = stream.next().await? {
//!     print!("{delta}");
//! }
//! ```
//!
//! Tools, structured output and multi-turn conversations are the same builder
//! with one more call on it. Every call reports an `ai.call` event — provider,
//! model, token counts, duration — so Telescope can show what the models cost
//! without ever seeing a prompt or a key.

pub mod completion;
pub mod config;
pub mod fake;
pub mod message;
pub mod provider;
pub mod providers;
pub mod request;
pub mod structured;
pub mod tool;

pub use completion::{Completion, StopReason, ToolCall, Usage};
pub use config::{ApiKey, Settings};
pub use fake::Fake;
pub use message::{Content, Conversation, Message, Role};
pub use provider::{BoxFuture, Provider, StreamDelta, TextStream};
pub use providers::{Anthropic, Ollama, OpenAi};
pub use request::Request;
pub use structured::generate_as;
pub use tool::{DEFAULT_MAX_ROUNDS, Schema, Tool, Toolbox, run_tools};

use rustlavel_client::Client;
use rustlavel_core::{Config, Error, Json, Result};
use std::sync::Arc;

/// The front door.
///
/// Holds a provider and the model to use with it. Cloning is cheap — the
/// provider is shared — so an `Ai` can live in the application context and be
/// handed to every handler.
#[derive(Clone)]
pub struct Ai {
    provider: Arc<dyn Provider>,
    model: String,
    /// Kept separately so a test can reach the script it wrote.
    fake: Option<Arc<Fake>>,
}

impl Ai {
    /// Use a provider directly.
    pub fn provider(provider: impl Provider) -> Ai {
        Ai::shared(Arc::new(provider))
    }

    /// Use a provider that is already shared.
    pub fn shared(provider: Arc<dyn Provider>) -> Ai {
        let model = provider.default_model().to_string();
        Ai { provider, model, fake: None }
    }

    /// Build the provider named in the configuration.
    ///
    /// This is what an application calls at boot; see [`Settings`] for where
    /// each value comes from.
    pub fn from_config(config: &Config) -> Result<Ai> {
        Ai::from_settings(Settings::resolve(config))
    }

    /// Build a provider from settings resolved elsewhere.
    pub fn from_settings(settings: Settings) -> Result<Ai> {
        settings.require_key()?;

        // One client per provider, with retries: a model API is a remote
        // service on someone else's bad day.
        let client = Client::new()
            .retries(2)
            .timeout(std::time::Duration::from_secs(120));

        let provider: Arc<dyn Provider> = match settings.provider.as_str() {
            "anthropic" | "claude" => Arc::new(
                Anthropic::new(settings.api_key.clone())
                    .base_url(&settings.base_url)
                    .client(client),
            ),
            "openai" => Arc::new(
                OpenAi::new(settings.api_key.clone()).base_url(&settings.base_url).client(client),
            ),
            "ollama" => Arc::new(Ollama::new().base_url(&settings.base_url).client(client)),
            other => {
                return Err(Error::msg(format!(
                    "unknown AI provider `{other}`. \
                     Set `ai.provider` to anthropic, openai or ollama."
                )));
            }
        };

        Ok(Ai { provider, model: settings.model, fake: None })
    }

    /// A provider that answers from a script, for tests.
    pub fn fake() -> Ai {
        Ai::fake_with(Fake::new())
    }

    /// A scripted provider, written out in advance.
    pub fn fake_with(fake: Fake) -> Ai {
        let fake = Arc::new(fake);
        Ai {
            provider: fake.clone(),
            model: "fake".to_string(),
            fake: Some(fake),
        }
    }

    /// The script behind [`Ai::fake`], to assert against after the fact.
    pub fn faked(&self) -> Option<&Arc<Fake>> {
        self.fake.as_ref()
    }

    /// The provider's name, as it appears in `ai.call` events.
    pub fn name(&self) -> &'static str {
        self.provider.name()
    }

    /// The model every call will use unless it says otherwise.
    pub fn default_model(&self) -> &str {
        &self.model
    }

    /// Change the model for every call made through this handle.
    pub fn using(mut self, model: impl Into<String>) -> Ai {
        self.model = model.into();
        self
    }

    /// Start a call.
    pub fn ask(&self) -> Generation {
        Generation {
            ai: self.clone(),
            request: Request::new(&self.model),
            toolbox: None,
            max_rounds: DEFAULT_MAX_ROUNDS,
        }
    }

    /// Start a call with the user's message already in it — the common case.
    pub fn prompt(&self, text: impl Into<String>) -> Generation {
        self.ask().prompt(text)
    }

    /// Start a call with a model other than the default.
    pub fn model(&self, model: impl Into<String>) -> Generation {
        self.ask().model(model)
    }

    /// Start a call from an existing conversation.
    pub fn chat(&self, conversation: Conversation) -> Generation {
        self.ask().conversation(conversation)
    }
}

impl std::fmt::Debug for Ai {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ai")
            .field("provider", &self.provider.name())
            .field("model", &self.model)
            .finish()
    }
}

/// One call being assembled.
///
/// Every setting is optional; `ai.prompt("…").generate().await` is a complete
/// program.
pub struct Generation {
    ai: Ai,
    request: Request,
    toolbox: Option<Toolbox>,
    max_rounds: usize,
}

impl Generation {
    pub fn model(mut self, model: impl Into<String>) -> Generation {
        self.request.model = model.into();
        self
    }

    pub fn system(mut self, system: impl Into<String>) -> Generation {
        self.request.system = Some(system.into());
        self
    }

    /// What to ask. Sugar for a single user message.
    pub fn prompt(mut self, text: impl Into<String>) -> Generation {
        self.request.messages.push(Message::user(text));
        self
    }

    pub fn message(mut self, message: Message) -> Generation {
        self.request.messages.push(message);
        self
    }

    pub fn conversation(mut self, conversation: Conversation) -> Generation {
        self.request.messages.extend(conversation.into_messages());
        self
    }

    pub fn temperature(mut self, temperature: f64) -> Generation {
        self.request.temperature = Some(temperature);
        self
    }

    pub fn max_tokens(mut self, max_tokens: u32) -> Generation {
        self.request.max_tokens = Some(max_tokens);
        self
    }

    pub fn stop(mut self, sequence: impl Into<String>) -> Generation {
        self.request.stop_sequences.push(sequence.into());
        self
    }

    /// Offer the model a tool it may ask for but that this crate will not run —
    /// for a caller who wants to handle the tool call themselves.
    pub fn tool(mut self, tool: Tool) -> Generation {
        self.request.tools.push(tool);
        self
    }

    /// Offer tools *and* run them: [`Generation::generate`] then loops until the
    /// model answers in words.
    pub fn tools(mut self, toolbox: Toolbox) -> Generation {
        self.toolbox = Some(toolbox);
        self
    }

    /// How many model turns the tool loop may take.
    pub fn max_rounds(mut self, max_rounds: usize) -> Generation {
        self.max_rounds = max_rounds;
        self
    }

    /// The request as it will be sent, for tests and for inspection.
    pub fn request(&self) -> &Request {
        &self.request
    }

    /// Ask, and wait for the whole answer.
    pub async fn generate(self) -> Result<Completion> {
        match &self.toolbox {
            Some(toolbox) => {
                run_tools(self.ai.provider.as_ref(), &self.request, toolbox, self.max_rounds).await
            }
            None => self.ai.provider.complete(&self.request).await,
        }
    }

    /// Ask, and keep only the words — the shortest useful call there is.
    pub async fn text(self) -> Result<String> {
        Ok(self.generate().await?.text)
    }

    /// Ask, and read the answer as it is written.
    ///
    /// Tools are not run while streaming: a tool call is not text, and a caller
    /// who wants both should use [`Generation::generate`].
    pub async fn stream(self) -> Result<TextStream> {
        self.ai.provider.stream(&self.request).await
    }

    /// Ask for JSON matching a schema, and get it parsed.
    ///
    /// Retries once if the model's answer will not parse; see
    /// [`structured::generate_as`].
    pub async fn generate_as(self, schema: &Schema) -> Result<Json> {
        structured::generate_as(self.ai.provider.as_ref(), &self.request, schema).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_core::events::{self, Event};
    use std::sync::Mutex;

    #[tokio::test]
    async fn the_common_case_is_one_line() {
        let ai = Ai::fake_with(Fake::new().always("The sky scatters blue light."));

        assert_eq!(ai.prompt("Why is the sky blue?").text().await.unwrap(), "The sky scatters blue light.");
    }

    #[tokio::test]
    async fn the_builder_carries_every_setting_onto_the_request() {
        let ai = Ai::fake_with(Fake::new().always("ok"));

        ai.model("claude-fable-5")
            .system("Be brief.")
            .prompt("Why is the sky blue?")
            .temperature(0.2)
            .max_tokens(1024)
            .stop("END")
            .generate()
            .await
            .unwrap();

        let sent = ai.faked().unwrap().last_request().unwrap();
        assert_eq!(sent.model, "claude-fable-5");
        assert_eq!(sent.system.as_deref(), Some("Be brief."));
        assert_eq!(sent.temperature, Some(0.2));
        assert_eq!(sent.max_tokens, Some(1024));
        assert_eq!(sent.stop_sequences, vec!["END".to_string()]);
        assert_eq!(sent.last_user_text().as_deref(), Some("Why is the sky blue?"));
    }

    #[tokio::test]
    async fn a_conversation_can_be_continued() {
        let ai = Ai::fake_with(Fake::new().always("Because of Rayleigh scattering."));
        let conversation = Conversation::new().user("Why is the sky blue?").assistant("Physics.");

        let answer = ai.chat(conversation).prompt("Go on.").text().await.unwrap();

        assert_eq!(answer, "Because of Rayleigh scattering.");
        assert_eq!(ai.faked().unwrap().last_request().unwrap().messages.len(), 3);
    }

    #[tokio::test]
    async fn streams_deltas_the_caller_can_loop_over() {
        let ai = Ai::fake_with(Fake::new().streams(["Once ", "upon ", "a time."]));

        let mut stream = ai.prompt("Tell me a story").stream().await.unwrap();
        let mut seen = Vec::new();
        while let Some(delta) = stream.next().await.unwrap() {
            seen.push(delta);
        }

        assert_eq!(seen, vec!["Once ", "upon ", "a time."]);
    }

    #[tokio::test]
    async fn runs_the_tool_loop_until_the_model_answers_in_words() {
        let ai = Ai::fake_with(
            Fake::new()
                .calls_tool("get_weather", Json::object([("city", "Oslo".into())]))
                .reply_with(Completion::text("It is 7 degrees in Oslo.").with_usage(Usage::new(30, 8))),
        );

        let toolbox = Toolbox::new().add(
            Tool::new("get_weather", "Look up today's weather").string("city", "The city"),
            |input: Json| async move {
                let city = input.get("city").and_then(Json::as_str).unwrap_or("").to_string();
                Ok(Json::object([("city", Json::from(city)), ("degrees", 7.into())]))
            },
        );

        let completion = ai
            .prompt("What is the weather in Oslo?")
            .tools(toolbox)
            .generate()
            .await
            .unwrap();

        assert_eq!(completion.text, "It is 7 degrees in Oslo.");
        assert!(!completion.wants_tools());

        let fake = ai.faked().unwrap();
        fake.assert_count(2);

        // The second call replayed the assistant's request and the tool's answer.
        let second = fake.last_request().unwrap();
        assert_eq!(second.messages.len(), 3);
        assert_eq!(second.messages[1].role, Role::Assistant);
        assert_eq!(second.messages[1].tool_uses().count(), 1);
        assert_eq!(second.messages[2].role, Role::Tool);
        assert!(second.messages[2].content[0] != Content::Text(String::new()));
        // The tools were described to the model without the caller repeating them.
        assert_eq!(second.tools.len(), 1);
    }

    #[tokio::test]
    async fn a_failing_tool_is_reported_to_the_model_rather_than_the_caller() {
        let ai = Ai::fake_with(
            Fake::new()
                .calls_tool("get_weather", Json::object([("city", "Atlantis".into())]))
                .reply("I could not find that city."),
        );

        let toolbox = Toolbox::new().add(
            Tool::new("get_weather", "Look up today's weather").string("city", "The city"),
            |_: Json| async { Err(rustlavel_core::Error::msg("no such city")) },
        );

        let completion =
            ai.prompt("Weather in Atlantis?").tools(toolbox).generate().await.unwrap();

        assert_eq!(completion.text, "I could not find that city.");
        let second = ai.faked().unwrap().last_request().unwrap();
        assert!(second.messages[2].text().is_empty());
        match &second.messages[2].content[0] {
            Content::ToolResult { is_error, output, .. } => {
                assert!(is_error);
                assert_eq!(output.as_str(), Some("no such city"));
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_tool_loop_that_will_not_end_is_capped() {
        let fake = Fake::new();
        let ai = Ai::fake_with(
            (0..5).fold(fake, |fake, _| {
                fake.calls_tool("spin", Json::object::<&str, _>([]))
            }),
        );

        let toolbox = Toolbox::new()
            .add(Tool::new("spin", "Spin forever"), |_: Json| async { Ok(Json::Null) });

        let error = ai
            .prompt("Spin")
            .tools(toolbox)
            .max_rounds(3)
            .generate()
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("3 rounds"), "{error}");
        ai.faked().unwrap().assert_count(3);
    }

    #[tokio::test]
    async fn tool_usage_is_summed_across_the_whole_loop() {
        let ai = Ai::fake_with(
            Fake::new()
                .reply_with(
                    Completion::default()
                        .with_tool_call(ToolCall::new("call_fake", "noop", Json::Null))
                        .with_usage(Usage::new(10, 4)),
                )
                .reply_with(Completion::text("done").with_usage(Usage::new(20, 6))),
        );

        let toolbox =
            Toolbox::new().add(Tool::new("noop", "Does nothing"), |_: Json| async { Ok(Json::Null) });

        let completion = ai.prompt("Go").tools(toolbox).generate().await.unwrap();

        assert_eq!(completion.usage, Usage::new(30, 10));
    }

    #[tokio::test]
    async fn generates_structured_output_through_the_builder() {
        let ai = Ai::fake_with(Fake::new().always(r#"{"title":"Rust","score":9}"#));

        let value = ai
            .prompt("Rate this article")
            .generate_as(&Schema::object().string("title", "Its title").integer("score", "1 to 10"))
            .await
            .unwrap();

        assert_eq!(value.get("title").unwrap().as_str(), Some("Rust"));
        assert_eq!(value.get("score").unwrap().as_i64(), Some(9));
    }

    #[test]
    fn the_configured_provider_is_built_from_settings() {
        let config = Config::new();
        config.set("ai.provider", "openai");
        config.set("ai.api_key", "sk-test");
        config.set("ai.model", "gpt-4o-mini");

        let ai = Ai::from_config(&config).unwrap();

        assert_eq!(ai.name(), "openai");
        assert_eq!(ai.default_model(), "gpt-4o-mini");
        assert!(format!("{ai:?}").contains("openai"));
    }

    #[test]
    fn an_unknown_provider_says_which_ones_exist() {
        let config = Config::new();
        config.set("ai.provider", "skynet");

        let error = Ai::from_config(&config).unwrap_err().to_string();

        assert!(error.contains("skynet"), "{error}");
        assert!(error.contains("anthropic, openai or ollama"), "{error}");
    }

    #[test]
    fn a_missing_key_is_caught_at_boot_and_never_printed() {
        let config = Config::new();
        config.set("ai.provider", "anthropic");
        config.set("ai.api_key", "");

        let error = Ai::from_config(&config).unwrap_err().to_string();

        assert!(error.contains("ANTHROPIC_API_KEY"), "{error}");
    }

    #[test]
    fn ollama_needs_no_key_and_defaults_to_a_local_model() {
        let config = Config::new();
        config.set("ai.provider", "ollama");

        let ai = Ai::from_config(&config).unwrap();

        assert_eq!(ai.name(), "ollama");
        assert_eq!(ai.default_model(), "llama3.2");
    }

    #[tokio::test]
    async fn a_provider_can_be_handed_over_directly_and_its_model_overridden() {
        let ai = Ai::provider(Fake::new().always("ok")).using("claude-haiku-4-5-20251001");

        assert_eq!(ai.default_model(), "claude-haiku-4-5-20251001");
        assert_eq!(ai.ask().prompt("Hi").request().model, "claude-haiku-4-5-20251001");
        assert!(ai.faked().is_none());
        assert_eq!(ai.prompt("Hi").text().await.unwrap(), "ok");
    }

    /// The event bus is process-global, so this test owns it: it is the only
    /// one in the crate that subscribes, and it clears up after itself.
    #[tokio::test]
    async fn a_call_is_reported_without_the_prompt_or_the_key() {
        use rustlavel_client::fake::{Fake as HttpFake, FakeResponse};

        let seen: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);

        events::clear_subscribers();
        events::subscribe(move |event: &Event| {
            if event.kind == "ai.call" {
                sink.lock().unwrap().push(event.clone());
            }
        });

        let answer = Json::parse(
            r#"{"model":"claude-sonnet-5","content":[{"type":"text","text":"Blue."}],
                "stop_reason":"end_turn","usage":{"input_tokens":14,"output_tokens":9}}"#,
        )
        .unwrap();

        let provider = Anthropic::new("sk-ant-super-secret")
            .client(Client::new().faking(HttpFake::new().fallback(FakeResponse::json(answer))));

        Ai::provider(provider)
            .prompt("A very secret prompt about the sky")
            .generate()
            .await
            .unwrap();

        let events = seen.lock().unwrap().clone();
        events::clear_subscribers();

        assert_eq!(events.len(), 1, "expected exactly one ai.call event");
        let event = &events[0];
        assert_eq!(event.field("provider").and_then(Json::as_str), Some("anthropic"));
        assert_eq!(event.field("model").and_then(Json::as_str), Some("claude-sonnet-5"));
        assert_eq!(event.field("input_tokens").and_then(Json::as_i64), Some(14));
        assert_eq!(event.field("output_tokens").and_then(Json::as_i64), Some(9));
        assert_eq!(event.field("streamed").and_then(Json::as_bool), Some(false));
        assert!(event.duration_ms().is_some());

        // Neither the prompt nor the key is anywhere in the event.
        let rendered = format!("{event:?}");
        assert!(!rendered.contains("sk-ant-super-secret"), "{rendered}");
        assert!(!rendered.contains("secret prompt"), "{rendered}");
    }
}
