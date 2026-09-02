//! A ceiling on how long a handler may run.
//!
//! Without one, a handler waiting on a database that has stopped answering
//! holds its connection, its task and its client for as long as the client is
//! willing to wait — which for a browser is minutes, and for a retrying
//! service is forever. With one, the wait ends at a known point and the
//! client gets an answer it can act on.
//!
//! ```ignore
//! r.group("/api", |api| {
//!     api.middleware(Timeout::after(Duration::from_secs(10)));
//!     …
//! });
//! ```
//!
//! The response is a 503. Not 504, which is for a gateway whose *upstream*
//! was slow, and not 408, which tells the client *it* was slow to send. A 503
//! says the service could not answer in time, which is the truth, and carries
//! no `Retry-After`, because nothing here knows when it would be safe to.
//!
//! What times out is dropped. A handler half-way through a database write is
//! abandoned at whatever `.await` it was parked on; the write either landed
//! or it did not, and the connection goes back to the pool in the state the
//! driver leaves it. Keep the limit generous enough that only a genuinely
//! stuck request hits it.

use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use crate::status::Status;
use rustlavel_core::Json;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct Timeout {
    limit: Duration,
}

impl Timeout {
    pub fn after(limit: Duration) -> Self {
        Timeout { limit }
    }

    pub fn seconds(seconds: u64) -> Self {
        Timeout::after(Duration::from_secs(seconds))
    }
}

impl Middleware for Timeout {
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response> {
        let limit = self.limit;
        let (method, path) = (request.method(), request.path().to_string());
        let wants_json = request.wants_json();

        Box::pin(async move {
            match tokio::time::timeout(limit, next.run(request)).await {
                Ok(response) => response,
                Err(_) => {
                    rustlavel_core::warn!(
                        "{method} {path} did not finish within {} ms and was abandoned",
                        limit.as_millis()
                    );
                    let response = Response::new(Status::SERVICE_UNAVAILABLE);
                    if wants_json {
                        response.with_json(Json::object([(
                            "message",
                            Json::from("The request took too long and was abandoned."),
                        )]))
                    } else {
                        response.with_text("The request took too long and was abandoned.")
                    }
                }
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

    fn client(limit: Duration) -> TestClient {
        let mut router = Router::new();
        router.middleware(Timeout::after(limit));
        router.get("/fast", |_req: Request| async { Response::text("done") });
        router.get("/slow", |_req: Request| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Response::text("finally")
        });
        TestClient::new(router)
    }

    #[tokio::test]
    async fn a_handler_within_the_limit_is_untouched() {
        let response = client(Duration::from_secs(1)).get("/fast").await;
        let response = response.assert_ok();
        assert_eq!(response.body(), "done");
    }

    #[tokio::test]
    async fn a_handler_over_the_limit_is_cut_off_with_a_503() {
        let started = std::time::Instant::now();
        let response = client(Duration::from_millis(50)).get("/slow").await;
        assert!(started.elapsed() < Duration::from_secs(5), "the wait ended at the limit");
        let response = response.assert_status(503);
        assert!(response.body().contains("too long"));
    }

    #[tokio::test]
    async fn an_api_client_gets_the_reason_as_json() {
        let request = Request::new(Method::Get, "/slow").with_header("accept", "application/json");
        let response = client(Duration::from_millis(50)).send(request).await;
        let response = response.assert_status(503);
        assert!(response.header("content-type").unwrap().starts_with("application/json"));
        assert!(response.json().get("message").is_some());
    }
}
