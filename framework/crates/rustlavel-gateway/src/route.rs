//! Where a request goes, and what it looks like when it gets there.
//!
//! A table of patterns rather than three hard-wired services: an upstream is
//! something you deployed *or* somebody else's API, and a gateway that can only
//! reach its own services is one every application ends up routing around.

/// Something that turns a service name into an address.
///
/// Defined here rather than taken from a discovery crate, so a gateway with a
/// table of fixed addresses — which is most gateways — compiles nothing it does
/// not use. `rustlavel-discovery` implements it behind its `gateway` feature.
pub trait Locate: Send + Sync {
    /// The base URL of an instance to send the next request to, or `None` when
    /// the service has none running.
    fn base_for<'a>(
        &'a self,
        service: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;
}

/// Somewhere a request can be sent.
#[derive(Clone)]
pub struct Upstream {
    /// Scheme, host and port. No trailing slash — [`Upstream::at`] takes it off,
    /// because `http://host/` plus `/api/x` is `http://host//api/x`, and some
    /// servers treat that as a different path.
    base: String,
    /// A prefix removed from the path before forwarding.
    ///
    /// `/maps/roads` behind `.strip("/maps")` arrives upstream as `/roads`. The
    /// gateway's own routing vocabulary is rarely the upstream's.
    strip: Option<String>,
    /// The `Host` header sent upstream, when it must not be the gateway's.
    ///
    /// A virtual host or a CDN often routes on it, so passing the gateway's
    /// name through would reach the wrong site — while overriding it for an
    /// internal service would break nothing and hide which name was asked for.
    host: Option<String>,
    /// Set when the address is looked up per request rather than written down.
    locate: Option<(String, std::sync::Arc<dyn Locate>)>,
    /// Reachable without a credential even when the gateway requires one.
    open: bool,
}

// Hand-written because a `dyn Locate` is not `Debug`, and because the useful
// thing to print is where the request goes, not the resolver's address.
impl std::fmt::Debug for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upstream")
            .field("base", &self.base)
            .field("strip", &self.strip)
            .field("host", &self.host)
            .field("discovered", &self.locate.is_some())
            .field("open", &self.open)
            .finish()
    }
}

impl Upstream {
    /// `Upstream::at("http://127.0.0.1:9002")`.
    pub fn at(base: impl Into<String>) -> Upstream {
        let base = base.into();
        Upstream {
            base: base.trim_end_matches('/').to_string(),
            strip: None,
            host: None,
            locate: None,
            open: false,
        }
    }

    /// `Upstream::service("orders", locator)` — an address looked up for every
    /// request rather than written down.
    ///
    /// The base reads `service://ORDERS` until it is resolved, which is what
    /// appears in the health report and in a log line. That is deliberate: a
    /// gateway that printed a resolved address there would be describing one
    /// request rather than the route.
    pub fn service(name: impl Into<String>, locate: std::sync::Arc<dyn Locate>) -> Upstream {
        let name = name.into().to_ascii_uppercase();
        Upstream {
            base: format!("service://{name}"),
            strip: None,
            host: None,
            locate: Some((name, locate)),
            open: false,
        }
    }

    pub fn strip(mut self, prefix: impl Into<String>) -> Upstream {
        self.strip = Some(prefix.into());
        self
    }

    pub fn host(mut self, host: impl Into<String>) -> Upstream {
        self.host = Some(host.into());
        self
    }

    /// Reachable without a credential, even behind [`Gateway::require_token`].
    ///
    /// **An authorization server needs this and cannot work without it.**
    /// Getting a token is what somebody does *before* they have one, so a
    /// gateway that required a credential everywhere would put the token
    /// endpoint behind a door that only opens from the inside — the request
    /// comes back `401`, and there is no way to obtain the thing that would
    /// have made it succeed.
    ///
    /// Opt-in per route, so opening one is a line somebody wrote rather than a
    /// hole in a default. The service behind it still checks for itself.
    pub fn open(mut self) -> Upstream {
        self.open = true;
        self
    }

    /// Whether this route is reachable without a credential.
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn forwarded_host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// The service name this route resolves, when it resolves one.
    pub fn service_name(&self) -> Option<&str> {
        self.locate.as_ref().map(|(name, _)| name.as_str())
    }

