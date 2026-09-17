//! What comes back: the words, and when they were said.

use rustlavel_core::Json;

/// One word, with the moment it was spoken.
///
/// Times are seconds from the start of the audio. A clip is cut on these, so
/// they are `f64`, not milliseconds rounded somewhere on the way in.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    pub start: f64,
    pub end: f64,
    /// Zero-based speaker index, when the provider was asked to tell speakers
    /// apart and could.
    pub speaker: Option<u32>,
    /// 0–1, when the provider reports one.
    pub confidence: Option<f64>,
}

/// A run of speech: a sentence, an utterance, one speaker's turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub text: String,
    pub start: f64,
    pub end: f64,
    pub speaker: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub text: String,
    /// ISO 639-1 where the provider says (`"id"`, `"en"`); `None` when it
    /// does not.
    pub language: Option<String>,
    /// Length of the audio in seconds, when reported.
    pub duration: Option<f64>,
    pub segments: Vec<Segment>,
    pub words: Vec<Word>,
    /// Whether speakers were told apart. `false` means every `speaker` is
    /// `None` because nobody looked, not because there was one speaker.
    pub diarized: bool,
    pub model: String,
    pub provider: String,
}

impl Transcript {
    /// How many distinct speakers were heard. Zero when not diarized.
    pub fn speakers(&self) -> usize {
        let mut seen: Vec<u32> = self
            .words
            .iter()
            .filter_map(|w| w.speaker)
            .chain(self.segments.iter().filter_map(|s| s.speaker))
            .collect();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    }

    /// The words spoken between two moments — what a clip from `start` to
    /// `end` contains. A word counts when any part of it is inside.
    pub fn words_between(&self, start: f64, end: f64) -> Vec<&Word> {
        self.words.iter().filter(|word| word.end > start && word.start < end).collect()
    }

    /// The segments that overlap a window, for cutting on sentence edges.
    pub fn segments_between(&self, start: f64, end: f64) -> Vec<&Segment> {
        self.segments.iter().filter(|segment| segment.end > start && segment.start < end).collect()
    }

    /// SubRip subtitles, one cue per segment. `ffmpeg -vf subtitles=` burns
    /// this in.
    pub fn to_srt(&self) -> String {
        let mut out = String::new();
        for (index, segment) in self.segments.iter().enumerate() {
            out.push_str(&format!(
                "{}\n{} --> {}\n{}\n\n",
                index + 1,
                timestamp(segment.start, ','),
                timestamp(segment.end, ','),
                labelled(segment)
            ));
        }
        out
    }

    /// WebVTT, for a `<track>` on a web player.
    pub fn to_vtt(&self) -> String {
        let mut out = String::from("WEBVTT\n\n");
        for segment in &self.segments {
            out.push_str(&format!(
                "{} --> {}\n{}\n\n",
                timestamp(segment.start, '.'),
                timestamp(segment.end, '.'),
                labelled(segment)
            ));
        }
        out
    }

