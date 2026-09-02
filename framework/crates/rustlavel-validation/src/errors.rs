//! What comes back when validation fails.
//!
//! Laravel answers a failed validation with `422` and a body of
//! `{"message": "...", "errors": {"email": ["..."]}}`. Every JavaScript client
//! written against a Laravel API already knows that shape, so Rustlavel emits
//! it verbatim rather than inventing a fourth error envelope.

use crate::messages::Messages;
use rustlavel_core::Json;
use rustlavel_http::{IntoResponse, Response, Status};
use std::collections::BTreeMap;

/// The messages a failed validation produced, grouped by field.
///
/// `wants_json` and `back` are captured when the errors are built from a
/// request, because [`IntoResponse::into_response`] no longer has the request
/// to negotiate with or to read a `Referer` from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Errors {
    fields: BTreeMap<String, Vec<String>>,
    wants_json: bool,
    /// Where a browser is sent when validation fails. `None` when there is no
    /// session to leave the messages in, which is when this falls back to
    /// rendering them as text.
    back: Option<String>,
}

impl Errors {
    pub fn new() -> Self {
        Errors::default()
    }

    /// The status a failed validation answers with.
    pub const STATUS: Status = Status::UNPROCESSABLE;

    pub fn add(&mut self, field: impl Into<String>, message: impl Into<String>) {
        self.fields.entry(field.into()).or_default().push(message.into());
    }

    pub fn has(&self, field: &str) -> bool {
        self.fields.contains_key(field)
    }

    /// The first message for a field — what a form renders next to the input.
    pub fn first(&self, field: &str) -> Option<&str> {
        self.fields.get(field)?.first().map(String::as_str)
    }

    /// Every message for one field.
    pub fn get(&self, field: &str) -> &[String] {
        self.fields.get(field).map_or(&[], Vec::as_slice)
    }

    /// Every message for every field, keyed by field name.
    pub fn all(&self) -> &BTreeMap<String, Vec<String>> {
        &self.fields
    }

    /// Every message, flattened, in field order.
    pub fn messages(&self) -> impl Iterator<Item = &str> {
        self.fields.values().flatten().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The number of messages, not the number of fields — one field can fail
    /// several rules.
    pub fn len(&self) -> usize {
        self.fields.values().map(Vec::len).sum()
    }

    pub fn fields(&self) -> impl Iterator<Item = &str> {
        self.fields.keys().map(String::as_str)
    }

    /// Whether the response should be JSON. Set from `Request::wants_json`.
    pub fn wants_json(&self) -> bool {
        self.wants_json
    }

    /// Send a browser back here instead of rendering the messages as text.
    pub fn redirecting_to(mut self, back: Option<String>) -> Self {
        self.back = back;
        self
    }

    /// Where a browser will be sent, if anywhere.
    pub fn back(&self) -> Option<&str> {
        self.back.as_deref()
    }

    pub fn with_json(mut self, wants_json: bool) -> Self {
        self.wants_json = wants_json;
        self
    }

    /// Add a message by hand, chaining — useful for a rule an application
    /// enforces itself, so its failure comes back in the same envelope as the
    /// built-in ones.
    pub fn with(mut self, field: impl Into<String>, message: impl Into<String>) -> Self {
        self.add(field, message);
        self
    }

    /// Render a hand-written message through the message bag's label rules, so
    /// it interpolates `:attribute` the way a built-in one does.
    pub fn add_interpolated(&mut self, messages: &Messages, field: &str, template: &str) {
        let rendered =
            crate::messages::interpolate(template, &[("attribute", messages.label(field))]);
        self.add(field, rendered);
    }

    /// The single-line summary Laravel puts in the `message` key: the first
    /// failure, and a count of the rest so a client that only shows one line
    /// still tells the user there is more to fix.
    pub fn summary(&self) -> String {
        let Some(first) = self.messages().next() else {
            return "The given data was invalid.".to_string();
        };
        match self.len() - 1 {
            0 => first.to_string(),
            1 => format!("{first} (and 1 more error)"),
            more => format!("{first} (and {more} more errors)"),
        }
    }

    /// The Laravel-shaped 422 body.
    /// Just the field map, as a template reads it: `{"email": ["…"]}`.
    ///
    /// [`Errors::to_json`] wraps this in Laravel's `{"message", "errors"}`
    /// envelope, which is right for an API response and one level too deep for
    /// a view.
    pub fn to_field_json(&self) -> Json {
        Json::object(self.fields.iter().map(|(field, messages)| {
            (
                field.as_str(),
                Json::Array(messages.iter().map(|m| Json::from(m.as_str())).collect()),
            )
        }))
    }

    pub fn to_json(&self) -> Json {
        let errors = self.fields.iter().map(|(field, messages)| {
            let messages = messages.iter().map(|m| Json::from(m.as_str())).collect();
            (field.clone(), Json::Array(messages))
        });
        Json::object([
            ("message", Json::from(self.summary())),
            ("errors", Json::Object(errors.collect())),
        ])
    }
}

impl std::fmt::Display for Errors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary())
    }
}

