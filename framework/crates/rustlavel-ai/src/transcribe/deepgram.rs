//! Deepgram's `/v1/listen`.
//!
//! `POST {base}/v1/listen?model=…&diarize=true…` with the audio as the raw
//! body and `Authorization: Token …`. The answer nests the words under
//! `results.channels[0].alternatives[0]`, each with its time, confidence and
//! — when `diarize=true` — its speaker; `utterances=true` adds the segments
//! already grouped by speaker.

use super::transcript::{Segment, Transcript, Word, segments_from_words};
use super::{TranscribeRequest, Transcriber};
use crate::config::ApiKey;
use crate::provider::BoxFuture;
use rustlavel_client::Client;
use rustlavel_core::{Error, Json, Result};

pub const DEEPGRAM_BASE_URL: &str = "https://api.deepgram.com";
pub const DEEPGRAM_DEFAULT_MODEL: &str = "nova-3";

pub struct Deepgram {
    client: Client,
    key: ApiKey,
    base_url: String,
    model: String,
}

impl Deepgram {
    pub fn new(key: impl Into<ApiKey>) -> Deepgram {
        Deepgram {
            client: Client::new(),
            key: key.into(),
            base_url: DEEPGRAM_BASE_URL.to_string(),
            model: DEEPGRAM_DEFAULT_MODEL.to_string(),
        }
    }

    pub fn client(mut self, client: Client) -> Deepgram {
        self.client = client;
        self
    }

    pub fn base_url(mut self, base_url: impl Into<String>) -> Deepgram {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Deepgram {
        self.model = model.into();
        self
    }

    fn endpoint(&self, request: &TranscribeRequest, model: &str) -> String {
        let mut query = vec![
            format!("model={}", encode(model)),
            "smart_format=true".to_string(),
            "punctuate=true".to_string(),
            "utterances=true".to_string(),
        ];
        match &request.language {
            Some(language) => query.push(format!("language={}", encode(language))),
            None => query.push("detect_language=true".to_string()),
        }
        if request.diarize {
            query.push("diarize=true".to_string());
        }
        if let Some(prompt) = &request.prompt {
            // Deepgram's hint is a keyword list, not free text.
            for term in prompt.split([',', '\n']).map(str::trim).filter(|t| !t.is_empty()) {
                query.push(format!("keyterm={}", encode(term)));
            }
        }
        format!("{}/v1/listen?{}", self.base_url, query.join("&"))
    }
}

impl Transcriber for Deepgram {
    fn name(&self) -> &'static str {
        "deepgram"
    }

    fn default_model(&self) -> &str {
        &self.model
    }

    fn transcribe<'a>(&'a self, request: &'a TranscribeRequest) -> BoxFuture<'a, Result<Transcript>> {
        Box::pin(async move {
            let model = request.model.clone().unwrap_or_else(|| self.model.clone());
            let response = self
                .client
                .post(self.endpoint(request, &model))
                .header("authorization", format!("Token {}", self.key.expose()))
                .header("content-type", &request.audio.mime)
                .body(request.audio.bytes.clone())
                .send()
                .await
                .and_then(rustlavel_client::ClientResponse::error_for_status)
                .map_err(|error| self.key.scrub_error(error))?;
            let mut transcript = parse(&response.json()?, request.diarize)?;
            transcript.model = model;
            Ok(transcript)
        })
    }
}

/// Read a `/v1/listen` answer. `diarized` is what was asked for; the answer
/// carries speakers only then.
pub fn parse(json: &Json, diarized: bool) -> Result<Transcript> {
    let channel = json
        .get("results.channels")
        .and_then(Json::as_array)
        .and_then(|channels| channels.first())
        .ok_or_else(|| Error::msg("the Deepgram answer has no `results.channels`"))?;
    let alternative = channel
        .get("alternatives")
        .and_then(Json::as_array)
        .and_then(|alternatives| alternatives.first())
        .ok_or_else(|| Error::msg("the Deepgram answer has no alternative"))?;

    let text = alternative.get("transcript").and_then(Json::as_str).unwrap_or("").trim().to_string();
    let words: Vec<Word> = alternative
        .get("words")
        .and_then(Json::as_array)
        .unwrap_or(&[])
        .iter()
        .filter_map(|w| {
            // `punctuated_word` carries the capital and the full stop
            // smart_format added; `word` is bare.
            let text = w
                .get("punctuated_word")
                .and_then(Json::as_str)
                .or_else(|| w.get("word").and_then(Json::as_str))?;
            Some(Word {
                text: text.to_string(),
                start: w.get("start")?.as_f64()?,
                end: w.get("end")?.as_f64()?,
                speaker: w.get("speaker").and_then(Json::as_i64).map(|s| s.max(0) as u32),
                confidence: w.get("confidence").and_then(Json::as_f64),
            })
        })
        .collect();

    let mut segments: Vec<Segment> = json
        .get("results.utterances")
        .and_then(Json::as_array)
        .unwrap_or(&[])
        .iter()
        .filter_map(|u| {
            Some(Segment {
                text: u.get("transcript")?.as_str()?.trim().to_string(),
                start: u.get("start")?.as_f64()?,
                end: u.get("end")?.as_f64()?,
                speaker: u.get("speaker").and_then(Json::as_i64).map(|s| s.max(0) as u32),
            })
        })
        .collect();
    if segments.is_empty() {
        segments = segments_from_words(&words);
    }

    Ok(Transcript {
        text,
        language: channel
            .get("detected_language")
            .and_then(Json::as_str)
            .map(|l| l.to_lowercase()),
        duration: json.get("metadata.duration").and_then(Json::as_f64),
        segments,
        words,
        diarized,
        model: String::new(),
        provider: "deepgram".to_string(),
    })
}

