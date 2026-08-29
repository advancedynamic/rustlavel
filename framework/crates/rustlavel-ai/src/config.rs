//! Where the provider, the model and the key come from.
//!
//! `config/ai.json` (or anything else that reaches [`Config`]) wins; the
//! conventional environment variables are the fallback, so an application that
//! only ever exports `ANTHROPIC_API_KEY` works with no configuration at all.

use rustlavel_core::{Config, Error, Json, Result};

/// The default model per provider.
///
/// Anthropic's is the Claude 5 family's balanced member: fast enough for a web
/// request, capable enough not to be a toy.
pub const ANTHROPIC_DEFAULT_MODEL: &str = "claude-sonnet-5";
pub const OPENAI_DEFAULT_MODEL: &str = "gpt-4o-mini";
pub const OLLAMA_DEFAULT_MODEL: &str = "llama3.2";

pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const OPENAI_BASE_URL: &str = "https://api.openai.com";
pub const OLLAMA_BASE_URL: &str = "http://localhost:11434";

/// An API key that will not print itself.
///
/// A key reaches an error message, a log line or an event by accident, never on
/// purpose — so the type makes the accident impossible: `Debug` and `Display`
/// both redact, and the only way to the plaintext is [`ApiKey::expose`], which
/// is easy to grep for in review.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> ApiKey {
        ApiKey(key.into().trim().to_string())
    }

    /// The key itself. Only call this when handing it to the wire.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Remove this key from a string that is about to be shown to somebody.
    ///
    /// The last line of defence: a provider that echoes the key back in an
    /// error body would otherwise put it straight into a log.
    pub fn scrub(&self, text: impl Into<String>) -> String {
        let text = text.into();
        if self.0.is_empty() {
            return text;
        }
        text.replace(&self.0, "[redacted]")
    }

    /// An error with this key scrubbed out of it.
    pub fn scrub_error(&self, error: Error) -> Error {
        Error::msg(self.scrub(error.to_string()))
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() { "ApiKey(unset)" } else { "ApiKey([redacted])" })
    }
}

impl std::fmt::Display for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() { "" } else { "[redacted]" })
    }
}

impl From<&str> for ApiKey {
    fn from(key: &str) -> ApiKey {
        ApiKey::new(key)
    }
}

impl From<String> for ApiKey {
    fn from(key: String) -> ApiKey {
        ApiKey::new(key)
    }
}

/// Everything needed to build a provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub provider: String,
    pub model: String,
    pub api_key: ApiKey,
    pub base_url: String,
}

impl Settings {
    /// Resolve from configuration and the environment.
    pub fn resolve(config: &Config) -> Settings {
        Settings::resolve_with(config, |name| std::env::var(name).ok())
    }

    /// Resolve against an explicit environment.
    ///
    /// The environment is injected rather than read directly so that tests can
    /// exercise every fallback without mutating process state that a
    /// concurrently running test would see.
    pub fn resolve_with(config: &Config, env: impl Fn(&str) -> Option<String>) -> Settings {
        let provider = config
            .get("ai.provider")
            .as_ref()
            .and_then(Json::as_str)
            .map(str::to_string)
            .or_else(|| env("AI_PROVIDER"))
            .unwrap_or_else(|| "anthropic".to_string())
            .to_lowercase();

        let model = config
            .get("ai.model")
            .as_ref()
            .and_then(Json::as_str)
            .map(str::to_string)
            .or_else(|| env("AI_MODEL"))
            .unwrap_or_else(|| default_model(&provider).to_string());

        let api_key = config
            .get("ai.api_key")
            .as_ref()
            .and_then(Json::as_str)
            .map(str::to_string)
            .or_else(|| key_variable(&provider).and_then(&env))
            .map(ApiKey::new)
            .unwrap_or_default();

        let base_url = config
            .get("ai.base_url")
            .as_ref()
            .and_then(Json::as_str)
            .map(str::to_string)
            .or_else(|| env("AI_BASE_URL"))
            .unwrap_or_else(|| default_base_url(&provider).to_string());

        Settings { provider, model, api_key, base_url: base_url.trim_end_matches('/').to_string() }
    }

    /// Fail early when a provider that needs a key has not been given one,
    /// with the variable to export rather than a stack trace.
    pub fn require_key(&self) -> Result<()> {
        if !self.api_key.is_empty() || key_variable(&self.provider).is_none() {
            return Ok(());
        }
        let variable = key_variable(&self.provider).unwrap_or("AI_API_KEY");
        Err(Error::msg(format!(
            "no API key for the `{}` provider. Set `ai.api_key` in config/ai.json \
             or export {variable}.",
            self.provider
        )))
    }
}

/// The conventional environment variable for a provider's key, if it needs one.
pub fn key_variable(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" | "claude" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        // Ollama runs on the developer's own machine and has nothing to sign in to.
        _ => None,
    }
}

