//! Whose `X-Forwarded-For` to believe.
//!
//! Behind a load balancer every request arrives from the balancer's address,
//! so the client's real address is only available in a header the balancer
//! added. The trap is that a *header* is something any client can send. A
//! server that reads `X-Forwarded-For` from whoever supplies it has not
//! learned the client's address; it has let the client choose one. Anything
//! keyed on that address — a rate limiter, an audit log, a block list — is
//! then trivially defeated by a header.
//!
//! So the header is believed only when the connection came from a proxy that
//! was named in advance:
//!
//! ```ignore
//! App::new()?.middleware(TrustProxies::from_config(app.config()))
//! // or
//! App::new()?.middleware(TrustProxies::at(["10.0.0.0/8", "172.16.0.0/12"]))
//! ```
//!
//! Without this middleware, [`Request::ip`] is the address of whoever opened
//! the socket, which is always true even when it is not always useful. With
//! it, and only for a connection from a trusted proxy, it becomes the
//! left-most address in the forwarded chain that the trusted proxies did not
//! themselves add.
//!
//! The same applies to the scheme. A proxy that terminates TLS forwards a
//! plain HTTP request with `X-Forwarded-Proto: https`, and without that
//! header an application would generate `http://` links on a site that is
//! entirely `https://`.
//!
//! `TrustProxies::any()` exists and is documented as what it is: correct on a
//! platform where nothing but the platform's own proxy can reach the process
//! — a Heroku dyno, a Cloud Run container, a pod behind an ingress with no
//! other route in — and a hole anywhere else.

use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use rustlavel_core::Config;
use std::net::IpAddr;

/// What a trusted proxy told us about the original client.
#[derive(Debug, Clone, Default)]
pub struct Forwarded {
    /// The client address, when the chain named one.
    pub ip: Option<String>,
    /// `http` or `https`, when the proxy said.
    pub scheme: Option<String>,
    /// The `Host` the client asked for, when the proxy said.
    pub host: Option<String>,
    /// The port the client connected to, when the proxy said.
    pub port: Option<u16>,
}

/// One entry in the trust list: a single address, a CIDR range, or everything.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Trusted {
    Any,
    Address(IpAddr),
    Network { base: IpAddr, prefix: u8 },
}

impl Trusted {
    fn parse(entry: &str) -> Option<Trusted> {
        let entry = entry.trim();
        if entry == "*" || entry.eq_ignore_ascii_case("any") {
            return Some(Trusted::Any);
        }
        match entry.split_once('/') {
            None => entry.parse().ok().map(Trusted::Address),
            Some((base, prefix)) => {
                let base: IpAddr = base.trim().parse().ok()?;
                let prefix: u8 = prefix.trim().parse().ok()?;
                let width = if base.is_ipv4() { 32 } else { 128 };
                (prefix <= width).then_some(Trusted::Network { base, prefix })
            }
        }
    }

    fn contains(&self, address: IpAddr) -> bool {
        match self {
            Trusted::Any => true,
            Trusted::Address(trusted) => *trusted == address,
            Trusted::Network { base, prefix } => in_network(*base, *prefix, address),
        }
    }
}

/// Whether an address falls inside a CIDR block.
///
/// Compared over the raw octets rather than as numbers, so one routine covers
/// both address families and a `/48` of IPv6 needs no special case. A v4 and a
/// v6 address never match each other, including a v4-mapped v6 address: two
/// spellings of one host are still two different things to a config file, and
/// silently equating them would let a `/8` of private v4 space quietly cover
/// addresses nobody listed.
fn in_network(base: IpAddr, prefix: u8, address: IpAddr) -> bool {
    let (base, address) = match (base, address) {
        (IpAddr::V4(base), IpAddr::V4(address)) => (base.octets().to_vec(), address.octets().to_vec()),
        (IpAddr::V6(base), IpAddr::V6(address)) => (base.octets().to_vec(), address.octets().to_vec()),
        _ => return false,
    };

    let whole_bytes = (prefix / 8) as usize;
    if base[..whole_bytes] != address[..whole_bytes] {
        return false;
    }
    let leftover = prefix % 8;
    if leftover == 0 {
        return true;
    }
    let mask = 0xFFu8 << (8 - leftover);
    base[whole_bytes] & mask == address[whole_bytes] & mask
}

#[derive(Debug, Clone, Default)]
pub struct TrustProxies {
    proxies: Vec<Trusted>,
}

impl TrustProxies {
    /// Trust nobody. Every forwarded header is ignored, which is the default
    /// and is right for a process reached directly from the internet.
    pub fn none() -> Self {
        TrustProxies::default()
    }

    /// Trust these addresses and ranges: `"10.0.0.0/8"`, `"192.168.1.7"`,
    /// `"2001:db8::/32"`.
    ///
    /// An entry that is not an address or a CIDR block is dropped rather than
    /// silently widening the list — a typo must never mean "trust everyone".
    pub fn at<I, S>(proxies: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        TrustProxies {
            proxies: proxies.into_iter().filter_map(|p| Trusted::parse(p.as_ref())).collect(),
        }
    }

