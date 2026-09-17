//! A transcriber that answers from a script.

use super::transcript::{Transcript, Word, segments_from_words};
use super::{TranscribeRequest, Transcriber};
use crate::provider::BoxFuture;
use rustlavel_core::{Error, Result};
use std::sync::Mutex;

/// Scripted answers, in order, and a record of what was asked.
///
/// ```ignore
/// let speech = Transcription::fake(FakeTranscriber::new().saying("Halo semua. Apa kabar"));
/// let transcript = speech.transcribe(audio).run().await?;
/// transcript.words[2].start   // 1.0 — words are laid out half a second each
/// ```
#[derive(Default)]
pub struct FakeTranscriber {
    answers: Mutex<Vec<Transcript>>,
    requests: Mutex<Vec<TranscribeRequest>>,
}

impl FakeTranscriber {
    pub fn new() -> FakeTranscriber {
        FakeTranscriber::default()
    }

    /// Answer with this text, one word every half second, so a test that
    /// cuts on timestamps has some to cut on.
    pub fn saying(self, text: &str) -> FakeTranscriber {
        let words: Vec<Word> = text
            .split_whitespace()
            .enumerate()
            .map(|(i, word)| Word {
                text: word.to_string(),
                start: i as f64 * 0.5,
                end: i as f64 * 0.5 + 0.4,
                speaker: None,
                confidence: Some(1.0),
            })
            .collect();
        let duration = words.last().map(|w| w.end);
        self.returning(Transcript {
            text: text.to_string(),
            language: None,
            duration,
            segments: segments_from_words(&words),
            words,
            diarized: false,
            model: "fake".to_string(),
            provider: "fake".to_string(),
        })
    }

    /// Answer with exactly this.
    pub fn returning(self, transcript: Transcript) -> FakeTranscriber {
        self.answers.lock().expect("fake lock poisoned").push(transcript);
        self
    }

    /// What was asked, in order.
    pub fn requests(&self) -> Vec<TranscribeRequest> {
        self.requests.lock().expect("fake lock poisoned").clone()
    }
}

impl Transcriber for FakeTranscriber {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn default_model(&self) -> &str {
        "fake"
    }

    fn transcribe<'a>(&'a self, request: &'a TranscribeRequest) -> BoxFuture<'a, Result<Transcript>> {
        Box::pin(async move {
            self.requests.lock().expect("fake lock poisoned").push(request.clone());
            let mut answers = self.answers.lock().expect("fake lock poisoned");
            if answers.is_empty() {
                return Err(Error::msg("the fake transcriber has no answer left; add `.saying(…)` for each call"));
            }
            let mut transcript = answers.remove(0);
            if let Some(model) = &request.model {
                transcript.model = model.clone();
            }
            Ok(transcript)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcribe::{Audio, Transcription};

    #[tokio::test]
    async fn the_script_is_answered_in_order_and_then_runs_out() {
        let speech = Transcription::fake(FakeTranscriber::new().saying("Halo semua. Apa kabar"));
        let transcript = speech.transcribe(Audio::new(vec![1], "a.mp3")).language("id").run().await.unwrap();
        assert_eq!(transcript.words.len(), 4);
        assert_eq!(transcript.words[2].start, 1.0);
        assert_eq!(transcript.segments.len(), 2);

        let error = speech.transcribe(Audio::new(vec![1], "b.mp3")).run().await.unwrap_err().to_string();
        assert!(error.contains("no answer left"), "{error}");
    }

    #[tokio::test]
    async fn empty_audio_is_refused_before_the_backend_sees_it() {
        let fake = std::sync::Arc::new(FakeTranscriber::new().saying("x"));
        let speech = Transcription::shared(fake.clone());
        let error = speech.transcribe(Audio::new(Vec::new(), "a.mp3")).run().await.unwrap_err().to_string();
        assert!(error.contains("empty"), "{error}");
        assert!(fake.requests().is_empty());
    }
}
