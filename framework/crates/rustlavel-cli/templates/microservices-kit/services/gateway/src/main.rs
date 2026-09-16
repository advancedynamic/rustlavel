//! The gateway: one door in front of the services.
//!
//! It has **no database**, deliberately. The moment a gateway owns a table it
//! stops being a gateway and becomes a fourth service that can fail — and the
//! one every request already goes through. Redis is here for rate limiting,
//! which is state that may be lost without anybody being locked out.

use std::sync::Arc;

use rustlavel::prelude::*;
use rustlavel::discovery::{Balancer, Discovery};
use rustlavel::{Gateway, Upstream};

#[rustlavel::main]
async fn main() -> Result<()> {
    let app = App::new()?;

    let (auth, api) = upstreams();

    // A limiter in front of everything. Per client address, because at the
    // gateway there is not yet a user — that is the point of being in front.
    let cache = CacheStore::from_config(app.config())?;

    app.state(cache.clone())
        // Applied, not merely available. A limiter sitting in state that no
        // middleware reads is a limit nobody is under.
        //
        // **Keyed per client and per feature**, and the default would not do.
        // `Throttle`'s own key is the client address plus `request.route()` —
        // and at a gateway there are no registered routes, because everything
        // arrives through the fallback. `route()` is `None`, the path is used
        // instead, and `/api/1`, `/api/2`, `/api/3` each get a bucket of their
        // own: a limit that never fires. The custom key here is what makes the
        // limit exist at all.
        .middleware(Throttle::per_minute(&cache, 600).by(|request: &Request| {
            format!("{}|{}", client_of(request), feature_of(request.path()))
        }))
        .plugin(
            Gateway::new()
                // The authorization server, and `.open()` is what makes that
                // sentence true. `require_token()` below covers every route,
                // and without this exception `/oauth/token` answers `401` — a
                // door that only opens from the inside, because a token is what
                // you ask that endpoint for.
                .route("/oauth/*", auth.open())
                // Everything else needs a credential before it costs a hop.
                // The service still checks for itself — see api/src/main.rs.
                .route("/api/*", api)
                .require_token()
                // Asked of every upstream, and reported by name. A load
                // balancer reads the status; a person reads the body and
                // learns which service is unwell.
                .health("/health", "/health"),
        )
        .run()
        .await
}

/// Where `/oauth` and `/api` go.
///
/// **Two arrangements, one line of configuration apart.** With `DISCOVERY_URL`
/// set, each name is resolved for every request and balanced across whatever is
/// registered, so a new instance takes traffic as soon as it registers. Without
/// it, the two fixed addresses — which is the right answer on a platform that
/// already balances, and the right answer for one of each on a laptop.
///
/// A service that resolves to nothing is `503`, not a request sent to a host
/// nobody invented.
fn upstreams() -> (Upstream, Upstream) {
    let registry = rustlavel::env::env_or("DISCOVERY_URL", "");

    if registry.trim().is_empty() {
        return (
            Upstream::at(rustlavel::env::env_or("AUTH_URL", "http://127.0.0.1:9001")),
            Upstream::at(rustlavel::env::env_or("API_URL", "http://127.0.0.1:9002")),
        );
    }

    // One balancer, shared. Two would keep two caches of the same registry and
    // two round-robin counters, and the counters would drift apart.
    let balancer = Arc::new(Balancer::new(Discovery::new(vec![registry.trim().to_string()])));
    (
        Upstream::service("auth", balancer.clone()),
        Upstream::service("api", balancer),
    )
}

/// Who is being limited.
///
/// The bearer token when there is one, so a limit follows the client rather
/// than the address it happens to be calling from — behind a NAT or a mobile
/// carrier, thousands of unrelated callers share one address, and limiting by
/// address there means one heavy user locks out a thousand.
///
/// **Hashed, never the token itself.** The key reaches a cache, and a cache is
/// a place things get dumped, logged and inspected; a bearer token in one is a
/// bearer token in all three.
///
/// No token means no client to name, so the address is the honest fallback —
/// and an anonymous caller with no address at all shares one bucket rather than
/// escaping the limit. Failing closed is the only safe direction.
fn client_of(request: &Request) -> String {
    match request.header("authorization").and_then(|value| value.strip_prefix("Bearer ")) {
        Some(token) => rustlavel::auth::hashing::sha256_hex(token.as_bytes()),
        None => request.ip().unwrap_or_else(|| "anonymous".to_string()),
    }
}

/// Which feature is being limited.
///
/// The first path segment, which is the boundary this gateway routes on:
/// everything under `/api` is one feature, everything under `/oauth` another.
/// So a client exhausting its budget on the API can still reach the
/// authorization server to refresh a token — and a client hammering one feature
/// does not lose the others.
///
/// Deliberately not the whole path. Keying on that gives every distinct URL its
/// own bucket, which is the bug in the default this replaces.
fn feature_of(path: &str) -> &str {
    path.trim_start_matches('/').split('/').next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this key exists to avoid: a bucket per URL is no limit at all.
    #[test]
    fn every_path_under_a_feature_shares_one_bucket() {
        assert_eq!(feature_of("/api/orders/1"), "api");
        assert_eq!(feature_of("/api/orders/2"), "api");
        assert_eq!(feature_of("/api"), "api");
    }

    /// And exhausting one feature must not cost the others — a client out of
    /// budget on the API still has to be able to refresh its token.
    #[test]
    fn features_are_limited_separately() {
        assert_ne!(feature_of("/api/orders"), feature_of("/oauth/token"));
        assert_eq!(feature_of("/"), "");
    }
}