    /// Trust whatever opened the connection.
    ///
    /// Correct only where nothing but the platform's own proxy can reach this
    /// process, and where that proxy replaces the forwarded headers rather
    /// than appending to them: a Heroku dyno, a Cloud Run container, a pod
    /// whose only ingress is the ingress. Anywhere a client can open a socket
    /// to the application directly, this hands every client the ability to
    /// choose its own address.
    pub fn any() -> Self {
        TrustProxies { proxies: vec![Trusted::Any] }
    }

    /// Read `trustedproxy.proxies` — an array, or a comma-separated string so
    /// it can come from `.env`. `*` means [`TrustProxies::any`].
    ///
    /// The key is Laravel's, from `config/trustedproxy.php`.
    pub fn from_config(config: &Config) -> Self {
        TrustProxies::at(config.list("trustedproxy.proxies"))
    }

    fn trusts(&self, address: IpAddr) -> bool {
        self.proxies.iter().any(|proxy| proxy.contains(address))
    }

    /// The client address from a forwarded chain, discarding the trailing
    /// entries the trusted proxies added themselves.
    ///
    /// `X-Forwarded-For: client, proxy-a, proxy-b` is read right to left: each
    /// trusted hop is dropped, and the first address that is not one of ours
    /// is the client. Taking the left-most entry instead would take whatever
    /// the client wrote there before the first proxy appended to it.
    fn client_from(&self, chain: &str) -> Option<String> {
        let hops: Vec<&str> = chain.split(',').map(str::trim).filter(|h| !h.is_empty()).collect();
        for hop in hops.iter().rev() {
            let address = strip_port(hop).parse::<IpAddr>().ok()?;
            if !self.trusts(address) {
                return Some(address.to_string());
            }
        }
        // Every hop was a proxy we trust, so the nearest one is the best answer
        // available — this is a proxy calling the application about itself.
        hops.first().map(|hop| strip_port(hop).to_string())
    }
}

/// `1.2.3.4:5678` and `[::1]:80` down to the address.
fn strip_port(hop: &str) -> &str {
    let hop = hop.trim();
    if let Some(rest) = hop.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(hop);
    }
    // A bare IPv6 address has several colons; only strip a single trailing one.
    match hop.rsplit_once(':') {
        Some((address, _)) if !address.contains(':') => address,
        _ => hop,
    }
}

