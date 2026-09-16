//! The registry, as something you can run.
//!
//! A plugin rather than a binary, so a registry is an ordinary Rustlavel
//! application with one line in it — and so the same process can serve the
//! registry, a health endpoint and whatever else an operator wants at the same
//! address, which is what they will want at three in the morning.
//!
//! The routes are Eureka's, exactly, so a Spring Cloud client registers here
//! with its stock configuration pointed at this host and nothing else changed.

use std::sync::Arc;

use rustlavel_core::Json;
use rustlavel_http::{BoxFuture, Plugin, Request, Response, Setup, Status};

use crate::eureka;
use crate::instance::Status as InstanceStatus;
use crate::registry::{Mode, Registry};
use crate::replicate::{Change, Peers, REPLICATION_HEADER, is_replication};

/// A service registry.
///
/// ```ignore
/// App::new()
///     .plugin(RegistryServer::new().peers(vec!["http://registry-b:8761".into()]))
///     .run()
///     .await
/// ```
pub struct RegistryServer {
    registry: Registry,
    peers: Peers,
    /// Where the dashboard is mounted, or `None` for no dashboard.
    dashboard: Option<String>,
}

impl Default for RegistryServer {
    fn default() -> Self {
        RegistryServer::new()
    }
}

impl RegistryServer {
    pub fn new() -> RegistryServer {
        RegistryServer {
            registry: Registry::new(),
            peers: Peers::new(Vec::new()),
            dashboard: Some("/discovery".to_string()),
        }
    }

    /// Share the registry with the rest of the application — a dashboard, or a
    /// gateway running in the same process.
    pub fn using(mut self, registry: Registry) -> RegistryServer {
        self.registry = registry;
        self
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The other registry nodes. Pass every node including this one and use
    /// [`Peers::excluding`] — or pass only the others; both work.
    pub fn peers(mut self, peers: Peers) -> RegistryServer {
        self.peers = peers;
        self
    }

    /// Move the dashboard, or take it away.
    ///
    /// **A registry's dashboard is an internal tool and this plugin puts no
    /// authentication in front of it.** It lists every host and port you run,
    /// which is a map of the estate for anybody who reaches it. Mount it behind
    /// whatever the application already uses, or pass `None` and read the
    /// registry through [`Registry`] from a page of your own — which is what
    /// the auth-kit dashboard does.
    pub fn dashboard(mut self, at: Option<&str>) -> RegistryServer {
        self.dashboard = at.map(str::to_string);
        self
    }
}

/// Whether this write should be forwarded to the peers.
fn arrived_from_a_client(request: &Request) -> bool {
    !is_replication(request.query("isReplication"), request.header(REPLICATION_HEADER))
}

impl Plugin for RegistryServer {
    fn name(&self) -> &'static str {
        "discovery"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        let this = Arc::new(*self);

        // The registry is application state as well, so a handler of the
        // application's own — a dashboard, a gateway — reads the same instances
        // rather than asking this server over HTTP to tell it about itself.
        setup.state(this.registry.clone());

        let owner = Arc::clone(&this);
        setup.router.post("/eureka/apps/{app}", move |mut request: Request| {
            let owner = Arc::clone(&owner);
            Box::pin(async move {
                let Some(body) = request.json().cloned() else {
                    return Response::new(Status::BAD_REQUEST)
                        .with_json(problem("a registration must be a JSON body"));
                };
                let Some(instance) = eureka::read_instance(&body) else {
                    return Response::new(Status::BAD_REQUEST)
                        .with_json(problem("a registration needs an app and a host"));
                };

                owner.registry.register(instance.clone());
                if arrived_from_a_client(&request) {
                    owner.peers.spread(Change::Registered(instance));
                }

                // 204, which is what Eureka answers and what its clients check
                // for. A 200 with a body is read as a failure by some of them.
                Response::no_content()
            }) as BoxFuture<Response>
        });

        let owner = Arc::clone(&this);
        setup.router.put("/eureka/apps/{app}/{id}", move |request: Request| {
            let owner = Arc::clone(&owner);
            Box::pin(async move {
                let (app, id) = named(&request);

                if !owner.registry.renew(&app, &id) {
                    // 404 is the contract: it is how a client learns it must
                    // register again, and it is what every client gets after
                    // this process restarts.
                    return Response::not_found();
                }

                if arrived_from_a_client(&request) {
                    owner.peers.spread(Change::Renewed { service: app, id });
                }
                Response::ok()
            }) as BoxFuture<Response>
        });

