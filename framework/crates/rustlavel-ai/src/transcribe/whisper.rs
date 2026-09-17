//! OpenAI's `/v1/audio/transcriptions` — and everything that copies it.
//!
//! `POST {base}/v1/audio/transcriptions`, multipart, bearer token, and with
//! `response_format=verbose_json` plus `timestamp_granularities[]=word` and
//! `=segment` the answer carries every word's time. Self-hosted
//! `faster-whisper-server` and `speaches` serve the same endpoint, so a GPU
//! box on the LAN is `Whisper::local("http://whisper.internal:8000")`.

use super::transcript::{Segment, Transcript, Word, segments_from_words};
use super::{TranscribeRequest, Transcriber, multipart::Multipart};
use crate::config::ApiKey;
use crate::provider::BoxFuture;
use rustlavel_client::Client;
use rustlavel_core::{Error, Json, Result};

pub const OPENAI_BASE_URL: &str = "https://api.openai.com";
pub const WHISPER_DEFAULT_MODEL: &str = "whisper-1";

pub struct Whisper {
    client: Client,
    key: ApiKey,
    base_url: String,
    model: String,
}

impl Whisper {
    /// OpenAI, with a key.
    pub fn new(key: impl Into<ApiKey>) -> Whisper {
        Whisper {
            client: Client::new(),
            key: key.into(),
            base_url: OPENAI_BASE_URL.to_string(),
            model: WHISPER_DEFAULT_MODEL.to_string(),
        }
    }

    /// A self-hosted server speaking the same protocol, with no key. The
    /// default model is left to the server — faster-whisper names them
    /// `Systran/faster-whisper-large-v3` and the like — so set one with
    /// [`Whisper::model`] if the server insists.
    pub fn local(base_url: impl Into<String>) -> Whisper {
        Whisper::new(ApiKey::default()).base_url(base_url)
    }

    pub fn client(mut self, client: Client) -> Whisper {
        self.client = client;
        self
    }

    pub fn base_url(mut self, base_url: impl Into<String>) -> Whisper {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    /// The model used when a request names none.
    pub fn model(mut self, model: impl Into<String>) -> Whisper {
        self.model = model.into();
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/audio/transcriptions", self.base_url)
    }
}

impl Transcriber for Whisper {
    fn name(&self) -> &'static str {
        "whisper"
    }

    fn default_model(&self) -> &str {
        &self.model
    }

    fn transcribe<'a>(&'a self, request: &'a TranscribeRequest) -> BoxFuture<'a, Result<Transcript>> {
        Box::pin(async move {
            if request.diarize {
                return Err(Error::msg(
                    "whisper does not tell speakers apart. Use the deepgram provider with `.diarize()`, \
                     or drop `.diarize()` and take the words without speakers.",
                ));
            }
            let model = request.model.clone().unwrap_or_else(|| self.model.clone());
            let mut form = Multipart::new()
                .text("model", &model)
                .text("response_format", "verbose_json")
                .text("timestamp_granularities[]", "word")
                .text("timestamp_granularities[]", "segment");
            if let Some(language) = &request.language {
                form = form.text("language", language);
            }
            if let Some(prompt) = &request.prompt {
                form = form.text("prompt", prompt);
            }
            let (content_type, body) =
                form.file("file", &request.audio.file_name, &request.audio.mime, &request.audio.bytes).finish();

            let mut http = self.client.post(self.endpoint()).header("content-type", &content_type).body(body);
            if !self.key.is_empty() {
                http = http.bearer(self.key.expose());
            }
            let response = http
                .send()
                .await
                .and_then(rustlavel_client::ClientResponse::error_for_status)
                .map_err(|error| self.key.scrub_error(error))?;
            let mut transcript = parse(&response.json()?)?;
            transcript.model = model;
            Ok(transcript)
        })
    }
}

