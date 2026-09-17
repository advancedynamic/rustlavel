//! Speech to text, with the time of every word.
//!
//! A transcript for a clipping tool is not a paragraph; it is a list of words
//! and when each was said, so a cut at 42.5 seconds can land between two of
//! them. That is what [`Transcript`] is, and the paragraph is derived from it.
//!
//! ```ignore
//! let speech = Transcription::from_config(&config)?;
//! let transcript = speech
//!     .transcribe(Audio::from_path("episode.mp4").await?)
//!     .language("id")
//!     .diarize()
//!     .run()
//!     .await?;
//!
//! transcript.words_between(42.5, 71.0);   // what the clip says
//! transcript.to_srt();                     // burn it in with ffmpeg
//! ```
//!
//! Two wire formats, three deployments:
//!
//! * [`Whisper`] — OpenAI's `/v1/audio/transcriptions`, which is also what
//!   **self-hosted faster-whisper** serves (`faster-whisper-server`,
//!   `speaches`) and what Groq's transcription endpoint speaks. Point
//!   `base_url` at it. Word timestamps, no speaker identification.
//! * [`Deepgram`] — `/v1/listen`. Word timestamps, confidence, and speakers
//!   told apart when asked.
//!
//! Asking a provider for what it cannot do is an error, not a quiet `None`:
//! a clip tool that assumed speakers and got none would cut on nothing.

pub mod audio;
pub mod deepgram;
pub mod fake;
pub(crate) mod multipart;
pub mod transcript;
pub mod whisper;

pub use audio::Audio;
pub use deepgram::Deepgram;
pub use fake::FakeTranscriber;
pub use transcript::{Segment, Transcript, Word};
pub use whisper::Whisper;

use crate::config::ApiKey;
use crate::provider::BoxFuture;
use rustlavel_core::events::Event;
use rustlavel_core::{Config, Error, Json, Result};
use std::sync::Arc;
use std::time::Duration;

/// What to transcribe, and how.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscribeRequest {
    pub audio: Audio,
    /// `None` uses the provider's default.
    pub model: Option<String>,
    /// ISO 639-1. Telling the provider saves it guessing, and guessing wrong
    /// on a short clip is common.
    pub language: Option<String>,
    /// Words the audio is likely to contain — names, jargon — for providers
    /// that take a hint.
    pub prompt: Option<String>,
    /// Tell speakers apart.
    pub diarize: bool,
}

impl TranscribeRequest {
    pub fn new(audio: Audio) -> TranscribeRequest {
        TranscribeRequest { audio, model: None, language: None, prompt: None, diarize: false }
    }
}

/// A speech-to-text backend.
pub trait Transcriber: Send + Sync + 'static {
    /// As it appears in `ai.transcribe` events.
    fn name(&self) -> &'static str;

    fn default_model(&self) -> &str;

    fn transcribe<'a>(&'a self, request: &'a TranscribeRequest) -> BoxFuture<'a, Result<Transcript>>;
}

/// The front door. Cheap to clone; the backend is shared.
#[derive(Clone)]
pub struct Transcription {
    backend: Arc<dyn Transcriber>,
    model: Option<String>,
}

impl Transcription {
    pub fn new(backend: impl Transcriber) -> Transcription {
        Transcription::shared(Arc::new(backend))
    }

    pub fn shared(backend: Arc<dyn Transcriber>) -> Transcription {
        Transcription { backend, model: None }
    }

    /// Build the backend named in the configuration. See
    /// [`TranscribeSettings`] for where each value comes from.
    pub fn from_config(config: &Config) -> Result<Transcription> {
        Transcription::from_settings(TranscribeSettings::resolve(config))
    }

    pub fn from_settings(settings: TranscribeSettings) -> Result<Transcription> {
        settings.require_key()?;
        // Audio is large and models are slow; a two-minute request timeout is
        // the floor, not a generous ceiling.
        let client = rustlavel_client::Client::new().retries(1).timeout(Duration::from_secs(600));
        let backend: Arc<dyn Transcriber> = match settings.provider.as_str() {
            "whisper" | "openai" => {
                Arc::new(Whisper::new(settings.api_key.clone()).base_url(&settings.base_url).client(client))
            }
            "deepgram" => {
                Arc::new(Deepgram::new(settings.api_key.clone()).base_url(&settings.base_url).client(client))
            }
            other => {
                return Err(Error::msg(format!(
                    "unknown transcription provider `{other}`. Set `ai.transcribe.provider` to whisper \
                     (OpenAI, or a self-hosted faster-whisper server through `ai.transcribe.base_url`) \
                     or deepgram."
                )));
            }
        };
        Ok(Transcription { backend, model: settings.model })
    }

