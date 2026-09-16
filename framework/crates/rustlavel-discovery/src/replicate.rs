//! Keeping several registries in agreement.
//!
//! Registry nodes are peers, not a cluster with a leader. A write arrives at
//! whichever node the client reached and is forwarded to the others, so every
//! node ends up holding the same registrations by repetition rather than by
//! consensus.
//!
//! **This is deliberately not consistent, and that is the right trade.** A
//! registry is a cache of where things are, and the answer is already slightly
//! out of date the moment it is given — an instance can die between the read
//! and the request. Paying for consensus would buy agreement about a fact that
//! is stale anyway, and would mean a node that loses quorum stops answering:
//! exactly the outage the registry exists to survive. Eureka chose availability
//! here for the same reason, and the disagreements resolve themselves within a
//! heartbeat.
//!
//! **The loop is the thing to get right.** A node that forwarded what it
//! received would send it back to the node it came from, forever. Every
//! forwarded write therefore carries a marker, and a marked write is applied
//! and never forwarded on — one hop, never two. Eureka's marker is
//! `?isReplication=true`, and this honours it so the two can be mixed.

use std::time::Duration;

use rustlavel_client::{Client as Http, RequestBuilder};
use rustlavel_core::Json;
use rustlavel_http::Method;

use crate::eureka;
use crate::instance::{Instance, Status};

/// Eureka's marker, on the query string.
pub const REPLICATION_QUERY: &str = "isReplication=true";

/// The same marker as a header, for anything that strips query strings on the
/// way through — a load balancer in front of the registries, usually.
pub const REPLICATION_HEADER: &str = "x-rustlavel-replication";

/// Something one node did that the others need to know about.
#[derive(Debug, Clone)]
pub enum Change {
    Registered(Instance),
    /// Heartbeats replicate too, and must.
    ///
    /// Without them a peer would hear about an instance once and never again,
    /// and would evict every instance it did not personally receive the
    /// registration for — the whole registry emptying on every node but one.
    Renewed { service: String, id: String },
    Cancelled { service: String, id: String },
    StatusChanged { service: String, id: String, status: Status },
}

impl Change {
    fn method(&self) -> Method {
        match self {
            Change::Registered(_) => Method::Post,
            Change::Renewed { .. } | Change::StatusChanged { .. } => Method::Put,
            Change::Cancelled { .. } => Method::Delete,
        }
    }

    fn path(&self) -> String {
        let app = |service: &str| service.to_ascii_uppercase();
        match self {
            Change::Registered(instance) => format!("/eureka/apps/{}", app(&instance.service)),
            Change::Renewed { service, id } | Change::Cancelled { service, id } => {
                format!("/eureka/apps/{}/{id}", app(service))
            }
            Change::StatusChanged { service, id, status } => {
                format!("/eureka/apps/{}/{id}/status?value={}", app(service), status.as_str())
            }
        }
    }

    fn body(&self) -> Option<Json> {
        match self {
            Change::Registered(instance) => {
                Some(Json::object([("instance", eureka::instance_json(instance))]))
            }
            _ => None,
        }
    }
}

/// Whether a request arrived from another node rather than from a client.
///
/// Takes the two values already parsed out of the request — the
/// `isReplication` query parameter and the header — rather than a raw query
/// string. The server parsed that string once already, and a second parser
/// here would be a second set of rules about `+`, casing and repeated keys for
/// the two to eventually disagree about.
///
/// A marked write is applied and never forwarded on. Getting this wrong in the
/// other direction — treating a client's write as replicated — is the quieter
/// bug: the registration reaches one node and no other, and the service is
/// findable through one registry out of three.
pub fn is_replication(marker: Option<&str>, header: Option<&str>) -> bool {
    [marker, header]
        .into_iter()
        .flatten()
        // Casing varies between clients, and a marker missed is a loop.
        .any(|value| value.trim().eq_ignore_ascii_case("true"))
}

/// The other registry nodes.
#[derive(Clone)]
pub struct Peers {
    addresses: Vec<String>,
    http: Http,
}

impl Peers {
    pub fn new(addresses: Vec<String>) -> Peers {
        Peers {
            addresses,
            // Short, and shorter than the heartbeat interval: replication runs
            // on every write, and a peer that has stopped answering must be
            // given up on before the next one arrives.
            http: Http::new().timeout(Duration::from_secs(5)),
        }
    }

    /// Drop this node's own address from the list.
    ///
    /// Operators paste the same list of nodes into every node's configuration,
    /// because keeping three different lists correct is how one of them ends up
    /// wrong. This is what makes that safe.
    pub fn excluding(mut self, own: &str) -> Peers {
        let own = own.trim_end_matches('/');
        self.addresses.retain(|address| address.trim_end_matches('/') != own);
        self
    }

    pub fn http(mut self, http: Http) -> Peers {
        self.http = http;
        self
    }

    pub fn addresses(&self) -> &[String] {
        &self.addresses
    }

    fn request(&self, peer: &str, change: &Change) -> RequestBuilder {
        let path = change.path();
        let separator = if path.contains('?') { '&' } else { '?' };
        let url = format!("{}{path}{separator}{REPLICATION_QUERY}", peer.trim_end_matches('/'));

        let mut request =
            self.http.request(change.method(), url).header(REPLICATION_HEADER, "true");
        if let Some(body) = change.body() {
            request = request.json(body);
        }
        request
    }

