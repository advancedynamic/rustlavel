//! A per-route ceiling on the request body.
//!
//! The server has a limit of its own ([`crate::Limits`]) that protects the
//! process: a body over it is refused before it is buffered. That limit has to
//! be as large as the biggest upload the application accepts anywhere, which
//! makes it useless as policy — a JSON endpoint that expects a kilobyte should
//! not accept the fifty megabytes the avatar upload needs. This middleware is
//! the policy: tighter, and per group.
//!
//! ```ignore
//! r.group("/api", |api| {
//!     api.middleware(BodyLimit::kilobytes(64));
//!     …
//! });
//! ```
//!
//! By the time middleware runs the body has been read, so this does not save
//! memory — the server's limit does that. It saves the handler from parsing
//! something it was never meant to receive, and tells the client why.

use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use crate::status::Status;
use rustlavel_core::Json;

#[derive(Debug, Clone, Copy)]
pub struct BodyLimit {
    max: usize,
}

impl BodyLimit {
    pub fn bytes(max: usize) -> Self {
        BodyLimit { max }
    }

    pub fn kilobytes(max: usize) -> Self {
        BodyLimit::bytes(max * 1024)
    }

    pub fn megabytes(max: usize) -> Self {
        BodyLimit::bytes(max * 1024 * 1024)
    }
}

impl Middleware for BodyLimit {
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response> {
        // The declared length is checked as well as the actual one, so a
        // request that lies in either direction is still caught.
        let declared = request.headers().content_length().unwrap_or(0);
        let actual = request.body().len();
        let size = declared.max(actual);

        if size <= self.max {
            return next.run(request);
        }

        let max = self.max;
        let wants_json = request.wants_json();
        Box::pin(async move {
            let message = format!("The request body is {size} bytes; this endpoint accepts at most {max}.");
            let response = Response::new(Status::PAYLOAD_TOO_LARGE);
            if wants_json {
                response.with_json(Json::object([("message", Json::from(message))]))
            } else {
                response.with_text(message)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;
    use crate::router::Router;
    use crate::testing::TestClient;

    fn client(limit: BodyLimit) -> TestClient {
        let mut router = Router::new();
        router.middleware(limit);
        router.post("/notes", |req: Request| async move {
            Response::text(format!("{} bytes", req.body().len()))
        });
        TestClient::new(router)
    }

    #[tokio::test]
    async fn a_body_under_the_limit_reaches_the_handler() {
        let request = Request::new(Method::Post, "/notes").with_body(vec![b'x'; 100]);
        let response = client(BodyLimit::bytes(100)).send(request).await;
        let response = response.assert_ok();
        assert_eq!(response.body(), "100 bytes");
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_a_413_with_the_numbers() {
        let request = Request::new(Method::Post, "/notes")
            .with_body(vec![b'x'; 101])
            .with_header("accept", "application/json");
        let response = client(BodyLimit::bytes(100)).send(request).await;
        let response = response.assert_status(413);
        let message = response.json().get("message").and_then(Json::as_str).unwrap().to_string();
        assert!(message.contains("101 bytes") && message.contains("at most 100"), "{message}");
    }

    #[tokio::test]
    async fn a_declared_length_over_the_limit_is_refused_too() {
        let request = Request::new(Method::Post, "/notes").with_header("content-length", "5000000");
        client(BodyLimit::kilobytes(64)).send(request).await.assert_status(413);
    }

    #[test]
    fn the_unit_helpers_multiply_correctly() {
        assert_eq!(BodyLimit::kilobytes(2).max, 2048);
        assert_eq!(BodyLimit::megabytes(1).max, 1_048_576);
    }
}