    /// A backend that answers from a script, for tests.
    pub fn fake(fake: FakeTranscriber) -> Transcription {
        Transcription::new(fake)
    }

    pub fn name(&self) -> &'static str {
        self.backend.name()
    }

    /// The model every call uses unless it says otherwise.
    pub fn using(mut self, model: impl Into<String>) -> Transcription {
        self.model = Some(model.into());
        self
    }

    /// Start a call.
    pub fn transcribe(&self, audio: Audio) -> Job {
        let mut request = TranscribeRequest::new(audio);
        request.model = self.model.clone();
        Job { backend: self.backend.clone(), request }
    }
}

impl std::fmt::Debug for Transcription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transcription").field("provider", &self.backend.name()).field("model", &self.model).finish()
    }
}

/// One transcription being assembled.
pub struct Job {
    backend: Arc<dyn Transcriber>,
    request: TranscribeRequest,
}

impl Job {
    pub fn model(mut self, model: impl Into<String>) -> Job {
        self.request.model = Some(model.into());
        self
    }

    pub fn language(mut self, language: impl Into<String>) -> Job {
        self.request.language = Some(language.into());
        self
    }

    pub fn prompt(mut self, prompt: impl Into<String>) -> Job {
        self.request.prompt = Some(prompt.into());
        self
    }

    pub fn diarize(mut self) -> Job {
        self.request.diarize = true;
        self
    }

    pub fn request(&self) -> &TranscribeRequest {
        &self.request
    }

    pub async fn run(self) -> Result<Transcript> {
        if self.request.audio.is_empty() {
            return Err(Error::msg("the audio is empty; nothing to transcribe"));
        }
        let started = std::time::Instant::now();
        let transcript = self.backend.transcribe(&self.request).await?;
        record_transcription(self.backend.name(), &transcript, self.request.audio.len(), started.elapsed());
        Ok(transcript)
    }
}

/// Where the provider, model, key and URL come from: `ai.transcribe.*` in
/// config, then the conventional variables.
///
/// | setting | config | environment | default |
/// |---|---|---|---|
/// | provider | `ai.transcribe.provider` | `TRANSCRIBE_PROVIDER` | `whisper` |
/// | model | `ai.transcribe.model` | `TRANSCRIBE_MODEL` | provider's |
/// | key | `ai.transcribe.api_key` | `DEEPGRAM_API_KEY` / `OPENAI_API_KEY` | — |
/// | base URL | `ai.transcribe.base_url` | `TRANSCRIBE_BASE_URL` | provider's |
///
/// Self-hosted faster-whisper is `provider = whisper` with `base_url` at the
/// server; it needs no key, and none is demanded when the URL is not
/// OpenAI's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscribeSettings {
    pub provider: String,
    pub model: Option<String>,
    pub api_key: ApiKey,
    pub base_url: String,
}

impl TranscribeSettings {
    pub fn resolve(config: &Config) -> TranscribeSettings {
        TranscribeSettings::resolve_with(config, |name| std::env::var(name).ok())
    }

    pub fn resolve_with(config: &Config, env: impl Fn(&str) -> Option<String>) -> TranscribeSettings {
        let read = |key: &str, variable: &str| {
            config
                .get(key)
                .as_ref()
                .and_then(Json::as_str)
                .map(str::to_string)
                .or_else(|| env(variable))
                .filter(|value| !value.is_empty())
        };
        let provider = read("ai.transcribe.provider", "TRANSCRIBE_PROVIDER")
            .unwrap_or_else(|| "whisper".to_string())
            .to_lowercase();
        let model = read("ai.transcribe.model", "TRANSCRIBE_MODEL");
        let base_url = read("ai.transcribe.base_url", "TRANSCRIBE_BASE_URL")
            .unwrap_or_else(|| default_base_url(&provider).to_string())
            .trim_end_matches('/')
            .to_string();
        let api_key = read("ai.transcribe.api_key", "TRANSCRIBE_API_KEY")
            .or_else(|| key_variable(&provider).and_then(&env))
            .map(ApiKey::new)
            .unwrap_or_default();
        TranscribeSettings { provider, model, api_key, base_url }
    }

