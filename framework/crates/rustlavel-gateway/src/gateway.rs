//! The plugin: one door in front of several services.

use std::sync::Arc;
use std::time::Duration;

use rustlavel_client::Client;
use rustlavel_http::{BoxFuture, Plugin, Request, Response, Setup, Status};

use crate::forward;
use crate::health;
use crate::route::{Routes, Upstream};

/// An API gateway.
///
/// Registers one catch-all route, so anything the application's own routes did
/// not claim falls through to the table. That order matters: a gateway that
/// took every path would make the health endpoint and the metrics scrape
/// unreachable, and those are exactly what somebody needs when the services
/// behind it are the problem.
pub struct Gateway {
    routes: Routes,
    client: Client,
    /// Where the aggregate health check answers, and the path asked of each
    /// upstream. `None` means do not mount one.
    health: Option<(String, String)>,
    /// Refuse a request that carries no credential, before it costs a hop.
    ///
    /// Off by default, because a gateway in front of a public API is a
    /// perfectly ordinary thing and refusing anonymous requests would break it.
    require_token: bool,
}

impl Default for Gateway {
    fn default() -> Self {
        Gateway::new()
    }
}

impl Gateway {
    pub fn new() -> Gateway {
        Gateway {
            routes: Routes::new(),
            // Timeouts rather than none: a gateway whose upstream never answers
            // holds the caller's connection open too, and one slow service
            // becomes every service being slow.
            client: Client::new().timeout(Duration::from_secs(30)),
            health: None,
            require_token: false,
        }
    }

    /// Route a pattern to an upstream. `"/api/*"` or an exact `"/health"`.
    pub fn route(mut self, pattern: impl Into<String>, upstream: Upstream) -> Gateway {
        self.routes.add(pattern, upstream);
        self
    }

    /// Use a client of your own — with a circuit breaker, a different timeout,
    /// or a fake, which is how the tests reach this.
    pub fn client(mut self, client: Client) -> Gateway {
        self.client = client;
        self
    }

    /// Refuse a request with no `Authorization` header before forwarding it.
    ///
    /// Routes an authorization server must be excepted with
    /// [`Upstream::open`], or nobody can obtain a token in the first place.
    ///
    /// **This does not replace the check inside each service.** A gateway is
    /// not the only door: anything on the network can reach a service directly,
    /// and a check that happens only at the edge is missing the moment somebody
    /// adds a second way in. What this saves is load — a request with no
    /// credential at all is refused here instead of after a hop.
    pub fn require_token(mut self) -> Gateway {
        self.require_token = true;
        self
    }

    /// Mount an aggregate health check: `at` on this gateway, asking each
    /// upstream `probe`.
    ///
    /// `gateway.health("/health", "/health")` is the usual pair. They are
    /// separate because the gateway's vocabulary is not always the upstream's —
    /// the same reason [`Upstream::strip`] exists.
    pub fn health(mut self, at: impl Into<String>, probe: impl Into<String>) -> Gateway {
        self.health = Some((at.into(), probe.into()));
        self
    }

    pub fn routes(&self) -> &Routes {
        &self.routes
    }
}

impl Plugin for Gateway {
    fn name(&self) -> &'static str {
        "gateway"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        let gateway = Arc::new(*self);

        // A real route, not the fallback: it must answer even when every
        // upstream is down, which is exactly when somebody is asking.
        if let Some((at, probe)) = gateway.health.clone() {
            let owner = Arc::clone(&gateway);
            setup.router.get(&at, move |_req: Request| {
                let owner = Arc::clone(&owner);
                let probe = probe.clone();
                Box::pin(async move { health::check(&owner.client, &owner.routes, &probe).await })
                    as BoxFuture<Response>
            });
        }

