//! How this service decides a token is good — and the fact that it is a
//! deployment decision rather than a code one.
//!
//! Two ways, chosen by `TOKEN_VERIFICATION`:
//!
//! `introspect` asks the authorization server (RFC 7662) and caches the answer
//! for a short time. Always current — a token revoked a second ago is refused a
//! second ago, once the cache expires — at the cost of a hop and of making the
//! authorization server something every request depends on.
//!
//! `signed` verifies an ES256 signature against a public key this service
//! already holds. No hop, no dependency, and revocation becomes hard: a token
//! handed out is good until it expires whatever anybody decides in between.
//!
//! Both produce the same [`Caller`], which is what lets every handler be written
//! once. And both are worth testing equally: **a security path that is rarely
//! used is the one that is wrong without anybody knowing.**

use rustlavel::prelude::*;
use rustlavel::oauth::verify::{self, Claims, INTROSPECTION_TTL};

use {{crate_name}}_shared::{Caller, problem};

/// Which way this deployment checks a token.
#[derive(Clone)]
pub enum Verifier {
    /// Ask the authorization server, and cache what it says.
    Introspect { url: String, client_id: String, secret: String },
    /// Check the signature here, against a public key.
    Signed { public_key: Vec<u8> },
}

impl Verifier {
    /// Read the choice from the environment.
    ///
    /// Introspection is the default because it needs no key distribution and a
    /// revoked token stops working within `INTROSPECTION_TTL` — thirty seconds,
    /// not immediately. The answer is cached, and a cached answer outlives the
    /// moment it was true; the alternative is a request to the authorization
    /// server on every single call.
    ///
    /// `signed` is the one you choose deliberately, having decided you can live
    /// with revocation taking until the token expires — an hour, not thirty
    /// seconds. That is the trade, and it is worth stating in those terms
    /// rather than as "immediate" against "eventual".
    pub fn from_env() -> Verifier {
        match rustlavel::env::env_or("TOKEN_VERIFICATION", "introspect").as_str() {
            "signed" => Verifier::Signed {
                public_key: rustlavel::auth::base64::decode(
                    &rustlavel::env::env_or("TOKEN_PUBLIC_KEY", ""),
                )
                .unwrap_or_default(),
            },
            _ => Verifier::Introspect {
                url: rustlavel::env::env_or("AUTH_URL", "http://127.0.0.1:9001"),
                client_id: rustlavel::env::env_or("INTROSPECT_CLIENT_ID", ""),
                secret: rustlavel::env::env_or("INTROSPECT_CLIENT_SECRET", ""),
            },
        }
    }

    /// The claims a token carries, or nothing.
    pub async fn claims(&self, req: &Request, token: &str) -> Option<Claims> {
        let now = rustlavel::auth::unix_now() as i64;

        match self {
            Verifier::Signed { public_key } => verify::verify_signed(token, public_key, now).ok(),

            Verifier::Introspect { url, client_id, secret } => {
                // The cache key is a hash, never the token. A cache is a place
                // things get dumped, logged and inspected, and a bearer token
                // in one is a bearer token in all three.
                let key = format!("introspect:{}", rustlavel::auth::hashing::sha256_hex(token.as_bytes()));

                if let Some(cache) = req.state::<CacheStore>()
                    && let Ok(Some(cached)) = cache.get(&key).await
                    && let Ok(claims) = verify::read_introspection(&cached, now)
                {
                    return Some(claims);
                }

                // Written out rather than reached for: the client has no
                // `basic_auth` or `form` helper, and RFC 7662 asks for exactly
                // these two things — HTTP basic on the client credentials, and
                // the token in a urlencoded body.
                let credentials = rustlavel::auth::base64::encode(
                    format!("{client_id}:{secret}").as_bytes(),
                );
                let response = match Client::new()
                    .post(format!("{url}/oauth/introspect"))
                    .header("authorization", format!("Basic {credentials}"))
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(format!("token={}", rustlavel::url::encode(token)))
                    .send()
                    .await
                {
                    Ok(response) => response,
                    // **Said out loud.** Every one of these used to be `.ok()?`,
                    // so a wrong `AUTH_URL`, an authorization server that was
                    // down, and wrong introspection credentials all produced the
                    // same bare `401` with nothing in the log. An operator
                    // cannot tell a forged token from a typo in a settings file,
                    // and the answer is identical in both cases. Found by
                    // pointing `AUTH_URL` at the wrong port.
                    Err(error) => {
                        error!("token check: cannot reach {url}/oauth/introspect: {error}");
                        return None;
                    }
                };

                // A 401 here is *this service's* credentials being refused, not
                // the caller's — a distinction worth the extra branch, because
                // the two are fixed in different files.
                if !response.is_success() {
                    error!(
                        "token check: {url} refused this service's introspection credentials \
                         (HTTP {}). Check INTROSPECT_CLIENT_ID and INTROSPECT_CLIENT_SECRET.",
                        response.status
                    );
                    return None;
                }

                let body = match response.json() {
                    Ok(body) => body,
                    Err(error) => {
                        error!("token check: {url} sent something unreadable: {error}");
                        return None;
                    }
                };

                // The only branch that is about the caller: the token really
                // was refused. Debug rather than error — an expired token is
                // ordinary traffic, not an incident.
                let claims = match verify::read_introspection(&body, now) {
                    Ok(claims) => claims,
                    Err(_) => {
                        debug!("token check: the token was refused");
                        return None;
                    }
                };

                if let Some(cache) = req.state::<CacheStore>() {
                    let _ = cache.put(&key, body, INTROSPECTION_TTL).await;
                }
                Some(claims)
            }
        }
    }
}

/// Refuse anything without a good token, and hand the rest a [`Caller`].
///
/// Skips the health endpoint: a load balancer has no token, and a service that
/// answers "unauthenticated" to a health probe is a service that will be taken
/// out of rotation for being healthy.
pub struct RequireCaller;

impl Middleware for RequireCaller {
    fn handle(&self, mut request: Request, next: Next) -> BoxFuture<Response> {
        Box::pin(async move {
            if request.path() == "/health" {
                return next.run(request).await;
            }

            let presented = request
                .header("authorization")
                .and_then(|value| value.strip_prefix("Bearer ").map(str::to_string));

            let Some(token) = presented else {
                return Response::new(Status::UNAUTHORIZED).with_json(problem("Unauthenticated."));
            };

            let verifier = match request.state::<Verifier>() {
                Some(verifier) => verifier.clone(),
                None => {
                    error!("no Verifier in state; every request will be refused");
                    return Response::new(Status::INTERNAL_ERROR);
                }
            };

            let Some(claims) = verifier.claims(&request, &token).await else {
                return Response::new(Status::UNAUTHORIZED).with_json(problem("Unauthenticated."));
            };

            request.extend(Caller {
                // `None` for a service, and the resource server is written to
                // expect that: see `Caller::owner`.
                subject: claims.subject,
                client: claims.client,
                scopes: claims.scopes.iter().map(str::to_string).collect(),
            });
            next.run(request).await
        })
    }
}
