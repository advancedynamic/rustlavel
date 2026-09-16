//! Routing a gateway's traffic by service name.
//!
//! Behind the `gateway` feature, because a gateway with a table of fixed
//! addresses — which is most gateways — should not compile a registry client it
//! never calls.
//!
//! ```ignore
//! let balancer = Arc::new(Balancer::new(Discovery::new(registries)));
//!
//! Gateway::new()
//!     .route("/api/orders/*", Upstream::service("orders", balancer.clone()).strip("/api"))
//!     .route("/api/billing/*", Upstream::service("billing", balancer).strip("/api"))
//! ```

use std::sync::Arc;

use rustlavel_gateway::Locate;

use crate::client::Discovery;
use crate::resolve::{Balance, RoundRobin};

/// A [`Discovery`] and a way of choosing between what it finds.
pub struct Balancer {
    discovery: Discovery,
    balance: Arc<dyn Balance>,
}

impl Balancer {
    pub fn new(discovery: Discovery) -> Balancer {
        Balancer { discovery, balance: Arc::new(RoundRobin::new()) }
    }

    /// Choose differently — by zone, by weight, or by something the application
    /// knows that a registry cannot.
    pub fn balancing(mut self, balance: Arc<dyn Balance>) -> Balancer {
        self.balance = balance;
        self
    }
}

impl Locate for Balancer {
    fn base_for<'a>(
        &'a self,
        service: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
        Box::pin(async move {
            let instances = match self.discovery.lookup(service).await {
                Ok(instances) => instances,
                // Already logged, with the reason, by the lookup. Saying it
                // twice per request would bury the line that explains it.
                Err(_) => return None,
            };

            self.balance.pick(&instances).map(|instance| instance.url())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eureka;
    use crate::instance::Instance;
    use rustlavel_client::{Client as Http, Fake, FakeResponse};

    fn balancer_over(instances: Vec<Instance>) -> Balancer {
        let http = Http::new().faking(
            Fake::new()
                .on("/eureka/apps/ORDERS", FakeResponse::json(eureka::application_json("ORDERS", &instances))),
        );
        Balancer::new(Discovery::new(vec!["http://registry:8761".into()]).http(http))
    }

    #[tokio::test]
    async fn a_service_name_becomes_an_instance_address() {
        let balancer = balancer_over(vec![Instance::new("orders", "10.0.0.7", 8080)]);
        assert_eq!(balancer.base_for("ORDERS").await.as_deref(), Some("http://10.0.0.7:8080"));
    }

    /// Each instance gets a turn, so a second request does not land on the
    /// first instance again.
    #[tokio::test]
    async fn successive_requests_are_spread_across_the_instances() {
        let balancer = balancer_over(vec![
            Instance::new("orders", "10.0.0.1", 8080),
            Instance::new("orders", "10.0.0.2", 8080),
        ]);

        let first = balancer.base_for("ORDERS").await;
        let second = balancer.base_for("ORDERS").await;
        assert_ne!(first, second, "every request went to the same instance");
    }

    /// `None`, which the gateway turns into a 503 that says the service has no
    /// instances — rather than forwarding to a host it made up.
    #[tokio::test]
    async fn a_service_with_nothing_registered_locates_nothing() {
        assert!(balancer_over(vec![]).base_for("ORDERS").await.is_none());
    }

    /// An unreachable registry with nothing cached is not an address either.
    #[tokio::test]
    async fn an_unreachable_registry_locates_nothing() {
        let balancer = Balancer::new(
            Discovery::new(vec!["http://registry:8761".into()]).http(Http::new().faking(Fake::new())),
        );
        assert!(balancer.base_for("ORDERS").await.is_none());
    }
}
