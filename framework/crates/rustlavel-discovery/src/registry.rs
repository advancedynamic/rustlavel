//! What the registry holds, and how it decides who is alive.
//!
//! In memory, and it never evicts on a timer. Both are deliberate, and the
//! second is the interesting one.
//!
//! **Self-preservation.** An instance carries a `last_seen`; a *read* decides
//! who is alive. When most of the registry goes stale at once, a network
//! partition is far more likely than every service dying in the same second —
//! so the registry says so and keeps serving the stale list rather than
//! emptying itself. A registry that empties during a blink turns the blink into
//! an outage, and the outage outlasts the blink because everything then has to
//! re-register before anything can find anything.
//!
//! Eureka has had this since the beginning and it is its most misunderstood
//! feature: people disable it because "stale instances got traffic", which is
//! true and is the smaller of the two failures.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::instance::{Instance, now};

/// How much of the registry must go stale before it stops believing the
/// staleness.
///
/// Eureka expresses the same idea as a renewal threshold — it expects a certain
/// number of heartbeats per minute and protects itself when it receives fewer
/// than 85% of them. The number here is the share that may be stale *before*
/// the registry concludes the problem is its own: lose more than this at once
/// and the simplest explanation stops being "they died".
pub const SELF_PRESERVATION_THRESHOLD: f64 = 0.15;

/// The registry's view of the world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Stale instances are being hidden, which is the ordinary state.
    Evicting,
    /// Too much went stale at once to believe it. Everything is being served,
    /// stale included.
    ///
    /// **This must be visible.** Eureka shows it as a red banner, and without
    /// one somebody spends an afternoon working out why a dead instance is
    /// still getting requests.
    SelfPreservation,
}

/// Instances, by service.
///
/// Cloning shares the registry, so it can be registered as application state.
#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<RwLock<HashMap<String, Vec<Instance>>>>,
}

impl Registry {
    pub fn new() -> Registry {
        Registry::default()
    }

    /// Add an instance, or refresh the one already there.
    ///
    /// Re-registering is not an error: a restarted instance registers again
    /// with the same id, and treating that as a conflict would mean an instance
    /// could not come back until something else forgot it.
    pub fn register(&self, instance: Instance) {
        let mut held = self.inner.write().expect("the registry lock is poisoned");
        let instances = held.entry(instance.service.clone()).or_default();

        match instances.iter_mut().find(|held| held.id == instance.id) {
            Some(existing) => *existing = instance,
            None => instances.push(instance),
        }
    }

    /// Record a heartbeat. `false` when nothing is registered under that id —
    /// which is how a client learns it must register again, and is exactly what
    /// happens to every client after the registry restarts.
    pub fn renew(&self, service: &str, id: &str) -> bool {
        let service = service.to_ascii_uppercase();
        let mut held = self.inner.write().expect("the registry lock is poisoned");
        let Some(instances) = held.get_mut(&service) else { return false };
        let Some(instance) = instances.iter_mut().find(|held| held.id == id) else { return false };
        instance.last_seen = now();
        true
    }

    /// Remove an instance, as a service does on the way down.
    ///
    /// The difference between a clean deploy and thirty seconds of failures: an
    /// instance that says goodbye leaves immediately, and one that is killed has
    /// to be noticed.
    pub fn cancel(&self, service: &str, id: &str) -> bool {
        let service = service.to_ascii_uppercase();
        let mut held = self.inner.write().expect("the registry lock is poisoned");
        let Some(instances) = held.get_mut(&service) else { return false };
        let before = instances.len();
        instances.retain(|held| held.id != id);
        let removed = instances.len() != before;

        // A service with no instances is not a service. Leaving the empty entry
        // would put a name on the dashboard that nothing answers to.
        if instances.is_empty() {
            held.remove(&service);
        }
        removed
    }

    /// Change an instance's status without touching anything else.
    ///
    /// How a deploy drains: set `OUT_OF_SERVICE`, wait for in-flight requests,
    /// then stop. The instance stays registered and keeps heartbeating the
    /// whole time, so nothing concludes it died.
    pub fn set_status(&self, service: &str, id: &str, status: crate::Status) -> bool {
        let service = service.to_ascii_uppercase();
        let mut held = self.inner.write().expect("the registry lock is poisoned");
        let Some(instances) = held.get_mut(&service) else { return false };
        let Some(instance) = instances.iter_mut().find(|held| held.id == id) else { return false };
        instance.status = status;
        instance.last_seen = now();
        true
    }

    /// Everything registered, stale included. For the dashboard and for
    /// replication, both of which want the truth rather than the filtered view.
    pub fn all(&self) -> Vec<Instance> {
        self.inner
            .read()
            .expect("the registry lock is poisoned")
            .values()
            .flatten()
            .cloned()
            .collect()
    }