    pub fn require_key(&self) -> Result<()> {
        if !self.api_key.is_empty() {
            return Ok(());
        }
        // A whisper server on your own machine has nothing to sign in to.
        if matches!(self.provider.as_str(), "whisper" | "openai") && self.base_url != whisper::OPENAI_BASE_URL {
            return Ok(());
        }
        let Some(variable) = key_variable(&self.provider) else { return Ok(()) };
        Err(Error::msg(format!(
            "no API key for the `{}` transcription provider. Set `ai.transcribe.api_key` or export {variable}.",
            self.provider
        )))
    }
}

fn key_variable(provider: &str) -> Option<&'static str> {
    match provider {
        "whisper" | "openai" => Some("OPENAI_API_KEY"),
        "deepgram" => Some("DEEPGRAM_API_KEY"),
        _ => None,
    }
}

fn default_base_url(provider: &str) -> &'static str {
    match provider {
        "deepgram" => deepgram::DEEPGRAM_BASE_URL,
        _ => whisper::OPENAI_BASE_URL,
    }
}

/// `ai.transcribe`: provider, model, audio size, audio length, time taken.
/// Never the words.
fn record_transcription(provider: &str, transcript: &Transcript, audio_bytes: usize, elapsed: Duration) {
    if !rustlavel_core::events::has_subscribers() {
        return;
    }
    Event::new("ai.transcribe")
        .with("provider", provider)
        .with("model", transcript.model.as_str())
        .with("audio_bytes", audio_bytes as i64)
        .with("audio_seconds", transcript.duration.unwrap_or(0.0))
        .with("words", transcript.words.len() as i64)
        .with("diarized", transcript.diarized)
        .took(elapsed)
        .dispatch();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, &str)]) -> Config {
        let config = Config::new();
        for (key, value) in pairs {
            config.set(key, *value);
        }
        config
    }

    #[test]
    fn defaults_to_whisper_at_openai_and_asks_for_its_key() {
        let settings = TranscribeSettings::resolve_with(&config(&[]), |_| None);
        assert_eq!(settings.provider, "whisper");
        assert_eq!(settings.base_url, whisper::OPENAI_BASE_URL);
        let error = settings.require_key().unwrap_err().to_string();
        assert!(error.contains("OPENAI_API_KEY"), "{error}");
    }

    /// The point of the whisper provider: a faster-whisper server on the LAN
    /// speaks the same protocol and has no key to give.
    #[test]
    fn a_self_hosted_whisper_needs_no_key() {
        let settings = TranscribeSettings::resolve_with(&config(&[]), |name| match name {
            "TRANSCRIBE_BASE_URL" => Some("http://whisper.internal:8000/".into()),
            _ => None,
        });
        assert_eq!(settings.base_url, "http://whisper.internal:8000");
        settings.require_key().unwrap();
    }

    #[test]
    fn deepgram_reads_its_conventional_variable_and_config_wins() {
        let settings = TranscribeSettings::resolve_with(&config(&[]), |name| match name {
            "TRANSCRIBE_PROVIDER" => Some("Deepgram".into()),
            "DEEPGRAM_API_KEY" => Some("dg-key".into()),
            _ => None,
        });
        assert_eq!(settings.provider, "deepgram");
        assert_eq!(settings.api_key.expose(), "dg-key");
        assert_eq!(settings.base_url, deepgram::DEEPGRAM_BASE_URL);

        let settings = TranscribeSettings::resolve_with(
            &config(&[
                ("ai.transcribe.provider", "deepgram"),
                ("ai.transcribe.model", "nova-3"),
                ("ai.transcribe.api_key", "from-config"),
            ]),
            |_| Some("from-env".into()),
        );
        assert_eq!(settings.model.as_deref(), Some("nova-3"));
        assert_eq!(settings.api_key.expose(), "from-config");
    }

    #[test]
    fn an_unknown_provider_names_the_known_ones() {
        let error = Transcription::from_settings(TranscribeSettings {
            provider: "assembly".into(),
            model: None,
            api_key: ApiKey::new("k"),
            base_url: String::new(),
        })
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("whisper") && error.contains("deepgram"), "{error}");
    }
}
