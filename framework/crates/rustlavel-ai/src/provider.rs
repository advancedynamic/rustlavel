//! The provider trait and the text stream every provider produces.

use crate::completion::{Completion, StopReason, Usage};
use crate::request::Request;
use rustlavel_client::stream::{Body, SseReader};
use rustlavel_core::events::Event;
use rustlavel_core::{Json, Result};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

/// A boxed future, which is how this crate spells "async trait method".
///
/// `async fn` in a trait would be neater, but it is not object-safe, and the
/// whole point here is choosing a provider at runtime from configuration.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Something that can answer a [`Request`].
///
/// Three implementations ship with the framework and a fourth, [`crate::Fake`],
/// is what an application's tests talk to.
pub trait Provider: Send + Sync + 'static {
    /// A short, stable name — it goes into the `ai.call` event.
    fn name(&self) -> &'static str;

    /// What to use when the caller did not name a model.
    fn default_model(&self) -> &'static str;

    /// Ask for a complete answer.
    fn complete<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Completion>>;

    /// Ask for an answer as it is written.
    fn stream<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<TextStream>>;
}

/// One decoded piece of a streaming answer.
///
/// Providers interleave text with bookkeeping — token counts arrive at the
/// end, stop reasons in their own frame — so a decoded frame may carry any
/// combination of them, or nothing at all.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamDelta {
    pub text: Option<String>,
    pub usage: Option<Usage>,
    pub stop_reason: Option<StopReason>,
    /// The provider said this was the last frame.
    pub done: bool,
}

impl StreamDelta {
    pub fn text(text: impl Into<String>) -> StreamDelta {
        StreamDelta { text: Some(text.into()), ..StreamDelta::default() }
    }

    pub fn nothing() -> StreamDelta {
        StreamDelta::default()
    }

    pub fn done() -> StreamDelta {
        StreamDelta { done: true, ..StreamDelta::default() }
    }

    pub fn with_usage(mut self, usage: Usage) -> StreamDelta {
        self.usage = Some(usage);
        self
    }

    pub fn with_stop_reason(mut self, reason: StopReason) -> StreamDelta {
        self.stop_reason = Some(reason);
        self
    }
}

/// Turns one wire frame into a [`StreamDelta`].
///
/// The first argument is the SSE event name where there is one; Ollama sends
/// bare JSON lines and passes `None`.
pub type Decoder = fn(Option<&str>, &str) -> Result<StreamDelta>;

/// Where the frames come from.
enum Wire {
    /// Named server-sent events, as Anthropic and OpenAI send them.
    Events(SseReader),
    /// Newline-delimited JSON, as Ollama sends it.
    Lines(LineReader),
    /// Already-decoded deltas, from a fake.
    Scripted(VecDeque<String>),
}

/// A streaming answer, read one delta at a time.
///
/// ```ignore
/// let mut stream = ai.prompt("Tell me a story").stream().await?;
/// while let Some(delta) = stream.next().await? {
///     print!("{delta}");
/// }
/// ```
pub struct TextStream {
    wire: Wire,
    decode: Decoder,
    usage: Usage,
    stop_reason: Option<StopReason>,
    finished: bool,
    /// Set when this stream should report itself to the event bus once done.
    call: Option<Call>,
}

/// What a finished stream reports to the instrumentation bus.
struct Call {
    provider: &'static str,
    model: String,
    started: Instant,
}

impl TextStream {
    /// A stream over server-sent events.
    pub fn events(body: Body, decode: Decoder) -> TextStream {
        TextStream::wrap(Wire::Events(body.events()), decode)
    }

    /// A stream over newline-delimited JSON.
    pub fn lines(body: Body, decode: Decoder) -> TextStream {
        TextStream::wrap(Wire::Lines(LineReader::new(body)), decode)
    }

    /// A stream of deltas decided in advance, for fakes and tests.
    pub fn scripted(deltas: impl IntoIterator<Item = String>) -> TextStream {
        TextStream::wrap(Wire::Scripted(deltas.into_iter().collect()), |_, _| {
            Ok(StreamDelta::nothing())
        })
    }

    fn wrap(wire: Wire, decode: Decoder) -> TextStream {
        TextStream {
            wire,
            decode,
            usage: Usage::default(),
            stop_reason: None,
            finished: false,
            call: None,
        }
    }

    /// Report an `ai.call` event when this stream runs out.
    ///
    /// Only then are the token counts known: providers put them in the last
    /// frame, so a stream cannot be measured when it is opened.
    pub fn measured(mut self, provider: &'static str, model: impl Into<String>) -> TextStream {
        self.call = Some(Call { provider, model: model.into(), started: Instant::now() });
        self
    }

    /// The next piece of text, or `None` at the end of the answer.
    ///
    /// Frames that carry only bookkeeping are consumed silently, so a caller
    /// never has to know what a `message_delta` is.
    pub async fn next(&mut self) -> Result<Option<String>> {
        loop {
            if self.finished {
                return Ok(None);
            }

            let frame = match &mut self.wire {
                Wire::Scripted(deltas) => match deltas.pop_front() {
                    Some(text) => return Ok(Some(text)),
                    None => None,
                },
                Wire::Events(reader) => reader
                    .next()
                    .await?
                    .map(|event| (event.event.clone(), event.data.clone())),
                Wire::Lines(reader) => reader.next_line().await?.map(|line| (None, line)),
            };

            let Some((name, data)) = frame else {
                self.finish();
                return Ok(None);
            };

            if data.trim().is_empty() {
                continue;
            }

            let delta = (self.decode)(name.as_deref(), &data)?;
            if let Some(usage) = delta.usage {
                // Providers report input and output counts in different frames;
                // taking the larger of each keeps a late zero from erasing one.
                self.usage.input_tokens = self.usage.input_tokens.max(usage.input_tokens);
                self.usage.output_tokens = self.usage.output_tokens.max(usage.output_tokens);
            }
            if let Some(reason) = delta.stop_reason {
                self.stop_reason = Some(reason);
            }
            if delta.done {
                self.finish();
                return Ok(None);
            }
            match delta.text {
                Some(text) if !text.is_empty() => return Ok(Some(text)),
                _ => continue,
            }
        }
    }

