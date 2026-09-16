//! One running copy of a service, and what the registry knows about it.

use std::time::{SystemTime, UNIX_EPOCH};

/// What an instance says about itself.
///
/// Separate from whether the registry can still *hear* it — that is `last_seen`,
/// and the two are different facts. An instance can be `Up` and unheard from
/// for a minute, which is a network problem; it can be heard from every second
/// and `OutOfService`, which is a deliberate drain before a deploy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Up,
    Down,
    Starting,
    /// Taken out of rotation on purpose. Still registered, still heartbeating,
    /// and deliberately not sent traffic — this is how a deploy drains an
    /// instance without the registry deciding it died.
    OutOfService,
}

impl Status {
    pub fn parse(text: &str) -> Status {
        match text.trim().to_ascii_uppercase().as_str() {
            "UP" => Status::Up,
            "STARTING" => Status::Starting,
            "OUT_OF_SERVICE" => Status::OutOfService,
            // Anything unrecognised is `Down`, not `Up`. A status this code
            // does not understand must not become traffic.
            _ => Status::Down,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Up => "UP",
            Status::Down => "DOWN",
            Status::Starting => "STARTING",
            Status::OutOfService => "OUT_OF_SERVICE",
        }
    }

    /// Whether this instance should be given traffic.
    pub fn takes_traffic(self) -> bool {
        self == Status::Up
    }
}

/// A registered instance.
#[derive(Debug, Clone)]
pub struct Instance {
    /// The service this belongs to. Upper-cased, because Eureka upper-cases
    /// application names and a registry that disagrees with its own clients
    /// about the name of a service is worse than one that shouts.
    pub service: String,
    /// Unique within the service. `host:port` when nothing better is given.
    pub id: String,
    pub host: String,
    pub port: u16,
    pub secure: bool,
    pub status: Status,
    /// Free-form, and the place zone and version live. A registry that fixed
    /// the set of things an instance may say about itself would be a registry
    /// somebody works around within a month.
    pub metadata: Vec<(String, String)>,
    /// Seconds between heartbeats, as the instance says it will send them.
    ///
    /// The instance decides, not the registry: only the instance knows whether
    /// it is a service that can promise every five seconds or a batch worker
    /// that cannot.
    pub renew_interval: u64,
    /// Unix seconds when the registry last heard from it.
    pub last_seen: u64,
}

impl Instance {
    pub fn new(service: impl Into<String>, host: impl Into<String>, port: u16) -> Instance {
        let host = host.into();
        let service = service.into().to_ascii_uppercase();
        Instance {
            id: format!("{host}:{port}"),
            service,
            host,
            port,
            secure: false,
            status: Status::Up,
            metadata: Vec::new(),
            renew_interval: 30,
            last_seen: now(),
        }
    }

    pub fn id(mut self, id: impl Into<String>) -> Instance {
        self.id = id.into();
        self
    }

    pub fn secure(mut self, secure: bool) -> Instance {
        self.secure = secure;
        self
    }

    pub fn status(mut self, status: Status) -> Instance {
        self.status = status;
        self
    }

    pub fn meta(mut self, key: impl Into<String>, value: impl Into<String>) -> Instance {
        self.metadata.push((key.into(), value.into()));
        self
    }

    pub fn renew_every(mut self, seconds: u64) -> Instance {
        self.renew_interval = seconds.max(1);
        self
    }

    /// Where to send a request.
    pub fn url(&self) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        format!("{scheme}://{}:{}", self.host, self.port)
    }

    pub fn zone(&self) -> Option<&str> {
        self.meta_value("zone")
    }

    pub fn meta_value(&self, key: &str) -> Option<&str> {
        self.metadata.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Whether the registry has heard from it recently enough.
    ///
    /// Three missed heartbeats, not one. A single miss is a garbage collection
    /// pause or a dropped packet; three in a row is a service that has stopped
    /// talking. Eureka's own default is the same shape — a ninety-second lease
    /// against a thirty-second renewal.
    pub fn fresh_at(&self, now: u64) -> bool {
        now.saturating_sub(self.last_seen) < self.renew_interval * 3
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A status this code does not understand must not become traffic.
    #[test]
    fn an_unknown_status_is_down_rather_than_up() {
        assert_eq!(Status::parse("UP"), Status::Up);
        assert_eq!(Status::parse("up"), Status::Up);
        assert_eq!(Status::parse("OUT_OF_SERVICE"), Status::OutOfService);
        for rubbish in ["", "MAYBE", "ok", "1", "UNKNOWN"] {
            assert_eq!(Status::parse(rubbish), Status::Down, "{rubbish:?}");
        }
    }

    /// Draining before a deploy: still registered, still heartbeating, and
    /// deliberately not sent traffic.
    #[test]
    fn only_up_takes_traffic() {
        assert!(Status::Up.takes_traffic());
        for other in [Status::Down, Status::Starting, Status::OutOfService] {
            assert!(!other.takes_traffic(), "{:?} was given traffic", other);
        }
    }

    /// One miss is a GC pause. Three is a service that stopped talking.
    #[test]
    fn freshness_allows_two_missed_heartbeats() {
        let instance = Instance::new("orders", "10.0.0.1", 8080).renew_every(30);
        let registered = instance.last_seen;

        assert!(instance.fresh_at(registered + 29), "one interval is not a death");
        assert!(instance.fresh_at(registered + 89), "two misses is not a death");
        assert!(!instance.fresh_at(registered + 90), "three misses is");
    }

    #[test]
    fn a_service_name_is_upper_cased_to_match_what_eureka_clients_send() {
        assert_eq!(Instance::new("orders", "h", 1).service, "ORDERS");
        assert_eq!(Instance::new("Orders", "h", 1).service, "ORDERS");
    }

    #[test]
    fn an_instance_knows_its_own_url_and_defaults_its_id_to_host_and_port() {
        let plain = Instance::new("orders", "10.0.0.1", 8080);
        assert_eq!(plain.url(), "http://10.0.0.1:8080");
        assert_eq!(plain.id, "10.0.0.1:8080");

        let tls = Instance::new("orders", "orders.example.com", 443).secure(true);
        assert_eq!(tls.url(), "https://orders.example.com:443");
    }

    #[test]
    fn zone_is_metadata_rather_than_a_field_of_its_own() {
        let instance = Instance::new("orders", "h", 1).meta("zone", "jakarta-a");
        assert_eq!(instance.zone(), Some("jakarta-a"));
        assert_eq!(Instance::new("orders", "h", 1).zone(), None);
    }
}