/// Read a `verbose_json` answer.
pub fn parse(json: &Json) -> Result<Transcript> {
    let text = json
        .get("text")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::msg("the transcription answer has no `text`"))?
        .trim()
        .to_string();

    let words: Vec<Word> = json
        .get("words")
        .and_then(Json::as_array)
        .unwrap_or(&[])
        .iter()
        .filter_map(|w| {
            Some(Word {
                text: w.get("word")?.as_str()?.trim().to_string(),
                start: w.get("start")?.as_f64()?,
                end: w.get("end")?.as_f64()?,
                speaker: None,
                confidence: None,
            })
        })
        .collect();

    let mut segments: Vec<Segment> = json
        .get("segments")
        .and_then(Json::as_array)
        .unwrap_or(&[])
        .iter()
        .filter_map(|s| {
            Some(Segment {
                text: s.get("text")?.as_str()?.trim().to_string(),
                start: s.get("start")?.as_f64()?,
                end: s.get("end")?.as_f64()?,
                speaker: None,
            })
        })
        .collect();
    if segments.is_empty() {
        segments = segments_from_words(&words);
    }

    Ok(Transcript {
        text,
        language: json.get("language").and_then(Json::as_str).map(normalise_language),
        duration: json.get("duration").and_then(Json::as_f64),
        segments,
        words,
        diarized: false,
        model: String::new(),
        provider: "whisper".to_string(),
    })
}