    /// The instances a caller should be given, and why.
    ///
    /// Ordinarily: fresh and `UP`. Under self-preservation: everything `UP`,
    /// stale included, because the staleness is more likely to be about the
    /// registry's own network than about the instances.
    pub fn alive(&self, service: &str) -> (Vec<Instance>, Mode) {
        let mode = self.mode();
        let service = service.to_ascii_uppercase();
        let at = now();

        let held = self.inner.read().expect("the registry lock is poisoned");
        let instances = held
            .get(&service)
            .map(|instances| {
                instances
                    .iter()
                    .filter(|instance| instance.status.takes_traffic())
                    .filter(|instance| mode == Mode::SelfPreservation || instance.fresh_at(at))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        (instances, mode)
    }

    /// Whether the registry currently believes its own staleness.
    pub fn mode(&self) -> Mode {
        let at = now();
        let held = self.inner.read().expect("the registry lock is poisoned");

        let mut total = 0usize;
        let mut stale = 0usize;
        for instance in held.values().flatten() {
            total += 1;
            if !instance.fresh_at(at) {
                stale += 1;
            }
        }

        // An empty registry is not in trouble, and neither is a small one: with
        // three instances, one stale is 33% and means one instance died. The
        // threshold is about *proportion at scale*, and applying it to a
        // handful would put a registry into self-preservation on its first
        // ordinary failure.
        if total < 8 {
            return Mode::Evicting;
        }

        match stale as f64 / total as f64 > SELF_PRESERVATION_THRESHOLD {
            true => Mode::SelfPreservation,
            false => Mode::Evicting,
        }
    }

    /// Service names, for the dashboard.
    pub fn services(&self) -> Vec<String> {
        let mut names: Vec<String> =
            self.inner.read().expect("the registry lock is poisoned").keys().cloned().collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::Status;

    fn aged(service: &str, id: &str, seconds_ago: u64) -> Instance {
        let mut instance = Instance::new(service, "10.0.0.1", 8080).id(id).renew_every(30);
        instance.last_seen = now().saturating_sub(seconds_ago);
        instance
    }

    #[test]
    fn registering_twice_refreshes_rather_than_duplicating() {
        let registry = Registry::new();
        registry.register(Instance::new("orders", "10.0.0.1", 8080));
        registry.register(Instance::new("orders", "10.0.0.1", 8080));
        assert_eq!(registry.all().len(), 1, "a restarted instance became two");
    }

    /// A client learns from a refused heartbeat that it must register again —
    /// which is what every client has to do after the registry restarts.
    #[test]
    fn a_heartbeat_for_something_unregistered_is_refused() {
        let registry = Registry::new();
        assert!(!registry.renew("orders", "10.0.0.1:8080"));

        registry.register(Instance::new("orders", "10.0.0.1", 8080));
        assert!(registry.renew("orders", "10.0.0.1:8080"));
        // And the name is matched however it was cased, because Eureka clients
        // upper-case and people do not.
        assert!(registry.renew("ORDERS", "10.0.0.1:8080"));
    }

    #[test]
    fn a_stale_instance_is_not_offered() {
        let registry = Registry::new();
        registry.register(aged("orders", "fresh", 0));
        registry.register(aged("orders", "gone", 200));

        let (alive, mode) = registry.alive("orders");
        assert_eq!(mode, Mode::Evicting);
        assert_eq!(alive.len(), 1);
        assert_eq!(alive[0].id, "fresh");
    }

    /// Draining: still registered, still heartbeating, deliberately not sent
    /// traffic.
    #[test]
    fn an_instance_out_of_service_keeps_its_registration_and_loses_its_traffic() {
        let registry = Registry::new();
        registry.register(Instance::new("orders", "10.0.0.1", 8080));
        assert!(registry.set_status("orders", "10.0.0.1:8080", Status::OutOfService));

        assert_eq!(registry.alive("orders").0.len(), 0, "a draining instance was given traffic");
        assert_eq!(registry.all().len(), 1, "a draining instance lost its registration");
    }

    /// The whole point of self-preservation: when most of the registry goes
    /// stale in the same moment, the simplest explanation stops being that they
    /// all died.
    #[test]
    fn losing_most_of_the_registry_at_once_is_treated_as_a_partition() {
        let registry = Registry::new();
        for n in 0..10 {
            registry.register(aged("orders", &format!("gone-{n}"), 200));
        }
        for n in 0..2 {
            registry.register(aged("orders", &format!("fresh-{n}"), 0));
        }

        let (alive, mode) = registry.alive("orders");
        assert_eq!(mode, Mode::SelfPreservation);
        assert_eq!(alive.len(), 12, "the stale instances were dropped during a partition");
    }

    /// And one death among many is still a death.
    #[test]
    fn one_instance_dying_among_many_is_still_evicted() {
        let registry = Registry::new();
        for n in 0..20 {
            registry.register(aged("orders", &format!("fresh-{n}"), 0));
        }
        registry.register(aged("orders", "gone", 200));

        let (alive, mode) = registry.alive("orders");
        assert_eq!(mode, Mode::Evicting);
        assert_eq!(alive.len(), 20);
    }

    /// With three instances, one stale is a third of the registry — and it
    /// means one instance died, not that the network broke. A proportion
    /// threshold applied to a handful fires on the first ordinary failure.
    #[test]
    fn a_small_registry_does_not_protect_itself_on_a_single_death() {
        let registry = Registry::new();
        registry.register(aged("orders", "a", 0));
        registry.register(aged("orders", "b", 0));
        registry.register(aged("orders", "c", 200));

        assert_eq!(registry.mode(), Mode::Evicting);
        assert_eq!(registry.alive("orders").0.len(), 2);
    }

    #[test]
    fn cancelling_removes_the_instance_and_then_the_empty_service() {
        let registry = Registry::new();
        registry.register(Instance::new("orders", "10.0.0.1", 8080));
        registry.cancel("orders", "10.0.0.1:8080");

        assert!(registry.all().is_empty());
        assert!(registry.services().is_empty(), "an empty service was left behind");
    }
}