    /// Forward a change to every peer, and say how many took it.
    ///
    /// Never fails. A peer that is down is logged and skipped: the client's
    /// registration has already succeeded here, and failing it because a
    /// third node is unreachable would make the registry less available than
    /// the thing it is keeping track of.
    pub async fn send(&self, change: &Change) -> usize {
        let mut accepted = 0;

        for peer in &self.addresses {
            match self.request(peer, change).send().await {
                Ok(response) if response.is_success() => accepted += 1,
                Ok(response) => rustlavel_core::warn!(
                    "discovery: {peer} refused a replicated change: HTTP {}",
                    response.status
                ),
                Err(error) => {
                    rustlavel_core::warn!("discovery: {peer} is unreachable: {error}")
                }
            }
        }

        accepted
    }

    /// Forward a change without waiting for the peers.
    ///
    /// What the registry's own routes call. Replication must not sit in front
    /// of the client's response — a node whose peers are slow would become slow
    /// itself, and a registration that takes a second is a service that starts
    /// a second late.
    pub fn spread(&self, change: Change) {
        if self.addresses.is_empty() {
            return;
        }
        let peers = self.clone();
        tokio::spawn(async move {
            peers.send(&change).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_client::{Fake, FakeResponse};

    fn peers(addresses: &[&str]) -> Peers {
        Peers::new(addresses.iter().map(|a| a.to_string()).collect())
    }

    /// The marker is what stops three nodes forwarding one registration to each
    /// other until somebody restarts them.
    #[test]
    fn a_forwarded_change_is_marked_both_ways() {
        let request = peers(&["http://b:8761"])
            .request("http://b:8761", &Change::Registered(Instance::new("orders", "h", 80)));

        assert!(request.url().contains(REPLICATION_QUERY), "{}", request.url());
        assert_eq!(request.headers().get(REPLICATION_HEADER), Some("true"));
    }

    #[test]
    fn eureka_s_own_marker_is_recognised() {
        assert!(is_replication(Some("true"), None));
        assert!(is_replication(Some("TRUE"), None));
        assert!(is_replication(None, Some("true")));
    }

    /// The quieter failure: a client's write mistaken for a replicated one
    /// reaches one node and no other.
    #[test]
    fn an_ordinary_request_is_not_mistaken_for_a_replicated_one() {
        assert!(!is_replication(None, None));
        assert!(!is_replication(Some(""), None));
        assert!(!is_replication(Some("false"), None));
        assert!(!is_replication(Some("1"), None), "only `true` means true");
        assert!(!is_replication(None, Some("false")));
    }

    /// Operators paste the same node list into every node.
    #[test]
    fn a_node_does_not_replicate_to_itself() {
        let peers = peers(&["http://a:8761", "http://b:8761", "http://c:8761"])
            .excluding("http://b:8761/");

        assert_eq!(peers.addresses(), ["http://a:8761", "http://c:8761"]);
    }

    #[test]
    fn each_change_goes_where_a_peer_expects_it() {
        let peers = peers(&["http://b:8761"]);
        let url_for = |change: &Change| peers.request("http://b:8761", change).url().to_string();
        let method_for = |change: &Change| peers.request("http://b:8761", change).method();

        let renew = Change::Renewed { service: "orders".into(), id: "10.0.0.1:8080".into() };
        assert_eq!(method_for(&renew), Method::Put);
        assert!(url_for(&renew).starts_with("http://b:8761/eureka/apps/ORDERS/10.0.0.1:8080?"));

        let cancel = Change::Cancelled { service: "orders".into(), id: "10.0.0.1:8080".into() };
        assert_eq!(method_for(&cancel), Method::Delete);

        // This one already carries a query string, so the marker has to join it
        // rather than start a second one.
        let status = Change::StatusChanged {
            service: "orders".into(),
            id: "10.0.0.1:8080".into(),
            status: Status::OutOfService,
        };
        let url = url_for(&status);
        assert!(url.contains("status?value=OUT_OF_SERVICE&isReplication=true"), "{url}");
        assert_eq!(url.matches('?').count(), 1, "two query strings: {url}");
    }

    #[tokio::test]
    async fn a_registration_reaches_every_peer() {
        let http = Http::new().faking(Fake::new().fallback(FakeResponse::text("")));
        let fake = std::sync::Arc::clone(http.fake().unwrap());
        let peers = peers(&["http://b:8761", "http://c:8761"]).http(http);

        let accepted = peers.send(&Change::Registered(Instance::new("orders", "h", 80))).await;

        assert_eq!(accepted, 2);
        fake.assert_sent("b:8761");
        fake.assert_sent("c:8761");
    }

    /// A node must not become less available than the services it is keeping
    /// track of.
    #[tokio::test]
    async fn a_dead_peer_does_not_stop_the_others() {
        let http = Http::new().faking(
            Fake::new()
                .on("dead", FakeResponse::text("boom").status(500))
                .fallback(FakeResponse::text("")),
        );
        let peers = peers(&["http://dead:8761", "http://alive:8761"]).http(http);

        assert_eq!(peers.send(&Change::Registered(Instance::new("orders", "h", 80))).await, 1);
    }

    #[test]
    fn a_node_with_no_peers_spawns_nothing() {
        // No runtime here on purpose: `tokio::spawn` outside one panics, so this
        // passing is the assertion.
        peers(&[]).spread(Change::Cancelled { service: "orders".into(), id: "x".into() });
    }
}