/// OpenAI answers `"indonesian"`, `"english"` — the language's name, not its
/// code. faster-whisper answers the code. Callers get the code either way.
fn normalise_language(language: &str) -> String {
    let lower = language.trim().to_lowercase();
    match lower.as_str() {
        "indonesian" => "id",
        "english" => "en",
        "malay" => "ms",
        "japanese" => "ja",
        "korean" => "ko",
        "chinese" | "mandarin" => "zh",
        "spanish" => "es",
        "french" => "fr",
        "german" => "de",
        "portuguese" => "pt",
        "arabic" => "ar",
        "hindi" => "hi",
        "javanese" => "jv",
        "sundanese" => "su",
        "thai" => "th",
        "vietnamese" => "vi",
        "tagalog" | "filipino" => "tl",
        _ => return lower,
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcribe::{Audio, Transcription};
    use rustlavel_client::fake::{Fake, FakeResponse};

    const ANSWER: &str = r#"{
        "task": "transcribe",
        "language": "indonesian",
        "duration": 2.7,
        "text": " Halo semua. Apa kabar hari ini? ",
        "words": [
            {"word": "Halo", "start": 0.0, "end": 0.4},
            {"word": "semua.", "start": 0.5, "end": 0.9},
            {"word": "Apa", "start": 1.2, "end": 1.4},
            {"word": "kabar", "start": 1.5, "end": 1.8},
            {"word": "hari", "start": 1.9, "end": 2.2},
            {"word": "ini?", "start": 2.3, "end": 2.7}
        ],
        "segments": [
            {"id": 0, "start": 0.0, "end": 0.9, "text": " Halo semua.", "avg_logprob": -0.2},
            {"id": 1, "start": 1.2, "end": 2.7, "text": " Apa kabar hari ini?", "avg_logprob": -0.3}
        ]
    }"#;

    #[test]
    fn reads_words_segments_and_the_language_as_a_code() {
        let transcript = parse(&Json::parse(ANSWER).unwrap()).unwrap();
        assert_eq!(transcript.text, "Halo semua. Apa kabar hari ini?");
        assert_eq!(transcript.language.as_deref(), Some("id"));
        assert_eq!(transcript.duration, Some(2.7));
        assert_eq!(transcript.words.len(), 6);
        assert_eq!(transcript.words[3].text, "kabar");
        assert_eq!(transcript.words[3].start, 1.5);
        assert_eq!(transcript.segments.len(), 2);
        assert_eq!(transcript.segments[1].text, "Apa kabar hari ini?");
        assert!(!transcript.diarized);
        assert_eq!(transcript.speakers(), 0);
    }

    /// A server that returns words but no segments — some self-hosted ones
    /// do — still yields subtitles.
    #[test]
    fn segments_are_built_when_the_server_sends_none() {
        let mut json = Json::parse(ANSWER).unwrap();
        if let Json::Object(map) = &mut json {
            map.remove("segments");
        }
        let transcript = parse(&json).unwrap();
        assert_eq!(transcript.segments.len(), 2, "{:?}", transcript.segments);
        assert_eq!(transcript.segments[0].text, "Halo semua.");
    }

    #[tokio::test]
    async fn sends_a_multipart_request_with_word_timestamps_asked_for() {
        let fake = Fake::new().on("*/v1/audio/transcriptions", FakeResponse::json(Json::parse(ANSWER).unwrap()));
        let client = Client::new().faking(fake);
        let whisper = Whisper::new("sk-test").client(client.clone());
        let speech = Transcription::new(whisper);

        let transcript = speech
            .transcribe(Audio::new(vec![1, 2, 3], "clip.mp3"))
            .language("id")
            .prompt("Rustlavel, Videotto")
            .run()
            .await
            .unwrap();
        assert_eq!(transcript.model, "whisper-1");
        assert_eq!(transcript.words.len(), 6);

        let sent = client.fake().unwrap().recorded().remove(0);
        assert_eq!(sent.url, "https://api.openai.com/v1/audio/transcriptions");
        assert_eq!(sent.headers.get("authorization"), Some("Bearer sk-test"));
        let content_type = sent.headers.get("content-type").unwrap();
        assert!(content_type.starts_with("multipart/form-data; boundary="), "{content_type}");
        let body = sent.body_text();
        for field in [
            "name=\"model\"\r\n\r\nwhisper-1",
            "name=\"response_format\"\r\n\r\nverbose_json",
            "name=\"timestamp_granularities[]\"\r\n\r\nword",
            "name=\"timestamp_granularities[]\"\r\n\r\nsegment",
            "name=\"language\"\r\n\r\nid",
            "name=\"prompt\"\r\n\r\nRustlavel, Videotto",
            "name=\"file\"; filename=\"clip.mp3\"\r\nContent-Type: audio/mpeg",
        ] {
            assert!(body.contains(field), "missing {field:?} in\n{body}");
        }
        assert!(sent.body.windows(3).any(|w| w == [1, 2, 3]));
    }

    #[tokio::test]
    async fn a_local_server_gets_no_authorization_header_and_its_own_model() {
        let fake = Fake::new().fallback(FakeResponse::json(Json::parse(ANSWER).unwrap()));
        let client = Client::new().faking(fake);
        let whisper = Whisper::local("http://whisper.internal:8000/").model("Systran/faster-whisper-large-v3").client(client.clone());

        let transcript = Transcription::new(whisper).transcribe(Audio::new(vec![9], "a.wav")).run().await.unwrap();
        assert_eq!(transcript.model, "Systran/faster-whisper-large-v3");

        let sent = client.fake().unwrap().recorded().remove(0);
        assert_eq!(sent.url, "http://whisper.internal:8000/v1/audio/transcriptions");
        assert!(sent.headers.get("authorization").is_none(), "a key was sent that does not exist");
        assert!(sent.body_text().contains("Systran/faster-whisper-large-v3"));
    }

    #[tokio::test]
    async fn asking_whisper_for_speakers_is_refused_before_any_upload() {
        let client = Client::new().faking(Fake::new());
        let speech = Transcription::new(Whisper::new("k").client(client.clone()));
        let error = speech.transcribe(Audio::new(vec![1], "a.mp3")).diarize().run().await.unwrap_err().to_string();
        assert!(error.contains("deepgram"), "{error}");
        assert_eq!(client.fake().unwrap().count(), 0, "the audio was uploaded anyway");
    }

    #[tokio::test]
    async fn an_error_body_never_carries_the_key() {
        let fake = Fake::new().fallback(FakeResponse::text("invalid key sk-secret-123").status(401));
        let client = Client::new().faking(fake);
        let speech = Transcription::new(Whisper::new("sk-secret-123").client(client));
        let error = speech.transcribe(Audio::new(vec![1], "a.mp3")).run().await.unwrap_err().to_string();
        assert!(!error.contains("sk-secret-123"), "{error}");
    }
}
