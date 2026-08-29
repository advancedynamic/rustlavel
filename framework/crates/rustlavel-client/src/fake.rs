//! Faking outbound requests.
//!
//! This is `Http::fake()`. An application's tests must not depend on a third
//! party being up, on network latency, or on a rate limit — and a test that
//! calls a paid API for real is a test nobody runs twice.

use crate::{ClientResponse, RequestBuilder};
use rustlavel_core::{Error, Json, Result};
use rustlavel_http::{Headers, Method, Status};
use std::sync::Mutex;

/// A scripted answer.
#[derive(Debug, Clone)]
pub struct FakeResponse {
    pub status: Status,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl FakeResponse {
    pub fn json(value: Json) -> Self {
        let mut headers = Headers::new();
        headers.set("content-type", "application/json");
        FakeResponse { status: Status::OK, headers, body: value.to_string().into_bytes() }
    }

    pub fn text(body: impl Into<String>) -> Self {
        FakeResponse { status: Status::OK, headers: Headers::new(), body: body.into().into_bytes() }
    }

    /// A server-sent event stream, for testing streaming responses.
    pub fn events(chunks: &[&str]) -> Self {
        let mut headers = Headers::new();
        headers.set("content-type", "text/event-stream");
        let body = chunks.iter().map(|c| format!("data: {c}\n\n")).collect::<String>();
        FakeResponse { status: Status::OK, headers, body: body.into_bytes() }
    }

    pub fn status(mut self, status: u16) -> Self {
        self.status = Status(status);
        self
    }
}

/// One request the fake saw.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: Method,
    pub url: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn json(&self) -> Option<Json> {
        Json::parse(&String::from_utf8_lossy(&self.body)).ok()
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A script of URL patterns to responses, plus a record of what was asked.
#[derive(Default)]
pub struct Fake {
    /// Matched in order; the first pattern that matches wins.
    routes: Vec<(String, FakeResponse)>,
    fallback: Option<FakeResponse>,
    recorded: Mutex<Vec<Recorded>>,
}

impl Fake {
    pub fn new() -> Self {
        Fake::default()
    }

    /// Answer any URL containing `pattern`, or matching it with a `*` wildcard.
    pub fn on(mut self, pattern: &str, response: FakeResponse) -> Self {
        self.routes.push((pattern.to_string(), response));
        self
    }

    /// Answer anything not matched above. Without this, an unexpected request
    /// is an error — a test should not silently pass because a call went
    /// somewhere nobody scripted.
    pub fn fallback(mut self, response: FakeResponse) -> Self {
        self.fallback = Some(response);
        self
    }

    pub(crate) fn respond(&self, request: &RequestBuilder) -> Result<ClientResponse> {
        self.recorded.lock().expect("fake lock poisoned").push(Recorded {
            method: request.method(),
            url: request.url().to_string(),
            headers: request.headers().clone(),
            body: request.body_bytes().to_vec(),
        });

        let matched = self
            .routes
            .iter()
            .find(|(pattern, _)| matches(pattern, request.url()))
            .map(|(_, response)| response.clone())
            .or_else(|| self.fallback.clone());

        match matched {
            Some(response) => Ok(ClientResponse {
                status: response.status,
                headers: response.headers,
                body: response.body,
            }),
            None => Err(Error::msg(format!(
                "no fake response is scripted for {} {}. Add `.on(\"…\", …)` or a `.fallback(…)`.",
                request.method(),
                request.url()
            ))),
        }
    }

    /// Every request the fake saw, in order.
    pub fn recorded(&self) -> Vec<Recorded> {
        self.recorded.lock().expect("fake lock poisoned").clone()
    }

    pub fn count(&self) -> usize {
        self.recorded.lock().expect("fake lock poisoned").len()
    }

    /// Whether a request matching this pattern was sent.
    pub fn sent(&self, pattern: &str) -> bool {
        self.recorded().iter().any(|request| matches(pattern, &request.url))
    }

    #[track_caller]
    pub fn assert_sent(&self, pattern: &str) {
        assert!(
            self.sent(pattern),
            "expected a request matching {pattern:?}; saw {:?}",
            self.recorded().iter().map(|r| r.url.clone()).collect::<Vec<_>>()
        );
    }

    #[track_caller]
    pub fn assert_not_sent(&self, pattern: &str) {
        assert!(!self.sent(pattern), "did not expect a request matching {pattern:?}");
    }

    #[track_caller]
    pub fn assert_count(&self, expected: usize) {
        assert_eq!(self.count(), expected, "unexpected number of outbound requests");
    }
}

/// Match a URL against a pattern.
///
/// `*` stands for any run of characters, and the pattern is matched as a
/// substring rather than anchored — `"api.example.com/v1/*"` finds what a test
/// author means by it without their having to write the scheme. Deliberately
/// permissive: this decides which scripted answer a test gets, not who may
/// talk to whom.
fn matches(pattern: &str, url: &str) -> bool {
    let mut rest = url;

    for part in pattern.split('*') {
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Client;

    #[tokio::test]
    async fn answers_from_the_script_without_touching_the_network() {
        let client = Client::new().faking(
            Fake::new().on("api.example.com/things", FakeResponse::json(Json::object([("id", 7.into())]))),
        );

        let response = client.get("https://api.example.com/things").send().await.unwrap();

        assert!(response.is_success());
        assert_eq!(response.json().unwrap().get("id").unwrap().as_i64(), Some(7));
    }

    #[tokio::test]
    async fn records_what_was_sent() {
        let client = Client::new().faking(Fake::new().fallback(FakeResponse::text("ok")));

        client
            .post("https://api.example.com/v1/messages")
            .bearer("secret")
            .json(Json::object([("model", "claude-sonnet-5".into())]))
            .send()
            .await
            .unwrap();

        let fake = client.fake().unwrap();
        fake.assert_sent("api.example.com/v1/*");
        fake.assert_not_sent("openai.com");
        fake.assert_count(1);

        let sent = &fake.recorded()[0];
        assert_eq!(sent.method, Method::Post);
        assert_eq!(sent.headers.get("authorization"), Some("Bearer secret"));
        assert_eq!(sent.json().unwrap().get("model").unwrap().as_str(), Some("claude-sonnet-5"));
    }

    #[tokio::test]
    async fn an_unscripted_request_fails_loudly() {
        let client = Client::new().faking(Fake::new().on("expected.com", FakeResponse::text("ok")));

        let error = client.get("https://surprise.com").send().await.unwrap_err().to_string();

        assert!(error.contains("no fake response is scripted"));
        assert!(error.contains("surprise.com"));
    }

    #[test]
    fn patterns_match_in_order_with_wildcards() {
        assert!(matches("example.com", "https://api.example.com/x"));
        assert!(matches("api.example.com/v1/*", "https://api.example.com/v1/messages"));
        assert!(matches("https://api.*/v1/*", "https://api.example.com/v1/messages"));
        assert!(!matches("https://api.*/v2/*", "https://api.example.com/v1/messages"));
        assert!(!matches("https://other.*", "https://api.example.com/v1"));
        // The parts must appear in the order the pattern gives them.
        assert!(!matches("messages*v1", "https://api.example.com/v1/messages"));
    }

    #[tokio::test]
    async fn failure_statuses_can_be_scripted() {
        let client = Client::new()
            .faking(Fake::new().fallback(FakeResponse::text("slow down").status(429)));

        let response = client.get("https://api.example.com").send().await.unwrap();

        assert_eq!(response.status.code(), 429);
        assert!(response.error_for_status().is_err());
    }
}
