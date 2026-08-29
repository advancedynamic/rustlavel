//! `Ai::fake()` — a provider that answers from a script.
//!
//! An application's tests must not need an API key, a network, or a model in a
//! good mood. This matters as much as the real providers: a feature nobody can
//! test is a feature nobody dares change.
//!
//! ```ignore
//! let ai = Ai::fake_with(Fake::new().reply("A short summary."));
//! let summary = summarise(&ai, article).await?;
//!
//! ai.faked().unwrap().assert_asked("summarise");
//! ```

use crate::completion::{Completion, ToolCall, Usage};
use crate::provider::{BoxFuture, Provider, TextStream};
use crate::request::Request;
use rustlavel_core::{Error, Json, Result};
use std::collections::VecDeque;
use std::sync::Mutex;

/// One scripted answer.
#[derive(Debug, Clone)]
pub enum Scripted {
    Answer(Box<Completion>),
    /// Text deltas, handed out one at a time by `stream()`.
    Deltas(Vec<String>),
    /// A failure, for testing the unhappy path.
    Failure(String),
}

/// A script of answers, and a record of what was asked.
///
/// Answers are handed out in order. Without a script — or once it runs out —
/// the fallback answers, and without a fallback an unscripted call is an error:
/// a test should not quietly pass because the model said nothing.
#[derive(Debug, Default)]
pub struct Fake {
    script: Mutex<VecDeque<Scripted>>,
    fallback: Mutex<Option<Scripted>>,
    recorded: Mutex<Vec<Request>>,
}

impl Fake {
    pub fn new() -> Fake {
        Fake::default()
    }

    /// Answer the next call with this text.
    pub fn reply(self, text: impl Into<String>) -> Fake {
        self.push(Scripted::Answer(Box::new(Completion::text(text).with_model("fake"))))
    }

    /// Answer the next call with a completion built by hand — for usage counts,
    /// stop reasons, or anything else a test needs to be specific about.
    pub fn reply_with(self, completion: Completion) -> Fake {
        self.push(Scripted::Answer(Box::new(completion)))
    }

    /// Answer the next call by asking for a tool.
    pub fn calls_tool(self, name: impl Into<String>, arguments: Json) -> Fake {
        let call = ToolCall::new("call_fake", name, arguments);
        self.push(Scripted::Answer(Box::new(
            Completion::default().with_model("fake").with_tool_call(call),
        )))
    }

    /// Answer the next `stream()` with these deltas.
    pub fn streams<I, S>(self, deltas: I) -> Fake
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.push(Scripted::Deltas(deltas.into_iter().map(Into::into).collect()))
    }

    /// Fail the next call.
    pub fn fails(self, message: impl Into<String>) -> Fake {
        self.push(Scripted::Failure(message.into()))
    }

    /// Answer anything the script does not cover.
    pub fn always(self, text: impl Into<String>) -> Fake {
        *self.fallback.lock().expect("fake lock poisoned") =
            Some(Scripted::Answer(Box::new(Completion::text(text).with_model("fake"))));
        self
    }

    fn push(self, scripted: Scripted) -> Fake {
        self.script.lock().expect("fake lock poisoned").push_back(scripted);
        self
    }

    /// Every request the fake saw, in order.
    pub fn requests(&self) -> Vec<Request> {
        self.recorded.lock().expect("fake lock poisoned").clone()
    }

    pub fn last_request(&self) -> Option<Request> {
        self.recorded.lock().expect("fake lock poisoned").last().cloned()
    }

    pub fn count(&self) -> usize {
        self.recorded.lock().expect("fake lock poisoned").len()
    }

    /// Whether any prompt or system prompt contained this text.
    pub fn asked(&self, needle: &str) -> bool {
        self.requests().iter().any(|request| {
            request.effective_system().is_some_and(|system| system.contains(needle))
                || request.messages.iter().any(|message| message.text().contains(needle))
        })
    }

    #[track_caller]
    pub fn assert_asked(&self, needle: &str) {
        assert!(
            self.asked(needle),
            "expected a prompt containing {needle:?}; saw {:?}",
            self.requests().iter().filter_map(Request::last_user_text).collect::<Vec<_>>()
        );
    }

    #[track_caller]
    pub fn assert_not_asked(&self, needle: &str) {
        assert!(!self.asked(needle), "did not expect a prompt containing {needle:?}");
    }

    #[track_caller]
    pub fn assert_count(&self, expected: usize) {
        assert_eq!(self.count(), expected, "unexpected number of model calls");
    }

    /// Take the next answer, recording the request that asked for it.
    fn next(&self, request: &Request) -> Result<Scripted> {
        self.recorded.lock().expect("fake lock poisoned").push(request.clone());

        let next = self.script.lock().expect("fake lock poisoned").pop_front();
        match next.or_else(|| self.fallback.lock().expect("fake lock poisoned").clone()) {
            Some(scripted) => Ok(scripted),
            None => Err(Error::msg(
                "the AI fake has no answer scripted for this call. \
                 Add `.reply(\"…\")`, `.streams([…])` or `.always(\"…\")`.",
            )),
        }
    }
}