impl Middleware for TrustProxies {
    fn handle(&self, mut request: Request, next: Next) -> BoxFuture<Response> {
        let peer = request.peer_addr().map(|addr| addr.ip());
        // No trust list, or a connection from somewhere not on it: the headers
        // are whatever the client chose to send, and are ignored.
        if !peer.is_some_and(|peer| self.trusts(peer)) {
            return next.run(request);
        }

        let mut forwarded = Forwarded::default();
        if let Some(chain) = request.header("x-forwarded-for") {
            forwarded.ip = self.client_from(chain);
        }
        if let Some(scheme) = request.header("x-forwarded-proto") {
            let scheme = scheme.split(',').next().unwrap_or("").trim().to_ascii_lowercase();
            if scheme == "http" || scheme == "https" {
                forwarded.scheme = Some(scheme);
            }
        }
        if let Some(host) = request.header("x-forwarded-host")
            && let Some(host) = host.split(',').next().map(str::trim).filter(|h| !h.is_empty())
        {
            forwarded.host = Some(host.to_string());
        }
        if let Some(port) = request.header("x-forwarded-port") {
            forwarded.port = port.split(',').next().and_then(|p| p.trim().parse().ok());
        }

        request.extend(forwarded);
        next.run(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;
    use crate::router::Router;
    use crate::testing::TestClient;

    fn client(trust: TrustProxies) -> TestClient {
        let mut router = Router::new();
        router.middleware(trust);
        router.get("/", |req: Request| async move {
            Response::text(format!(
                "{}|{}|{}",
                req.ip().unwrap_or_else(|| "none".into()),
                req.scheme(),
                req.forwarded_host().unwrap_or("none")
            ))
        });
        TestClient::new(router)
    }

    /// A request as it arrives from `peer`, carrying a forwarded chain.
    fn from(peer: &str, chain: &str) -> Request {
        Request::new(Method::Get, "/")
            .with_peer(format!("{peer}:44321").parse().expect("an address"))
            .with_header("x-forwarded-for", chain)
    }

    #[tokio::test]
    async fn without_a_trust_list_the_header_is_ignored_entirely() {
        let response = client(TrustProxies::none()).send(from("203.0.113.9", "1.2.3.4")).await;
        assert_eq!(response.body(), "203.0.113.9|http|none", "the peer, not what it claimed");
    }

    #[tokio::test]
    async fn an_untrusted_peer_cannot_choose_its_own_address() {
        // The attack this exists to stop: a client sending a header to get a
        // rate limit bucket of its own on every request.
        let trust = TrustProxies::at(["10.0.0.0/8"]);
        let response = client(trust).send(from("203.0.113.9", "9.9.9.9")).await;
        assert_eq!(response.body(), "203.0.113.9|http|none");
    }

    #[tokio::test]
    async fn a_trusted_proxy_is_believed() {
        let trust = TrustProxies::at(["10.0.0.0/8"]);
        let response = client(trust).send(from("10.1.2.3", "203.0.113.9")).await;
        assert_eq!(response.body(), "203.0.113.9|http|none");
    }

    #[tokio::test]
    async fn trusted_hops_are_stripped_from_the_right() {
        // client, then two of our own proxies. Reading left to right would
        // take whatever the client put in the header before the first hop.
        let trust = TrustProxies::at(["10.0.0.0/8"]);
        let request = from("10.0.0.2", "203.0.113.9, 10.0.0.1, 10.0.0.2");
        assert_eq!(client(trust).send(request).await.body(), "203.0.113.9|http|none");
    }

    #[tokio::test]
    async fn a_spoofed_prefix_before_the_real_client_is_not_believed() {
        // The client sent "x-forwarded-for: 9.9.9.9"; the proxy appended the
        // address it actually saw. The right-most untrusted hop is the truth.
        let trust = TrustProxies::at(["10.0.0.0/8"]);
        let request = from("10.0.0.1", "9.9.9.9, 203.0.113.9");
        assert_eq!(client(trust).send(request).await.body(), "203.0.113.9|http|none");
    }

    #[tokio::test]
    async fn ports_are_stripped_from_forwarded_addresses() {
        let trust = TrustProxies::at(["10.0.0.0/8"]);
        let request = from("10.0.0.1", "203.0.113.9:51234");
        assert_eq!(client(trust).send(request).await.body(), "203.0.113.9|http|none");
    }

    #[tokio::test]
    async fn the_scheme_and_host_come_from_a_trusted_proxy_only() {
        let trust = TrustProxies::at(["10.0.0.0/8"]);
        let trusted = from("10.0.0.1", "203.0.113.9")
            .with_header("x-forwarded-proto", "https")
            .with_header("x-forwarded-host", "app.example.com");
        assert_eq!(client(trust.clone()).send(trusted).await.body(), "203.0.113.9|https|app.example.com");

        let spoofed = from("198.51.100.7", "1.2.3.4")
            .with_header("x-forwarded-proto", "https")
            .with_header("x-forwarded-host", "evil.example");
        assert_eq!(client(trust).send(spoofed).await.body(), "198.51.100.7|http|none");
    }

    #[tokio::test]
    async fn any_trusts_whoever_connected() {
        let response = client(TrustProxies::any()).send(from("203.0.113.9", "1.2.3.4")).await;
        assert_eq!(response.body(), "1.2.3.4|http|none");
    }

    #[test]
    fn cidr_matching_covers_both_families_and_odd_prefixes() {
        let ten = Trusted::parse("10.0.0.0/8").unwrap();
        assert!(ten.contains("10.255.255.255".parse().unwrap()));
        assert!(!ten.contains("11.0.0.1".parse().unwrap()));

        // A prefix that is not a whole number of bytes.
        let odd = Trusted::parse("192.168.4.0/22").unwrap();
        assert!(odd.contains("192.168.7.255".parse().unwrap()));
        assert!(!odd.contains("192.168.8.1".parse().unwrap()));

        let v6 = Trusted::parse("2001:db8::/32").unwrap();
        assert!(v6.contains("2001:db8:1234::1".parse().unwrap()));
        assert!(!v6.contains("2001:db9::1".parse().unwrap()));
        assert!(!v6.contains("10.0.0.1".parse().unwrap()), "families never match");

        assert_eq!(Trusted::parse("not an address"), None);
        assert_eq!(Trusted::parse("10.0.0.0/33"), None);
        assert_eq!(Trusted::parse("*"), Some(Trusted::Any));
    }

    #[test]
    fn a_typo_in_the_list_is_dropped_rather_than_widening_it() {
        let trust = TrustProxies::at(["10.0.0.0/8", "hello", ""]);
        assert_eq!(trust.proxies.len(), 1);
        assert!(!trust.trusts("203.0.113.9".parse().unwrap()));
    }

    #[test]
    fn from_config_reads_a_comma_separated_env_value() {
        let config = Config::new();
        config.set("trustedproxy.proxies", "10.0.0.0/8, 192.168.1.7");
        let trust = TrustProxies::from_config(&config);
        assert!(trust.trusts("10.9.9.9".parse().unwrap()));
        assert!(trust.trusts("192.168.1.7".parse().unwrap()));
        assert!(!trust.trusts("192.168.1.8".parse().unwrap()));

        assert!(!TrustProxies::from_config(&Config::new()).trusts("10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn ipv6_hops_keep_their_colons() {
        assert_eq!(strip_port("[2001:db8::1]:443"), "2001:db8::1");
        assert_eq!(strip_port("2001:db8::1"), "2001:db8::1");
        assert_eq!(strip_port("1.2.3.4:80"), "1.2.3.4");
        assert_eq!(strip_port("1.2.3.4"), "1.2.3.4");
    }
}
