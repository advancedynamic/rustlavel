//! Taking a request in, sending it on, and handing the answer back.
//!
//! The part nothing else in the framework does. Most of the care here is not in
//! the sending — the client already retries, times out and breaks a circuit —
//! but in what crosses and what does not.
//!
//! **Hop-by-hop headers are dropped in both directions.** `Connection`,
//! `Transfer-Encoding` and their relatives describe *this* connection, not the
//! message, and forwarding one makes the gateway and its upstream disagree
//! about where the message ends. That disagreement is request smuggling with an
//! extra hop, and this crate refusing to pass them on is the same defence the
//! server makes when it refuses to parse them.
//!
//! **`X-Forwarded-*` is set, and the client's own is not trusted.** The gateway
//! knows who opened the socket; a header the client sent claiming otherwise is
//! a claim, and appending to it would let anybody write whatever address they
//! liked into the upstream's logs and rate limiter.

use rustlavel_client::Client;
use rustlavel_core::Result;
use rustlavel_http::{Headers, Method, Request, Response, Status};

use crate::route::{Upstream, is_forwardable};

/// Send a request upstream and turn the answer into a response.
///
/// A failure to reach the upstream is `502 Bad Gateway` rather than a `500`:
/// the gateway is working, and something behind it is not. The distinction is
/// the difference between paging the team that owns this and the team that owns
/// that.
pub async fn forward(client: &Client, upstream: &Upstream, request: Request) -> Result<Response> {
    let url = upstream.url_for(request.target());
    let method = request.method();
    let peer = request.ip();

    let mut outbound = client.request(method, url);

    for (name, value) in request.headers().iter() {
        if is_forwardable(name) {
            outbound = outbound.header(name, value.to_string());
        }
    }

    // The upstream's own name, when it needs one. A CDN or a virtual host
    // routes on `Host`, so passing the gateway's through would reach the wrong
    // site.
    if let Some(host) = upstream.forwarded_host() {
        outbound = outbound.header("host", host.to_string());
    }

    // Set, not appended to. Whatever the client sent under this name is a
    // claim about itself; the address that opened the socket is a fact.
    if let Some(address) = peer {
        outbound = outbound.header("x-forwarded-for", address);
    }
    outbound = outbound.header("x-forwarded-proto", request.scheme());

    let body = request.body().to_vec();
    if !body.is_empty() {
        outbound = outbound.body(body);
    }

    match outbound.send().await {
        Ok(upstream_response) => {
            // Whatever the upstream said, unchanged. A gateway that
            // normalises status codes hides what the service actually
            // answered, and the caller then debugs the wrong program.
            let mut response = Response::new(upstream_response.status)
                .with_body(upstream_response.body.clone());

            for (name, value) in upstream_response.headers.iter() {
                if is_forwardable(name) {
                    response = response.with_header(name, value.to_string());
                }
            }
            Ok(response)
        }
        // The upstream is unreachable, timed out, or its circuit is open.
        // Deliberately without the underlying message: an error from inside the
        // network is not the client's to read, and it names hosts and ports
        // they have no business knowing.
        Err(error) => {
            rustlavel_core::warn!("gateway: {} is not answering: {error}", upstream.base());
            Ok(Response::new(Status(502))
                .with_json(rustlavel_core::Json::object([(
                    "message",
                    rustlavel_core::Json::from("The service behind this endpoint is not responding."),
                )])))
        }
    }
}

/// The headers a response carries back, for a test to look at.
pub fn forwardable(headers: &Headers) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| is_forwardable(name))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// Whether this method may carry a body upstream.
///
/// Not a rule the gateway invents: a `GET` with a body is legal to parse and
/// undefined in meaning, and forwarding one is a reliable way to find out that
/// two servers disagree about it.
pub fn carries_body(method: Method) -> bool {
    matches!(method, Method::Post | Method::Put | Method::Patch | Method::Delete)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_headers_are_dropped_in_both_directions() {
        let mut headers = Headers::new();
        headers.set("authorization", "Bearer abc");
        headers.set("connection", "close");
        headers.set("transfer-encoding", "chunked");
        headers.set("content-type", "application/json");

        let crossing: Vec<String> =
            forwardable(&headers).into_iter().map(|(name, _)| name).collect();

        assert!(crossing.contains(&"authorization".to_string()));
        assert!(crossing.contains(&"content-type".to_string()));
        assert!(
            !crossing.contains(&"connection".to_string()),
            "a connection header crossed the gateway, which is smuggling with an extra hop"
        );
        assert!(!crossing.contains(&"transfer-encoding".to_string()));
    }

    #[test]
    fn only_methods_that_define_a_body_carry_one() {
        assert!(carries_body(Method::Post));
        assert!(carries_body(Method::Put));
        assert!(!carries_body(Method::Get));
        assert!(!carries_body(Method::Head));
    }
}
