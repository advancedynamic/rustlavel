//! One endpoint that says whether the services behind the gateway are up.
//!
//! A load balancer in front of the gateway asks the gateway. Nobody asks four
//! services and takes the worst answer — so the gateway does it, once, and says
//! which one is unwell rather than only that something is.
//!
//! **Each upstream is asked with a short timeout of its own.** A health check
//! that hangs is worse than one that fails: the load balancer times out, marks
//! the gateway down, and takes away the only thing that could have told it
//! which service was the problem.

use std::time::Duration;

use rustlavel_client::Client;
use rustlavel_core::Json;
use rustlavel_http::{Response, Status};

use crate::route::Routes;

/// How long an upstream has to answer before it is called unwell.
///
/// Short on purpose. This is not "is it fast enough to serve", it is "is it
/// there at all", and a service that cannot answer two words in two seconds is
/// not there in any sense a load balancer cares about.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Ask every upstream, and report each by name.
///
/// `503` when any is unwell, so a load balancer reading only the status still
/// learns the truth — and the body still names which, so a person reading it
/// learns more.
pub async fn check(client: &Client, routes: &Routes, path: &str) -> Response {
    let mut services = Vec::new();
    let mut all_well = true;

    for (pattern, upstream) in routes.upstreams() {
        // A service that resolves to nothing is down, and is not probed: there
        // is no address to probe, and reporting it as up because no request
        // failed would be the most misleading answer available.
        let well = match upstream.resolved().await {
            Some(resolved) => {
                let url = format!("{}{path}", resolved.base());
                matches!(
                    tokio::time::timeout(PROBE_TIMEOUT, client.get(&url).send()).await,
                    Ok(Ok(response)) if response.is_success()
                )
            }
            None => false,
        };
        all_well &= well;

        services.push(Json::object([
            ("route", Json::from(pattern)),
            // The route's own description — `service://ORDERS` for a resolved
            // one. Printing one request's address here would describe that
            // request rather than the route.
            ("upstream", Json::from(upstream.base())),
            ("status", Json::from(if well { "up" } else { "down" })),
        ]));
    }

    let status = if all_well { Status::OK } else { Status(503) };
    Response::new(status).with_json(Json::object([
        ("status", Json::from(if all_well { "up" } else { "degraded" })),
        ("services", Json::Array(services)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::Upstream;

    /// A gateway with nothing behind it is up: there is nothing to be wrong.
    /// Reporting `degraded` for an empty table would make a fresh deploy look
    /// broken before a single service was added.
    #[tokio::test]
    async fn an_empty_table_is_healthy() {
        let response = check(&Client::new(), &Routes::new(), "/health").await;
        assert_eq!(response.status.0, 200);
    }

    /// Reporting a service with no instances as up, on the grounds that no
    /// request failed, would be the most misleading answer available.
    #[tokio::test]
    async fn a_service_that_resolves_to_nothing_is_down() {
        struct Nothing;
        impl crate::route::Locate for Nothing {
            fn base_for<'a>(
                &'a self,
                _service: &'a str,
            ) -> std::pin::Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
                Box::pin(async { None })
            }
        }

        let mut routes = Routes::new();
        routes.add("/api/*", Upstream::service("orders", std::sync::Arc::new(Nothing)));

        let response = check(&Client::new(), &routes, "/health").await;
        assert_eq!(response.status.0, 503);

        let body = String::from_utf8_lossy(&response.body).to_string();
        assert!(body.contains("service://ORDERS"), "the body does not name the route: {body}");
        assert!(body.contains("down"), "{body}");
    }

    /// And an upstream that is not there is reported by name rather than as a
    /// bare failure — the point of asking on everybody's behalf.
    #[tokio::test]
    async fn an_unreachable_upstream_makes_the_gateway_degraded_and_says_which() {
        let mut routes = Routes::new();
        // Port 1 answers nothing, on any machine.
        routes.add("/api/*", Upstream::at("http://127.0.0.1:1"));

        let response = check(&Client::new().timeout(PROBE_TIMEOUT), &routes, "/health").await;
        assert_eq!(response.status.0, 503);

        let body = String::from_utf8_lossy(&response.body).to_string();
        assert!(body.contains("degraded"), "{body}");
        assert!(body.contains("127.0.0.1:1"), "the body does not say which upstream: {body}");
        assert!(body.contains("down"), "{body}");
    }
}