/// Query-string percent-encoding, for a model name or a keyterm.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcribe::{Audio, Transcription};
    use rustlavel_client::fake::{Fake, FakeResponse};

    const ANSWER: &str = r#"{
        "metadata": {"duration": 2.7, "channels": 1, "models": ["abc"]},
        "results": {
            "channels": [{
                "detected_language": "id",
                "alternatives": [{
                    "transcript": "Halo semua. Apa kabar hari ini?",
                    "confidence": 0.98,
                    "words": [
                        {"word": "halo", "start": 0.0, "end": 0.4, "confidence": 0.99, "speaker": 0, "punctuated_word": "Halo"},
                        {"word": "semua", "start": 0.5, "end": 0.9, "confidence": 0.97, "speaker": 0, "punctuated_word": "semua."},
                        {"word": "apa", "start": 1.2, "end": 1.4, "confidence": 0.95, "speaker": 1, "punctuated_word": "Apa"},
                        {"word": "kabar", "start": 1.5, "end": 1.8, "confidence": 0.96, "speaker": 1, "punctuated_word": "kabar"},
                        {"word": "hari", "start": 1.9, "end": 2.2, "confidence": 0.99, "speaker": 1, "punctuated_word": "hari"},
                        {"word": "ini", "start": 2.3, "end": 2.7, "confidence": 0.98, "speaker": 1, "punctuated_word": "ini?"}
                    ]
                }]
            }],
            "utterances": [
                {"start": 0.0, "end": 0.9, "confidence": 0.98, "channel": 0, "transcript": "Halo semua.", "speaker": 0, "id": "u1"},
                {"start": 1.2, "end": 2.7, "confidence": 0.97, "channel": 0, "transcript": "Apa kabar hari ini?", "speaker": 1, "id": "u2"}
            ]
        }
    }"#;

    #[test]
    fn reads_words_with_speakers_and_utterances_as_segments() {
        let transcript = parse(&Json::parse(ANSWER).unwrap(), true).unwrap();
        assert_eq!(transcript.text, "Halo semua. Apa kabar hari ini?");
        assert_eq!(transcript.language.as_deref(), Some("id"));
        assert_eq!(transcript.duration, Some(2.7));
        assert_eq!(transcript.words[1].text, "semua.", "the punctuated word was not preferred");
        assert_eq!(transcript.words[2].speaker, Some(1));
        assert_eq!(transcript.words[2].confidence, Some(0.95));
        assert_eq!(transcript.segments.len(), 2);
        assert_eq!(transcript.segments[1].speaker, Some(1));
        assert!(transcript.diarized);
        assert_eq!(transcript.speakers(), 2);
        assert!(transcript.to_srt().contains("[Speaker 2] Apa kabar hari ini?"));
    }

    #[tokio::test]
    async fn sends_the_audio_raw_with_the_options_in_the_query() {
        let fake = Fake::new().on("*/v1/listen*", FakeResponse::json(Json::parse(ANSWER).unwrap()));
        let client = Client::new().faking(fake);
        let speech = Transcription::new(Deepgram::new("dg-key").client(client.clone()));

        let transcript = speech
            .transcribe(Audio::new(vec![1, 2, 3], "clip.m4a"))
            .language("id")
            .diarize()
            .prompt("Rustlavel, Videotto")
            .run()
            .await
            .unwrap();
        assert_eq!(transcript.model, "nova-3");

        let sent = client.fake().unwrap().recorded().remove(0);
        assert!(sent.url.starts_with("https://api.deepgram.com/v1/listen?"), "{}", sent.url);
        for part in ["model=nova-3", "diarize=true", "language=id", "utterances=true", "keyterm=Rustlavel", "keyterm=Videotto"] {
            assert!(sent.url.contains(part), "missing {part} in {}", sent.url);
        }
        assert!(!sent.url.contains("detect_language"), "asked to detect a language that was given");
        assert_eq!(sent.headers.get("authorization"), Some("Token dg-key"));
        assert_eq!(sent.headers.get("content-type"), Some("audio/mp4"));
        assert_eq!(sent.body, vec![1, 2, 3], "the audio was wrapped rather than sent raw");
    }

    #[tokio::test]
    async fn without_a_language_the_provider_is_asked_to_detect_one() {
        let fake = Fake::new().fallback(FakeResponse::json(Json::parse(ANSWER).unwrap()));
        let client = Client::new().faking(fake);
        let speech = Transcription::new(Deepgram::new("dg-key").client(client.clone())).using("nova-2");
        let transcript = speech.transcribe(Audio::new(vec![1], "a.mp3")).run().await.unwrap();
        assert_eq!(transcript.model, "nova-2");
        assert!(!transcript.diarized, "speakers were claimed without being asked for");
        let sent = client.fake().unwrap().recorded().remove(0);
        assert!(sent.url.contains("detect_language=true") && sent.url.contains("model=nova-2"), "{}", sent.url);
    }

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(encode("nova-3"), "nova-3");
        assert_eq!(encode("a b/c"), "a%20b%2Fc");
    }
}
