//! Where a request goes, and what it looks like when it gets there.
//!
//! A table of patterns rather than three hard-wired services: an upstream is
//! something you deployed *or* somebody else's API, and a gateway that can only
//! reach its own services is one every application ends up routing around.

/// Somewhere a request can be sent.
#[derive(Debug, Clone)]
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
}

impl Upstream {
    /// `Upstream::at("http://127.0.0.1:9002")`.
    pub fn at(base: impl Into<String>) -> Upstream {
        let base = base.into();
        Upstream { base: base.trim_end_matches('/').to_string(), strip: None, host: None }
    }

    pub fn strip(mut self, prefix: impl Into<String>) -> Upstream {
        self.strip = Some(prefix.into());
        self
    }

    pub fn host(mut self, host: impl Into<String>) -> Upstream {
        self.host = Some(host.into());
        self
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn forwarded_host(&self) -> Option<&str> {
        self.host.as_deref()
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