    /// This upstream with a real address in it.
    ///
    /// A fixed upstream is borrowed and nothing is copied — which is every
    /// request through most gateways. `None` means the service resolved to
    /// nothing, and the caller answers `503`: the gateway is working and the
    /// service has no instances, which is a different thing from an instance
    /// refusing the request.
    pub async fn resolved(&self) -> Option<std::borrow::Cow<'_, Upstream>> {
        let Some((name, locate)) = &self.locate else {
            return Some(std::borrow::Cow::Borrowed(self));
        };

        let base = locate.base_for(name).await?;
        let mut resolved = self.clone();
        resolved.base = base.trim_end_matches('/').to_string();
        // Taken off, so a resolved upstream cannot be resolved twice — and so
        // the clone does not keep the resolver alive any longer than the
        // request.
        resolved.locate = None;
        Some(std::borrow::Cow::Owned(resolved))
    }

    /// The full URL this request becomes upstream.
    ///
    /// `target` is the path and query as it arrived. The prefix comes off, and
    /// a path that strips down to nothing becomes `/` rather than the empty
    /// string — `GET ` with no target is not a request.
    pub fn url_for(&self, target: &str) -> String {
        let path = match &self.strip {
            Some(prefix) => target.strip_prefix(prefix.as_str()).unwrap_or(target),
            None => target,
        };
        let path = if path.is_empty() { "/" } else { path };
        let separator = if path.starts_with('/') { "" } else { "/" };
        format!("{}{separator}{path}", self.base)
    }
}

/// The table, longest pattern first.
#[derive(Debug, Clone, Default)]
pub struct Routes {
    entries: Vec<(String, Upstream)>,
}

impl Routes {
    pub fn new() -> Routes {
        Routes::default()
    }

    /// Add a route. `"/api/*"` matches everything under `/api/`; `"/health"`
    /// matches only itself.
    ///
    /// Order of declaration does not decide the winner — length does, so a
    /// specific route added first is not shadowed by a general one added later.
    /// Registration order deciding a match is the kind of rule that works until
    /// somebody sorts the file.
    pub fn add(&mut self, pattern: impl Into<String>, upstream: Upstream) {
        self.entries.push((pattern.into(), upstream));
        self.entries.sort_by_key(|(pattern, _)| std::cmp::Reverse(pattern.len()));
    }

    /// The upstream for a path, or nothing.
    pub fn find(&self, path: &str) -> Option<&Upstream> {
        self.entries.iter().find_map(|(pattern, upstream)| {
            match pattern.strip_suffix('*') {
                Some(prefix) => path.starts_with(prefix).then_some(upstream),
                None => (path == pattern).then_some(upstream),
            }
        })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every upstream, for the aggregate health check.
    pub fn upstreams(&self) -> impl Iterator<Item = (&str, &Upstream)> {
        self.entries.iter().map(|(pattern, upstream)| (pattern.as_str(), upstream))
    }
}

/// Headers that describe *this* connection and must not be passed on.
///
/// RFC 7230 §6.1. Forwarding `Connection` or `Transfer-Encoding` makes the
/// gateway and its upstream disagree about where the message ends, which is
/// request smuggling one layer up — and `Content-Length` is recomputed for the
/// body actually sent, so passing the old one along would be a second answer to
/// the same question.
pub const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "host",
];

