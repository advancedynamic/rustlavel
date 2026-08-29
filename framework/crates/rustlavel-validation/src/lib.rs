//! rustlavel-validation: Laravel-style validation, written from scratch.
//!
//! Rules are declared the way they are in Laravel — as a string — or built with
//! methods when the compiler should check them:
//!
//! ```no_run
//! # use rustlavel_validation::{validate, Rule, Validator};
//! # use rustlavel_http::Request;
//! # async fn example(request: &mut Request) {
//! let data = validate(request, &[("email", "required|email"), ("age", "integer|min:18")])
//!     .await
//!     .unwrap();
//!
//! // the same rules, checked at compile time
//! let same = Validator::from_request(request)
//!     .rule("email", Rule::required().email())
//!     .rule("age", Rule::integer().min(18))
//!     .validate();
//! # let _ = (data, same);
//! # }
//! ```
//!
//! A failure is an [`Errors`] — field to messages — which turns into Laravel's
//! `422` body, `{"message": "...", "errors": {"email": ["..."]}}`, for a client
//! that wants JSON, and a plain `422` for a browser.
//!
//! ## Why the entry point is async
//!
//! Nothing here awaits yet. It is async because the rules that come next —
//! Laravel's `unique` and `exists` — must ask the database, and this project
//! treats a stable API as a feature. Better one `.await` today than a breaking
//! signature change the week `rustlavel-db` lands.
//!
//! ## Using `?` in a handler
//!
//! [`Errors`] implements [`IntoResponse`], so a handler can hand one straight
//! back. Rust's orphan rules stop this crate from also implementing that trait
//! for `Result<_, Errors>` — `Result` belongs to `core` and the trait belongs to
//! `rustlavel-http` — so [`attempt`] bridges the gap and lets the body of a
//! handler use `?` as normal:
//!
//! ```no_run
//! # use rustlavel_validation::{attempt, validate};
//! # use rustlavel_http::{IntoResponse, Request, Response};
//! async fn store(mut request: Request) -> impl IntoResponse {
//!     attempt(async move {
//!         let data = validate(&mut request, &[("email", "required|email")]).await?;
//!         Ok(Response::json(data.into_json()))
//!     })
//!     .await
//! }
//! ```

pub mod check;
pub mod errors;
pub mod input;
pub mod messages;
pub mod rule;
pub mod validator;

pub use errors::Errors;
pub use input::Input;
pub use messages::{Messages, SizeKind};
pub use rule::{IntoRules, Rule, Rules};
pub use validator::{Validated, Validator};

use rustlavel_http::{IntoResponse, Request, Response};
use std::future::Future;

/// Validate a request against Laravel-style rule strings.
///
/// The shortest path from a request to trusted data: on success the validated
/// subset comes back, on failure an [`Errors`] that already knows whether the
/// client wanted JSON.
///
/// # Panics
///
/// If a rule spec is malformed. A bad spec is a bug in the source rather than
/// bad input from a user, and the panic carries the parser's message — which
/// names the rule and suggests the one that was meant. Use [`Rules::parse`]
/// when the spec comes from data instead of from code.
pub async fn validate(
    request: &mut Request,
    rules: &[(&str, &str)],
) -> Result<Validated, Errors> {
    Validator::from_request(request).rules(rules).validate()
}

/// Run a handler body that uses `?`, turning a validation failure into its
/// `422` response instead of letting it escape as a server error.
///
/// See the crate documentation for why this exists rather than an
/// `IntoResponse` implementation on `Result<_, Errors>`.
pub async fn attempt<T: IntoResponse>(
    body: impl Future<Output = Result<T, Errors>>,
) -> Response {
    match body.await {
        Ok(value) => value.into_response(),
        Err(errors) => errors.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_core::Json;
    use rustlavel_http::{Method, Status};
    use std::task::{Context, Poll};

    /// Drive a future to completion without a runtime.
    ///
    /// Validation never awaits I/O, so one poll always finishes it; pulling in
    /// an executor to prove that would only slow the test suite down.
    fn block_on<F: Future>(future: F) -> F::Output {
        let mut context = Context::from_waker(std::task::Waker::noop());
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("a validation future must never need a runtime"),
        }
    }

    #[test]
    fn the_entry_point_returns_the_validated_subset_of_a_request() {
        let mut request = Request::new(Method::Post, "/users")
            .with_json(Json::object([("email", "ada@example.com".into()), ("age", 36.into())]));

        let data = block_on(validate(
            &mut request,
            &[("email", "required|email"), ("age", "integer|min:18")],
        ))
        .unwrap();

        assert_eq!(data.string("email").as_deref(), Some("ada@example.com"));
        assert_eq!(data.integer("age"), Some(36));
    }

    #[test]
    fn the_entry_point_returns_errors_shaped_like_laravels_422() {
        let mut request = Request::new(Method::Post, "/users")
            .with_json(Json::object([("email", "nope".into()), ("age", 12.into())]));

        let errors = block_on(validate(
            &mut request,
            &[("email", "required|email"), ("age", "integer|min:18")],
        ))
        .unwrap_err();

        assert_eq!(
            errors.to_json().to_string(),
            r#"{"errors":{"age":["The age field must be at least 18."],"email":["The email field must be a valid email address."]},"message":"The age field must be at least 18. (and 1 more error)"}"#
        );
    }

    #[test]
    fn a_handler_body_using_the_question_mark_answers_422() {
        let mut request = Request::new(Method::Post, "/api/users")
            .with_json(Json::object([("email", "nope".into())]));

        let response = block_on(attempt(async move {
            let data = validate(&mut request, &[("email", "required|email")]).await?;
            Ok(Response::json(data.into_json()))
        }));

        assert_eq!(response.status, Status::UNPROCESSABLE);
        assert_eq!(response.headers.content_type(), Some("application/json"));
        assert!(response.body_string().contains(r#""errors":{"email":["#));
    }

    #[test]
    fn a_handler_body_that_validates_answers_with_its_own_response() {
        let mut request = Request::new(Method::Post, "/api/users")
            .with_json(Json::object([("email", "ada@example.com".into())]));

        let response = block_on(attempt(async move {
            let data = validate(&mut request, &[("email", "required|email")]).await?;
            Ok(Response::json(data.into_json()))
        }));

        assert_eq!(response.status, Status::OK);
        assert_eq!(response.body_string(), r#"{"email":"ada@example.com"}"#);
    }

    #[test]
    fn a_browser_submission_fails_with_a_plain_422() {
        let mut request = Request::new(Method::Post, "/register").with_form(&[("email", "nope")]);

        let response = block_on(attempt(async move {
            let data = validate(&mut request, &[("email", "required|email")]).await?;
            Ok(Response::json(data.into_json()))
        }));

        assert_eq!(response.status, Status::UNPROCESSABLE);
        assert_eq!(response.headers.content_type(), Some("text/plain"));
        assert_eq!(response.body_string(), "The email field must be a valid email address.");
    }
}