    /// Read the whole answer, for a caller who wanted streaming for the
    /// latency rather than for the display.
    pub async fn collect(&mut self) -> Result<String> {
        let mut out = String::new();
        while let Some(delta) = self.next().await? {
            out.push_str(&delta);
        }
        Ok(out)
    }

    /// The tokens reported so far. Complete only once the stream has ended.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    pub fn stop_reason(&self) -> Option<&StopReason> {
        self.stop_reason.as_ref()
    }

    fn finish(&mut self) {
        self.finished = true;
        if let Some(call) = self.call.take() {
            record_call(call.provider, &call.model, self.usage, call.started.elapsed(), true);
        }
    }
}

/// Reads a body as newline-delimited JSON.
struct LineReader {
    body: Body,
    pending: String,
    drained: bool,
}

impl LineReader {
    fn new(body: Body) -> LineReader {
        LineReader { body, pending: String::new(), drained: false }
    }

    async fn next_line(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(at) = self.pending.find('\n') {
                let line: String = self.pending.drain(..at + 1).collect();
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                return Ok(Some(line));
            }
            if self.drained {
                // Ollama's last object may arrive without a trailing newline.
                let rest = std::mem::take(&mut self.pending);
                let rest = rest.trim().to_string();
                return Ok(if rest.is_empty() { None } else { Some(rest) });
            }
            match self.body.chunk().await? {
                Some(bytes) => self.pending.push_str(&String::from_utf8_lossy(&bytes)),
                None => self.drained = true,
            }
        }
    }
}

/// Report a call on the instrumentation bus.
///
/// Deliberately narrow: provider, model, token counts and duration. The prompt
/// and the API key never appear here — an event goes to Telescope's storage and
/// to whatever else subscribed, and neither is a place for either.
pub(crate) fn record_call(
    provider: &str,
    model: &str,
    usage: Usage,
    elapsed: Duration,
    streamed: bool,
) {
    if !rustlavel_core::events::has_subscribers() {
        return;
    }
    Event::new("ai.call")
        .with("provider", provider)
        .with("model", model)
        .with("input_tokens", usage.input_tokens)
        .with("output_tokens", usage.output_tokens)
        .with("streamed", streamed)
        .took(elapsed)
        .dispatch();
}

/// Read a number from a provider's usage block, whatever it calls it.
pub(crate) fn token_count(value: &Json, path: &str) -> u32 {
    value.get(path).and_then(Json::as_i64).unwrap_or(0).max(0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_http::{Headers, Status};

    fn body(text: &str) -> Body {
        Body::from_bytes(Status::OK, Headers::new(), text.as_bytes().to_vec())
    }

    /// A decoder in the shape every real provider's is: text in `t`, counts in
    /// `n`, and a `done` flag.
    fn decode(_name: Option<&str>, data: &str) -> Result<StreamDelta> {
        let value = Json::parse(data)?;
        if value.get("done").and_then(Json::as_bool) == Some(true) {
            return Ok(StreamDelta::done().with_usage(Usage::new(11, 22)));
        }
        Ok(StreamDelta::text(value.get("t").and_then(Json::as_str).unwrap_or("")))
    }

    #[tokio::test]
    async fn a_scripted_stream_hands_back_its_deltas_in_order() {
        let mut stream = TextStream::scripted(["Hel".to_string(), "lo".to_string()]);

        assert_eq!(stream.next().await.unwrap().as_deref(), Some("Hel"));
        assert_eq!(stream.next().await.unwrap().as_deref(), Some("lo"));
        assert!(stream.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_event_stream_skips_bookkeeping_frames_and_keeps_the_usage() {
        let source = "data: {\"t\":\"Hel\"}\n\ndata: {\"t\":\"\"}\n\ndata: {\"t\":\"lo\"}\n\n\
                      data: {\"done\":true}\n\n";
        let mut stream = TextStream::events(body(source), decode);

        assert_eq!(stream.collect().await.unwrap(), "Hello");
        assert_eq!(stream.usage(), Usage::new(11, 22));
    }

    #[tokio::test]
    async fn a_line_stream_reads_newline_delimited_json_including_an_unterminated_last_line() {
        let source = "{\"t\":\"one \"}\n{\"t\":\"two\"}\n\n{\"done\":true}";
        let mut stream = TextStream::lines(body(source), decode);

        assert_eq!(stream.collect().await.unwrap(), "one two");
        assert_eq!(stream.usage().total(), 33);
    }

    #[tokio::test]
    async fn a_finished_stream_stays_finished() {
        let mut stream = TextStream::scripted(["only".to_string()]);

        assert!(stream.next().await.unwrap().is_some());
        assert!(stream.next().await.unwrap().is_none());
        assert!(stream.next().await.unwrap().is_none());
    }

    #[test]
    fn token_counts_default_to_zero_rather_than_failing() {
        let value = Json::object([("usage", Json::object([("input_tokens", 12.into())]))]);

        assert_eq!(token_count(&value, "usage.input_tokens"), 12);
        assert_eq!(token_count(&value, "usage.output_tokens"), 0);
    }
}