        // The fallback, not a wildcard route: the application's own routes —
        // health, metrics, anything it serves itself — are matched first, and
        // only what nothing claimed reaches the table.
        setup.router.fallback(move |request: Request| {
            let gateway = Arc::clone(&gateway);
            Box::pin(async move {
                let Some(upstream) = gateway.routes.find(request.path()) else {
                    return Response::not_found();
                };

                // `open` wins. Without it the authorization server would sit
                // behind a door that only opens from the inside.
                if gateway.require_token
                    && !upstream.is_open()
                    && request.header("authorization").is_none()
                {
                    return Response::new(Status::UNAUTHORIZED).with_json(
                        rustlavel_core::Json::object([(
                            "message",
                            rustlavel_core::Json::from("Unauthenticated."),
                        )]),
                    );
                }

                // Looked up here, per request, for a route that names a
                // service rather than an address. A service with nothing
                // running is 503 — the gateway is fine and there is nothing
                // behind it, which is not the 502 that means an instance
                // refused us.
                let Some(upstream) = upstream.resolved().await else {
                    rustlavel_core::warn!(
                        "gateway: {} has no instances",
                        upstream.service_name().unwrap_or("an upstream")
                    );
                    return Response::new(Status(503)).with_json(rustlavel_core::Json::object([(
                        "message",
                        rustlavel_core::Json::from("No instances are available."),
                    )]));
                };

                match forward::forward(&gateway.client, &upstream, request).await {
                    Ok(response) => response,
                    // `forward` turns an unreachable upstream into 502 itself,
                    // so reaching here means the gateway broke rather than the
                    // service behind it — and saying so is the difference
                    // between paging one team and the other.
                    Err(error) => {
                        rustlavel_core::error!("gateway failed to forward: {error}");
                        Response::new(Status::INTERNAL_ERROR)
                    }
                }
            }) as BoxFuture<Response>
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_are_matched_by_specificity_through_the_builder() {
        let gateway = Gateway::new()
            .route("/api/*", Upstream::at("http://api"))
            .route("/api/billing/*", Upstream::at("http://billing"));

        assert_eq!(
            gateway.routes().find("/api/billing/x").map(Upstream::base),
            Some("http://billing")
        );
        assert_eq!(gateway.routes().find("/api/x").map(Upstream::base), Some("http://api"));
    }

    /// The health endpoint has to be a route rather than the fallback: it must
    /// answer when every upstream is down, which is exactly when it is asked.
    #[test]
    fn a_health_endpoint_is_mounted_only_when_asked_for() {
        assert!(Gateway::new().health.is_none());
        let gateway = Gateway::new().health("/health", "/up");
        assert_eq!(gateway.health.as_ref().map(|(at, probe)| (at.as_str(), probe.as_str())),
                   Some(("/health", "/up")));
    }

    /// A gateway in front of a public API is ordinary, so refusing anonymous
    /// requests must be something you ask for rather than something you get.
    #[test]
    fn a_token_is_not_required_unless_it_is_asked_for() {
        assert!(!Gateway::new().require_token);
        assert!(Gateway::new().require_token().require_token);
    }

    /// The microservices kit shipped with `require_token()` covering every
    /// route, including `/oauth/*`. Asking for a token returned `401`, and
    /// there was no way to obtain the thing that would have made it succeed —
    /// a door that only opens from the inside. Found by running it.
    #[test]
    fn an_authorization_server_can_be_excepted_from_the_token_requirement() {
        let gateway = Gateway::new()
            .require_token()
            .route("/oauth/*", Upstream::at("http://auth").open())
            .route("/api/*", Upstream::at("http://api"));

        assert!(
            gateway.routes().find("/oauth/token").expect("a route").is_open(),
            "the token endpoint is behind the credential it issues"
        );
        assert!(
            !gateway.routes().find("/api/orders").expect("a route").is_open(),
            "opening one route opened another"
        );
    }

    /// Opening a route must be a line somebody wrote, never a default.
    #[test]
    fn an_upstream_is_closed_until_it_is_opened() {
        assert!(!Upstream::at("http://api").is_open());
        assert!(Upstream::at("http://api").open().is_open());
        // And it survives the other builders, whatever order they come in.
        assert!(Upstream::at("http://api").open().strip("/x").host("h").is_open());
    }
}
