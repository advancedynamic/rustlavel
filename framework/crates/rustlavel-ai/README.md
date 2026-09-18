# rustlavel-ai

Rustlavel AI: one API for Anthropic, OpenAI and Ollama, with streaming, tools and a fake provider — and speech to text with the time of every word, over Whisper or Deepgram.

Part of [Rustlavel](https://github.com/advancedynamic/rustlavel), a full-stack web
framework for Rust written from scratch — no Axum, no hyper, no SeaORM. Tokio is the
only large dependency.

## Using it

Enable it through the meta-crate rather than depending on this one directly, so the
versions stay in step:

```toml
[dependencies]
rustlavel = { version = "0.8", features = ["ai"] }
```

## Speech to text

A transcript for a clipping tool is a list of words and when each was said, so a cut at 42.5 seconds lands between two of them. That is what `Transcript` is; the paragraph is derived from it.

```rust
let speech = Transcription::from_config(&config)?;      // ai.transcribe.* / TRANSCRIBE_*
let transcript = speech
    .transcribe(Audio::from_path("episode.mp4").await?)  // video is fine; the audio track is read server-side
    .language("id")
    .prompt("Rustlavel, Videotto")                        // names the model would otherwise misspell
    .diarize()                                            // Deepgram only; Whisper refuses rather than pretending
    .run()
    .await?;

transcript.words_between(42.5, 71.0);   // what the clip says
transcript.segments_between(42.5, 71.0); // sentence edges to cut on
transcript.to_srt();                     // burn in with ffmpeg -vf subtitles=
transcript.speakers();                   // 2
```

Two wire formats, three deployments: `Whisper` speaks OpenAI's `/v1/audio/transcriptions`, which is also what a **self-hosted faster-whisper server** serves — `Whisper::local("http://whisper.internal:8000")`, no key — and `Deepgram` speaks `/v1/listen` with speakers told apart. Measured against `fedirz/faster-whisper-server` with a spoken clip: thirteen words, each with its time, and the hint turning "Rust Level" into "Rustlavel". `FakeTranscriber::new().saying("…")` lays words out half a second apart for tests.

## Documentation

- [API documentation](https://docs.rs/rustlavel-ai)
- [The framework](https://github.com/advancedynamic/rustlavel), including the roadmap
  and the design rules this crate is written under

## Licence

MIT.