/// Whether a header should cross the gateway.
pub fn is_forwardable(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    !HOP_BY_HOP.contains(&name.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A resolver that answers whatever the test tells it to.
    struct Fixed(Option<String>);

    impl Locate for Fixed {
        fn base_for<'a>(
            &'a self,
            _service: &'a str,
        ) -> std::pin::Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
            let answer = self.0.clone();
            Box::pin(async move { answer })
        }
    }

    #[tokio::test]
    async fn a_fixed_upstream_resolves_to_itself_without_copying() {
        let upstream = Upstream::at("http://api:9000");
        let resolved = upstream.resolved().await.expect("a fixed upstream always resolves");

        assert!(matches!(resolved, std::borrow::Cow::Borrowed(_)), "a fixed route was cloned");
        assert_eq!(resolved.base(), "http://api:9000");
    }

    #[tokio::test]
    async fn a_named_service_takes_its_address_from_the_resolver() {
        let upstream = Upstream::service(
            "orders",
            std::sync::Arc::new(Fixed(Some("http://10.0.0.7:8080/".into()))),
        )
        .strip("/api");

        assert_eq!(upstream.service_name(), Some("ORDERS"));
        // Before resolving, the route describes itself rather than one request.
        assert_eq!(upstream.base(), "service://ORDERS");

        let resolved = upstream.resolved().await.expect("an instance was available");
        assert_eq!(resolved.base(), "http://10.0.0.7:8080", "the trailing slash survived");
        // And everything else about the route came along.
        assert_eq!(resolved.url_for("/api/orders/7"), "http://10.0.0.7:8080/orders/7");
    }

    /// `None`, not an empty base. A service with nothing running is a 503, and
    /// a gateway that forwarded to `/orders/7` with no host would produce
    /// something much harder to read.
    #[tokio::test]
    async fn a_service_with_no_instances_resolves_to_nothing() {
        let upstream = Upstream::service("orders", std::sync::Arc::new(Fixed(None)));
        assert!(upstream.resolved().await.is_none());
    }

    /// A resolved upstream must not carry the resolver into the next request.
    #[tokio::test]
    async fn resolving_is_not_repeatable() {
        let upstream = Upstream::service(
            "orders",
            std::sync::Arc::new(Fixed(Some("http://10.0.0.7:8080".into()))),
        );
        let resolved = upstream.resolved().await.unwrap().into_owned();

        assert_eq!(resolved.service_name(), None);
        assert_eq!(resolved.resolved().await.unwrap().base(), "http://10.0.0.7:8080");
    }

    #[test]
    fn a_wildcard_matches_everything_under_it_and_an_exact_route_only_itself() {
        let mut routes = Routes::new();
        routes.add("/api/*", Upstream::at("http://api:9002"));
        routes.add("/health", Upstream::at("http://health:9003"));

        assert_eq!(routes.find("/api/orders").map(Upstream::base), Some("http://api:9002"));
        assert_eq!(routes.find("/health").map(Upstream::base), Some("http://health:9003"));
        assert!(routes.find("/health/deep").is_none(), "an exact route matched a longer path");
        assert!(routes.find("/nothing").is_none());
    }

    /// The specific route wins whichever order it was added in. Registration
    /// order deciding a match works until somebody sorts the file.
    #[test]
    fn the_more_specific_route_wins_regardless_of_order() {
        let mut general = Routes::new();
        general.add("/api/*", Upstream::at("http://general"));
        general.add("/api/billing/*", Upstream::at("http://billing"));

        let mut reversed = Routes::new();
        reversed.add("/api/billing/*", Upstream::at("http://billing"));
        reversed.add("/api/*", Upstream::at("http://general"));

        for routes in [general, reversed] {
            assert_eq!(routes.find("/api/billing/invoices").map(Upstream::base), Some("http://billing"));
            assert_eq!(routes.find("/api/orders").map(Upstream::base), Some("http://general"));
        }
    }

    #[test]
    fn a_stripped_prefix_does_not_reach_the_upstream() {
        let upstream = Upstream::at("https://api.example.com").strip("/maps");
        assert_eq!(upstream.url_for("/maps/roads?zoom=3"), "https://api.example.com/roads?zoom=3");
    }

    /// `GET` with an empty target is not a request, and `http://host//path` is
    /// a different path to some servers than `http://host/path`.
    #[test]
    fn a_path_that_strips_to_nothing_becomes_a_slash() {
        let upstream = Upstream::at("http://service/").strip("/maps");
        assert_eq!(upstream.url_for("/maps"), "http://service/");
        assert_eq!(upstream.url_for("/maps/"), "http://service/");
    }

    /// Forwarding a hop-by-hop header makes the gateway and its upstream
    /// disagree about where the message ends. That is request smuggling with an
    /// extra hop.
    #[test]
    fn connection_headers_do_not_cross_the_gateway() {
        for refused in ["Connection", "transfer-encoding", "Keep-Alive", "content-length", "Host"] {
            assert!(!is_forwardable(refused), "{refused} would have been passed on");
        }
        for allowed in ["authorization", "content-type", "accept", "x-request-id"] {
            assert!(is_forwardable(allowed), "{allowed} was dropped");
        }
    }
}
