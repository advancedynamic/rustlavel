//! Turning a service name into an address.
//!
//! Three answers behind one trait, because the right one depends on where the
//! application runs and that should not be a rewrite.

use std::sync::atomic::{AtomicUsize, Ordering};

use rustlavel_core::Result;

use crate::instance::Instance;

/// Somewhere service names come from.
pub trait Resolve: Send + Sync {
    /// The instances currently serving `service`.
    ///
    /// An empty list is not an error: a service may legitimately have nothing
    /// running, and the caller wants `503` rather than a message about a
    /// registry it did not know existed.
    fn instances<'a>(
        &'a self,
        service: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<Instance>>> + Send + 'a>>;
}

/// The list you wrote.
///
/// Where everybody starts, and where a great many applications correctly stay.
/// Two services and a load balancer do not need a registry; they need two lines
/// of configuration.
pub struct Static {
    instances: Vec<Instance>,
}

impl Static {
    pub fn new(instances: Vec<Instance>) -> Static {
        Static { instances }
    }
}

impl Resolve for Static {
    fn instances<'a>(
        &'a self,
        service: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<Instance>>> + Send + 'a>> {
        let service = service.to_ascii_uppercase();
        Box::pin(async move {
            Ok(self.instances.iter().filter(|held| held.service == service).cloned().collect())
        })
    }
}

/// A name the operating system already resolves.
///
/// **This is the Kubernetes answer, and the Consul answer.** A `Service` there
/// already resolves to live pods and something already balances across them, so
/// a registry would be a second copy of a fact the platform is keeping anyway —
/// and two sources for one fact disagree eventually.
///
/// The suffix is how a bare name becomes a resolvable one:
/// `Dns::new(".default.svc.cluster.local", 8080)` turns `orders` into
/// `orders.default.svc.cluster.local:8080`.
pub struct Dns {
    suffix: String,
    port: u16,
}

impl Dns {
    pub fn new(suffix: impl Into<String>, port: u16) -> Dns {
        Dns { suffix: suffix.into(), port }
    }

    fn host_for(&self, service: &str) -> String {
        format!("{}{}", service.to_ascii_lowercase(), self.suffix)
    }
}

impl Resolve for Dns {
    fn instances<'a>(
        &'a self,
        service: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<Instance>>> + Send + 'a>> {
        let host = self.host_for(service);
        let port = self.port;
        let service = service.to_string();

        Box::pin(async move {
            // One name, not a list of addresses. The platform is balancing
            // already, and resolving to individual pods here would mean load
            // balancing twice — badly the second time, because this side cannot
            // see which of them are ready.
            Ok(vec![Instance::new(service, host, port)])
        })
    }
}

/// Which instance gets the next request.
pub trait Balance: Send + Sync {
    /// Pick one, or `None` when there are none to pick from.
    fn pick<'a>(&self, instances: &'a [Instance]) -> Option<&'a Instance>;
}

/// Each in turn.
///
/// Predictable, which is worth more here than clever: with least-connections a
/// slow instance quietly attracts less traffic and nobody notices it is slow,
/// and with random the distribution is only even in the long run. Round-robin
/// is wrong in exactly one way — it sends the same share to an instance that
/// cannot cope — and the circuit breaker is what answers that.
#[derive(Default)]
pub struct RoundRobin {
    next: AtomicUsize,
}

impl RoundRobin {
    pub fn new() -> RoundRobin {
        RoundRobin::default()
    }
}

impl Balance for RoundRobin {
    fn pick<'a>(&self, instances: &'a [Instance]) -> Option<&'a Instance> {
        if instances.is_empty() {
            return None;
        }
        // Wrapping: the counter is allowed to run round, and the modulus makes
        // it an index either way. A counter that saturated would send every
        // request after four billion to the same instance.
        let at = self.next.fetch_add(1, Ordering::Relaxed);
        instances.get(at % instances.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instances(count: usize) -> Vec<Instance> {
        (0..count).map(|n| Instance::new("orders", format!("10.0.0.{n}"), 8080)).collect()
    }

    #[tokio::test]
    async fn a_static_resolver_answers_only_for_the_service_asked_for() {
        let resolver = Static::new(vec![
            Instance::new("orders", "10.0.0.1", 8080),
            Instance::new("billing", "10.0.0.2", 8080),
        ]);

        assert_eq!(resolver.instances("orders").await.unwrap().len(), 1);
        // However it was cased — Eureka clients upper-case and people do not.
        assert_eq!(resolver.instances("ORDERS").await.unwrap().len(), 1);
        assert_eq!(resolver.instances("nothing").await.unwrap().len(), 0);
    }

    /// One name, because the platform is already balancing. Resolving to
    /// individual pods would mean balancing twice, and worse the second time.
    #[tokio::test]
    async fn dns_resolves_to_the_platform_name_rather_than_to_pods() {
        let resolver = Dns::new(".default.svc.cluster.local", 8080);
        let found = resolver.instances("orders").await.unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].url(), "http://orders.default.svc.cluster.local:8080");
    }

    #[test]
    fn round_robin_gives_each_instance_a_turn() {
        let pool = instances(3);
        let balance = RoundRobin::new();

        let picked: Vec<&str> =
            (0..6).filter_map(|_| balance.pick(&pool)).map(|i| i.host.as_str()).collect();
        assert_eq!(picked, ["10.0.0.0", "10.0.0.1", "10.0.0.2", "10.0.0.0", "10.0.0.1", "10.0.0.2"]);
    }

    /// Nothing to pick from is not a panic and not a default — it is `None`,
    /// and the caller turns it into a 503 that says the service has no
    /// instances.
    #[test]
    fn picking_from_nothing_is_nothing() {
        assert!(RoundRobin::new().pick(&[]).is_none());
    }

    /// A counter that saturated would send every request after four billion to
    /// the same instance, which is the sort of failure that arrives on a
    /// Tuesday eighteen months in.
    #[test]
    fn the_counter_wraps_rather_than_saturating() {
        let pool = instances(3);
        let balance = RoundRobin::new();
        balance.next.store(usize::MAX, Ordering::Relaxed);

        assert!(balance.pick(&pool).is_some());
        assert!(balance.pick(&pool).is_some());
    }
}