    pub fn to_json(&self) -> Json {
        Json::object([
            ("text", Json::from(self.text.as_str())),
            ("language", self.language.as_deref().map_or(Json::Null, Json::from)),
            ("duration", self.duration.map_or(Json::Null, Json::from)),
            ("diarized", Json::from(self.diarized)),
            ("model", Json::from(self.model.as_str())),
            ("provider", Json::from(self.provider.as_str())),
            (
                "segments",
                Json::Array(
                    self.segments
                        .iter()
                        .map(|s| {
                            Json::object([
                                ("text", Json::from(s.text.as_str())),
                                ("start", Json::from(s.start)),
                                ("end", Json::from(s.end)),
                                ("speaker", s.speaker.map_or(Json::Null, |n| Json::from(n as i64))),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "words",
                Json::Array(
                    self.words
                        .iter()
                        .map(|w| {
                            Json::object([
                                ("text", Json::from(w.text.as_str())),
                                ("start", Json::from(w.start)),
                                ("end", Json::from(w.end)),
                                ("speaker", w.speaker.map_or(Json::Null, |n| Json::from(n as i64))),
                                ("confidence", w.confidence.map_or(Json::Null, Json::from)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])
    }
}

/// Segments built from words when a provider gives words but no segments:
/// a new one at each change of speaker, at sentence-ending punctuation, or
/// after a pause of a second.
pub(crate) fn segments_from_words(words: &[Word]) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut current: Option<Segment> = None;
    let mut previous_end = 0.0;

    for word in words {
        let breaks = match &current {
            None => true,
            Some(segment) => segment.speaker != word.speaker || word.start - previous_end > 1.0,
        };
        if breaks && let Some(done) = current.take() {
            segments.push(done);
        }
        match &mut current {
            Some(segment) => {
                segment.text.push(' ');
                segment.text.push_str(&word.text);
                segment.end = word.end;
            }
            None => {
                current = Some(Segment { text: word.text.clone(), start: word.start, end: word.end, speaker: word.speaker });
            }
        }
        previous_end = word.end;
        if word.text.ends_with(['.', '?', '!']) && let Some(done) = current.take() {
            segments.push(done);
        }
    }
    if let Some(done) = current {
        segments.push(done);
    }
    segments
}

fn labelled(segment: &Segment) -> String {
    match segment.speaker {
        Some(speaker) => format!("[Speaker {}] {}", speaker + 1, segment.text),
        None => segment.text.clone(),
    }
}

/// `HH:MM:SS,mmm` (SRT) or `HH:MM:SS.mmm` (VTT).
fn timestamp(seconds: f64, separator: char) -> String {
    let millis = (seconds.max(0.0) * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02}{separator}{:03}",
        millis / 3_600_000,
        millis / 60_000 % 60,
        millis / 1000 % 60,
        millis % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, start: f64, end: f64, speaker: Option<u32>) -> Word {
        Word { text: text.into(), start, end, speaker, confidence: None }
    }

    #[test]
    fn timestamps_are_subtitle_shaped() {
        assert_eq!(timestamp(0.0, ','), "00:00:00,000");
        assert_eq!(timestamp(61.5, ','), "00:01:01,500");
        assert_eq!(timestamp(3725.042, '.'), "01:02:05.042");
    }

    #[test]
    fn segments_break_on_speaker_punctuation_and_pause() {
        let words = vec![
            word("Halo", 0.0, 0.4, Some(0)),
            word("semua.", 0.5, 0.9, Some(0)),
            word("Apa", 1.0, 1.2, Some(0)),
            word("kabar", 1.3, 1.6, Some(1)),
            word("hari", 3.0, 3.3, Some(1)),
            word("ini", 3.4, 3.6, Some(1)),
        ];
        let segments = segments_from_words(&words);
        let texts: Vec<&str> = segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["Halo semua.", "Apa", "kabar", "hari ini"]);
        assert_eq!(segments[3].start, 3.0);
        assert_eq!(segments[3].end, 3.6);
        assert_eq!(segments[3].speaker, Some(1));
    }

    #[test]
    fn a_clip_window_keeps_words_that_touch_it() {
        let transcript = Transcript {
            text: String::new(),
            language: None,
            duration: None,
            segments: vec![],
            words: vec![word("a", 0.0, 1.0, None), word("b", 1.0, 2.0, None), word("c", 2.5, 3.0, None)],
            diarized: false,
            model: String::new(),
            provider: String::new(),
        };
        let inside: Vec<&str> = transcript.words_between(0.9, 2.2).iter().map(|w| w.text.as_str()).collect();
        assert_eq!(inside, ["a", "b"]);
        assert_eq!(transcript.speakers(), 0);
    }

    #[test]
    fn srt_and_vtt_label_speakers_when_there_are_any() {
        let transcript = Transcript {
            text: "Halo semua. Apa kabar".into(),
            language: Some("id".into()),
            duration: Some(2.0),
            segments: vec![
                Segment { text: "Halo semua.".into(), start: 0.0, end: 0.9, speaker: Some(0) },
                Segment { text: "Apa kabar".into(), start: 1.0, end: 1.6, speaker: Some(1) },
            ],
            words: vec![],
            diarized: true,
            model: "m".into(),
            provider: "p".into(),
        };
        let srt = transcript.to_srt();
        assert!(srt.starts_with("1\n00:00:00,000 --> 00:00:00,900\n[Speaker 1] Halo semua.\n\n2\n"), "{srt}");
        let vtt = transcript.to_vtt();
        assert!(vtt.starts_with("WEBVTT\n\n00:00:00.000 --> 00:00:00.900\n[Speaker 1] Halo semua.\n"), "{vtt}");
        assert_eq!(transcript.speakers(), 2);
    }
}