impl Provider for Fake {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn default_model(&self) -> &'static str {
        "fake"
    }

    fn complete<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move {
            match self.next(request)? {
                Scripted::Answer(completion) => Ok(*completion),
                // A scripted stream answers a non-streaming call too, joined —
                // one script then covers both ways of asking.
                Scripted::Deltas(deltas) => Ok(Completion::text(deltas.concat())
                    .with_model("fake")
                    .with_usage(Usage::new(0, deltas.len() as u32))),
                Scripted::Failure(message) => Err(Error::msg(message)),
            }
        })
    }

    fn stream<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<TextStream>> {
        Box::pin(async move {
            match self.next(request)? {
                Scripted::Deltas(deltas) => Ok(TextStream::scripted(deltas)),
                // A scripted completion streams as one delta.
                Scripted::Answer(completion) => Ok(TextStream::scripted([completion.text.clone()])),
                Scripted::Failure(message) => Err(Error::msg(message)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::StopReason;

    fn asking(text: &str) -> Request {
        Request::new("claude-sonnet-5").user(text)
    }

    #[tokio::test]
    async fn answers_from_the_script_in_order() {
        let fake = Fake::new().reply("first").reply("second");

        assert_eq!(fake.complete(&asking("one")).await.unwrap().text, "first");
        assert_eq!(fake.complete(&asking("two")).await.unwrap().text, "second");
        fake.assert_count(2);
    }

    #[tokio::test]
    async fn records_the_requests_it_saw() {
        let fake = Fake::new().always("ok");

        fake.complete(&Request::new("claude-sonnet-5").system("Be brief.").user("Summarise this"))
            .await
            .unwrap();

        fake.assert_asked("Summarise");
        fake.assert_asked("Be brief.");
        fake.assert_not_asked("translate");

        let seen = fake.last_request().unwrap();
        assert_eq!(seen.model, "claude-sonnet-5");
        assert_eq!(seen.last_user_text().as_deref(), Some("Summarise this"));
    }

    #[tokio::test]
    async fn a_fallback_answers_everything_the_script_did_not() {
        let fake = Fake::new().reply("scripted").always("fallback");

        assert_eq!(fake.complete(&asking("one")).await.unwrap().text, "scripted");
        assert_eq!(fake.complete(&asking("two")).await.unwrap().text, "fallback");
        assert_eq!(fake.complete(&asking("three")).await.unwrap().text, "fallback");
    }

    #[tokio::test]
    async fn an_unscripted_call_fails_loudly() {
        let error = Fake::new().complete(&asking("hello")).await.unwrap_err().to_string();

        assert!(error.contains("no answer scripted"), "{error}");
        assert!(error.contains("reply"), "{error}");
    }

    #[tokio::test]
    async fn scripts_a_stream_and_a_failure() {
        let fake = Fake::new().streams(["Hel", "lo"]).fails("the model is overloaded");

        let mut stream = fake.stream(&asking("hi")).await.unwrap();
        assert_eq!(stream.collect().await.unwrap(), "Hello");

        let error = fake.complete(&asking("again")).await.unwrap_err().to_string();
        assert_eq!(error, "the model is overloaded");
    }

    #[tokio::test]
    async fn one_script_serves_both_ways_of_asking() {
        let streamed = Fake::new().streams(["a", "b"]);
        assert_eq!(streamed.complete(&asking("hi")).await.unwrap().text, "ab");

        let completed = Fake::new().reply("whole");
        let mut stream = completed.stream(&asking("hi")).await.unwrap();
        assert_eq!(stream.collect().await.unwrap(), "whole");
    }

    #[tokio::test]
    async fn scripts_a_tool_call_and_a_hand_built_completion() {
        let fake = Fake::new()
            .calls_tool("get_weather", Json::object([("city", "Oslo".into())]))
            .reply_with(Completion::text("It is 7 degrees.").with_usage(Usage::new(30, 8)));

        let first = fake.complete(&asking("weather?")).await.unwrap();
        assert!(first.wants_tools());
        assert_eq!(first.stop_reason, StopReason::ToolUse);
        assert_eq!(first.tool_calls[0].name, "get_weather");
        assert_eq!(first.tool_calls[0].argument("city").unwrap().as_str(), Some("Oslo"));

        let second = fake.complete(&asking("and now?")).await.unwrap();
        assert_eq!(second.usage, Usage::new(30, 8));
    }
}