pub fn default_model(provider: &str) -> &'static str {
    match provider {
        "openai" => OPENAI_DEFAULT_MODEL,
        "ollama" => OLLAMA_DEFAULT_MODEL,
        _ => ANTHROPIC_DEFAULT_MODEL,
    }
}

pub fn default_base_url(provider: &str) -> &'static str {
    match provider {
        "openai" => OPENAI_BASE_URL,
        "ollama" => OLLAMA_BASE_URL,
        _ => ANTHROPIC_BASE_URL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An environment with nothing in it.
    fn bare(_name: &str) -> Option<String> {
        None
    }

    #[test]
    fn anthropic_and_claude_sonnet_five_are_the_defaults() {
        let settings = Settings::resolve_with(&Config::new(), bare);

        assert_eq!(settings.provider, "anthropic");
        assert_eq!(settings.model, "claude-sonnet-5");
        assert_eq!(settings.base_url, "https://api.anthropic.com");
        assert!(settings.api_key.is_empty());
    }

    #[test]
    fn configuration_beats_the_environment() {
        let config = Config::new();
        config.set("ai.provider", "openai");
        config.set("ai.model", "gpt-4o-mini");
        config.set("ai.api_key", "sk-from-config");

        let settings = Settings::resolve_with(&config, |name| match name {
            "AI_PROVIDER" => Some("ollama".to_string()),
            "OPENAI_API_KEY" => Some("sk-from-the-environment".to_string()),
            _ => None,
        });

        assert_eq!(settings.provider, "openai");
        assert_eq!(settings.model, "gpt-4o-mini");
        assert_eq!(settings.api_key.expose(), "sk-from-config");
    }

    #[test]
    fn the_key_falls_back_to_the_variable_the_provider_conventionally_uses() {
        let config = Config::new();
        config.set("ai.provider", "openai");

        let settings = Settings::resolve_with(&config, |name| match name {
            "OPENAI_API_KEY" => Some("sk-openai".to_string()),
            "ANTHROPIC_API_KEY" => Some("sk-anthropic".to_string()),
            _ => None,
        });

        assert_eq!(settings.api_key.expose(), "sk-openai");
        assert_eq!(settings.model, "gpt-4o-mini");
        assert_eq!(settings.base_url, "https://api.openai.com");
    }

    #[test]
    fn anthropic_reads_its_own_variable() {
        let settings = Settings::resolve_with(&Config::new(), |name| match name {
            "ANTHROPIC_API_KEY" => Some("sk-ant-123".to_string()),
            _ => None,
        });

        assert_eq!(settings.api_key.expose(), "sk-ant-123");
    }

    #[test]
    fn a_base_url_is_taken_from_config_and_stripped_of_its_slash() {
        let config = Config::new();
        config.set("ai.base_url", "https://gateway.internal/v1/");

        assert_eq!(Settings::resolve_with(&config, bare).base_url, "https://gateway.internal/v1");
    }

    #[test]
    fn ollama_needs_no_key_but_anthropic_says_which_one_to_export() {
        let ollama = Config::new();
        ollama.set("ai.provider", "ollama");
        let settings = Settings::resolve_with(&ollama, bare);
        assert!(settings.require_key().is_ok());
        assert_eq!(settings.base_url, "http://localhost:11434");
        assert_eq!(settings.model, "llama3.2");

        let error = Settings::resolve_with(&Config::new(), bare).require_key().unwrap_err();
        assert!(error.to_string().contains("ANTHROPIC_API_KEY"), "{error}");
    }

    #[test]
    fn a_key_never_prints_itself() {
        let key = ApiKey::new("sk-ant-super-secret");

        assert_eq!(format!("{key}"), "[redacted]");
        assert_eq!(format!("{key:?}"), "ApiKey([redacted])");
        assert_eq!(format!("{:?}", ApiKey::default()), "ApiKey(unset)");

        // Even when a third party echoes it back at us.
        let echoed = "invalid key: sk-ant-super-secret (check your dashboard)";
        assert_eq!(
            key.scrub(echoed),
            "invalid key: [redacted] (check your dashboard)"
        );
        assert!(!key.scrub_error(Error::msg(echoed)).to_string().contains("super-secret"));
    }

    #[test]
    fn scrubbing_with_no_key_leaves_the_text_alone() {
        assert_eq!(ApiKey::default().scrub("HTTP 500: boom"), "HTTP 500: boom");
    }

    #[test]
    fn surrounding_whitespace_in_a_key_is_dropped() {
        // A key pasted into `.env` with a trailing newline is otherwise a
        // 401 nobody can explain.
        assert_eq!(ApiKey::new("  sk-ant-1\n").expose(), "sk-ant-1");
    }
}
