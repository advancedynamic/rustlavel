//! Against a real Whisper-protocol server. Set `WHISPER_TEST_URL` to a
//! faster-whisper server (`fedirz/faster-whisper-server`) and
//! `WHISPER_TEST_AUDIO` to a spoken clip; skipped otherwise.

use rustlavel_ai::transcribe::{Audio, Transcription, Whisper};

#[tokio::test]
async fn a_self_hosted_whisper_answers_with_timed_words() {
    let (Ok(url), Ok(audio)) = (std::env::var("WHISPER_TEST_URL"), std::env::var("WHISPER_TEST_AUDIO")) else {
        println!("skipped: set WHISPER_TEST_URL and WHISPER_TEST_AUDIO");
        return;
    };
    let model = std::env::var("WHISPER_TEST_MODEL").unwrap_or_else(|_| "Systran/faster-whisper-tiny".into());
    let client = rustlavel_client::Client::new().timeout(std::time::Duration::from_secs(600));
    let speech = Transcription::new(Whisper::local(url).model(&model).client(client));

    let transcript = speech
        .transcribe(Audio::from_path(&audio).await.unwrap())
        .language("en")
        .run()
        .await
        .unwrap();

    println!("text: {}", transcript.text);
    println!("language: {:?}, duration: {:?}", transcript.language, transcript.duration);
    for word in &transcript.words {
        println!("  {:6.2} – {:6.2}  {}", word.start, word.end, word.text);
    }
    print!("{}", transcript.to_srt());

    assert_eq!(transcript.model, model);
    assert!(transcript.text.to_lowercase().contains("hello"), "{}", transcript.text);
    assert!(transcript.words.len() >= 5, "{} words", transcript.words.len());
    let mut last = 0.0;
    for word in &transcript.words {
        assert!(word.start >= last - 0.01, "words are out of order at {word:?}");
        assert!(word.end >= word.start);
        last = word.start;
    }
    assert!(!transcript.segments.is_empty());
    assert!(!transcript.diarized);

    // The clip's second sentence is what a clipping tool asks for.
    let second = transcript.words_between(2.0, 4.0);
    assert!(!second.is_empty());
}

/// The hint field is accepted by the server, and the fake-friendly path of
/// no language set makes it detect one.
#[tokio::test]
async fn a_hint_and_language_detection_are_accepted_by_the_server() {
    let (Ok(url), Ok(audio)) = (std::env::var("WHISPER_TEST_URL"), std::env::var("WHISPER_TEST_AUDIO")) else {
        return;
    };
    let model = std::env::var("WHISPER_TEST_MODEL").unwrap_or_else(|_| "Systran/faster-whisper-tiny".into());
    let client = rustlavel_client::Client::new().timeout(std::time::Duration::from_secs(600));
    let speech = Transcription::new(Whisper::local(url).model(&model).client(client));

    let transcript = speech
        .transcribe(Audio::from_path(&audio).await.unwrap())
        .prompt("Rustlavel, a Rust web framework")
        .run()
        .await
        .unwrap();
    println!("with hint: {} ({:?})", transcript.text, transcript.language);
    assert_eq!(transcript.language.as_deref(), Some("en"), "the language was not detected");
    assert!(!transcript.words.is_empty());
}