impl std::error::Error for Errors {}

impl From<&Errors> for Json {
    fn from(errors: &Errors) -> Self {
        errors.to_json()
    }
}

impl From<Errors> for Json {
    fn from(errors: Errors) -> Self {
        errors.to_json()
    }
}

/// A `422` for an API client; for a browser, a redirect back to the form.
///
/// The two halves answer different questions. A JSON client asked for a result
/// and gets one, with the status that says why. A browser asked for a page, and
/// the useful answer is the form it just submitted, with the messages attached
/// and the boxes still filled in.
///
/// It is a redirect rather than the page itself because the answer to a failed
/// `POST` has to leave the browser somewhere reloadable. Rendering in place
/// leaves it on a URL that re-submits the form on refresh, which is the
/// double-submission problem in miniature — and it is why the messages travel
/// through the session rather than in this response.
///
/// With no session to leave them in, this falls back to plain text. That is a
/// degraded answer rather than a broken one, and it is what an application
/// with no session middleware gets.
impl IntoResponse for Errors {
    fn into_response(self) -> Response {
        if self.wants_json {
            return Response::new(Errors::STATUS).with_json(self.to_json());
        }
        if let Some(back) = &self.back {
            // 303, so the browser follows it with a GET. A 302 leaves the
            // method to the browser, and the older ones famously disagreed.
            return Response::see_other(back.clone());
        }
        let mut body = self.summary();
        for message in self.messages().skip(1) {
            body.push('\n');
            body.push_str(message);
        }
        Response::new(Errors::STATUS).with_text(body)
    }
}

impl From<Errors> for Response {
    fn from(errors: Errors) -> Self {
        errors.into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Errors {
        Errors::new()
            .with("email", "The email field is required.")
            .with("email", "The email field must be a valid email address.")
            .with("age", "The age field must be at least 18.")
    }

    #[test]
    fn an_empty_bag_reports_itself_as_empty() {
        let errors = Errors::new();
        assert!(errors.is_empty());
        assert_eq!(errors.len(), 0);
        assert!(!errors.has("email"));
        assert_eq!(errors.first("email"), None);
        assert!(errors.get("email").is_empty());
    }

    #[test]
    fn messages_are_grouped_by_field_and_kept_in_order() {
        let errors = sample();

        assert!(errors.has("email"));
        assert_eq!(errors.first("email"), Some("The email field is required."));
        assert_eq!(errors.get("email").len(), 2);
        assert_eq!(errors.all().len(), 2, "two fields failed");
        assert_eq!(errors.len(), 3, "three messages in total");
        assert_eq!(errors.fields().collect::<Vec<_>>(), ["age", "email"]);
    }

    #[test]
    fn the_summary_counts_the_failures_it_did_not_show() {
        assert_eq!(Errors::new().summary(), "The given data was invalid.");
        assert_eq!(Errors::new().with("a", "One.").summary(), "One.");
        assert_eq!(
            Errors::new().with("a", "One.").with("a", "Two.").summary(),
            "One. (and 1 more error)"
        );
        assert_eq!(sample().summary(), "The age field must be at least 18. (and 2 more errors)");
    }

    #[test]
    fn the_body_has_laravels_422_shape() {
        let body = Errors::new()
            .with("email", "The email field is required.")
            .to_json();

        assert_eq!(
            body.to_string(),
            r#"{"errors":{"email":["The email field is required."]},"message":"The email field is required."}"#
        );
        assert_eq!(body.get("errors.email.0").unwrap().as_str(), Some("The email field is required."));
    }

    #[test]
    fn a_json_client_gets_the_422_envelope() {
        let response = sample().with_json(true).into_response();

        assert_eq!(response.status, Status::UNPROCESSABLE);
        assert_eq!(response.headers.content_type(), Some("application/json"));
        assert!(response.body_string().contains(r#""errors":{"age":["#));
    }

    #[test]
    fn a_browser_gets_a_plain_body_it_can_read() {
        let response = sample().into_response();

        assert_eq!(response.status, Status::UNPROCESSABLE);
        assert_eq!(response.headers.content_type(), Some("text/plain"));
        assert!(response.body_string().contains("The email field is required."));
    }

    #[test]
    fn a_hand_written_message_interpolates_the_attribute() {
        let messages = Messages::new().attribute("dob", "date of birth");
        let mut errors = Errors::new();
        errors.add_interpolated(&messages, "dob", "The :attribute field is in the future.");

        assert_eq!(errors.first("dob"), Some("The date of birth field is in the future."));
    }

    #[test]
    fn errors_display_as_their_summary() {
        assert_eq!(sample().to_string(), sample().summary());
    }
}