        let owner = Arc::clone(&this);
        setup.router.put("/eureka/apps/{app}/{id}/status", move |request: Request| {
            let owner = Arc::clone(&owner);
            Box::pin(async move {
                let (app, id) = named(&request);
                // No `value` at all is `Down`, through `Status::parse`. A status
                // this server cannot read must not be treated as healthy.
                let status = InstanceStatus::parse(request.query("value").unwrap_or(""));

                if !owner.registry.set_status(&app, &id, status) {
                    return Response::not_found();
                }

                if arrived_from_a_client(&request) {
                    owner.peers.spread(Change::StatusChanged { service: app, id, status });
                }
                Response::ok()
            }) as BoxFuture<Response>
        });

        let owner = Arc::clone(&this);
        setup.router.delete("/eureka/apps/{app}/{id}", move |request: Request| {
            let owner = Arc::clone(&owner);
            Box::pin(async move {
                let (app, id) = named(&request);
                let removed = owner.registry.cancel(&app, &id);

                // Forwarded even when this node had nothing to remove. The
                // instance may have registered with a peer and never with this
                // one, and a cancellation that stopped here would leave it
                // being handed out by every other node.
                if arrived_from_a_client(&request) {
                    owner.peers.spread(Change::Cancelled { service: app, id });
                }

                match removed {
                    true => Response::ok(),
                    false => Response::not_found(),
                }
            }) as BoxFuture<Response>
        });

        let owner = Arc::clone(&this);
        setup.router.get("/eureka/apps", move |_request: Request| {
            let owner = Arc::clone(&owner);
            Box::pin(async move {
                // Everything, stale included. This is the document a peer reads
                // to catch up, and filtering it would mean a node that restarts
                // never learns about an instance its peers are unsure of.
                Response::json(eureka::applications_json(&owner.registry.all()))
            }) as BoxFuture<Response>
        });

        let owner = Arc::clone(&this);
        setup.router.get("/eureka/apps/{app}", move |request: Request| {
            let owner = Arc::clone(&owner);
            Box::pin(async move {
                let app = request.param("app").unwrap_or("").to_ascii_uppercase();
                let (instances, mode) = owner.registry.alive(&app);

                if instances.is_empty() {
                    return Response::not_found();
                }

                let mut response = Response::json(eureka::application_json(&app, &instances));
                if mode == Mode::SelfPreservation {
                    // Said out loud, on every answer, because a caller
                    // wondering why it reached a dead instance deserves to find
                    // the reason without reading the registry's logs.
                    response = response.with_header("x-discovery-self-preservation", "true");
                }
                response
            }) as BoxFuture<Response>
        });

        // The whole registry as plain JSON, for a dashboard. Eureka's own
        // document is shaped for Eureka clients; this one is shaped for a page.
        let owner = Arc::clone(&this);
        setup.router.get("/discovery/registry.json", move |_request: Request| {
            let owner = Arc::clone(&owner);
            Box::pin(async move { Response::json(crate::ui::registry_json(&owner.registry)) })
                as BoxFuture<Response>
        });

        if let Some(at) = this.dashboard.clone() {
            let owner = Arc::clone(&this);
            setup.router.get(&at, move |_request: Request| {
                let owner = Arc::clone(&owner);
                Box::pin(async move { Response::html(crate::ui::page(&owner.registry)) })
                    as BoxFuture<Response>
            });
        }
    }
}

fn named(request: &Request) -> (String, String) {
    (
        request.param("app").unwrap_or("").to_ascii_uppercase(),
        request.param("id").unwrap_or("").to_string(),
    )
}

fn problem(message: &str) -> Json {
    Json::object([("message", Json::from(message))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::Instance;

    #[test]
    fn the_dashboard_can_be_taken_away() {
        assert_eq!(RegistryServer::new().dashboard.as_deref(), Some("/discovery"));
        assert!(RegistryServer::new().dashboard(None).dashboard.is_none());
        assert_eq!(
            RegistryServer::new().dashboard(Some("/ops/discovery")).dashboard.as_deref(),
            Some("/ops/discovery")
        );
    }

    /// A shared registry is how a dashboard in the same process reads the
    /// instances without asking this server over HTTP about itself.
    #[test]
    fn a_registry_can_be_shared_with_the_rest_of_the_application() {
        let registry = Registry::new();
        let server = RegistryServer::new().using(registry.clone());

        registry.register(Instance::new("orders", "10.0.0.1", 8080));
        assert_eq!(server.registry().all().len(), 1);
    }
}
