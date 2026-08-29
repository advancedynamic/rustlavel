//! Proving which client is talking, at the token, revoke and introspect
//! endpoints.
//!
//! RFC 6749 §2.3 defines two ways a client presents its credentials, and this
//! server accepts both:
//!
//! * **HTTP Basic** (§2.3.1), which the specification says clients SHOULD use
//!   and servers MUST support. The id and secret are form-urlencoded *before*
//!   being base64'd — a detail that is easy to miss and produces a baffling
//!   `invalid_client` for any secret containing a `+` or a `/`.
//! * **Form body** `client_id` / `client_secret` (§2.3.1's second paragraph),
//!   which is what most libraries actually send.
//!
//! Presenting both is refused, not merged: §2.3 says a client "MUST NOT use
//! more than one authentication method in each request", and a request bearing
//! two sets of credentials is either a confused client or an attempt to have
//! one layer read one set and another read the other.
//!
//! A public client authenticates with nothing but its `client_id`. That is not
//! authentication and is not treated as such — what stands in for it is PKCE
//! and the registered redirect URI.

use crate::client::Client;
use crate::endpoints::params::Params;
use crate::server::AuthorizationServer;
use rustlavel_auth::base64;
use rustlavel_http::Request;
use rustlavel_oauth::{OAuthError, OAuthErrorCode, url};

/// What a request presented, before any of it is believed.
pub struct Credentials {
    pub id: String,
    pub secret: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("id", &self.id)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Pull credentials out of the `Authorization` header and the form body.
pub fn presented(request: &Request, params: &Params) -> Result<Credentials, OAuthError> {
    let basic = basic_auth(request)?;
    let body_id = params.get("client_id")?.map(str::to_string);
    let body_secret = params.get("client_secret")?.map(str::to_string);

    match (basic, body_id, body_secret) {
        (Some(_), _, Some(_)) => Err(OAuthError::because(
            OAuthErrorCode::InvalidClient,
            "credentials were sent both in the Authorization header and in the body. RFC 6749 \
             §2.3 allows exactly one method per request; send only the header.",
        )),
        // A `client_id` in the body alongside Basic is tolerated only when it
        // agrees: some libraries send it as a hint, and disagreeing copies are
        // the ambiguity the rule above exists to prevent.
        (Some(basic), Some(id), None) if basic.id != id => Err(OAuthError::because(
            OAuthErrorCode::InvalidClient,
            "the `client_id` in the body does not match the one in the Authorization header",
        )),
        (Some(basic), _, None) => Ok(basic),
        (None, Some(id), secret) => Ok(Credentials { id, secret }),
        (None, None, _) => Err(OAuthError::because(
            OAuthErrorCode::InvalidClient,
            "no client credentials were presented: send HTTP Basic, or `client_id` in the body",
        )),
    }
}

/// Decode `Authorization: Basic ...`, per RFC 6749 §2.3.1.
fn basic_auth(request: &Request) -> Result<Option<Credentials>, OAuthError> {
    let Some(header) = request.header("authorization") else { return Ok(None) };
    let Some(encoded) = header.strip_prefix("Basic ").or_else(|| header.strip_prefix("basic ")) else {
        return Ok(None);
    };

    let malformed = || {
        OAuthError::because(
            OAuthErrorCode::InvalidClient,
            "the Authorization header is not a readable HTTP Basic credential",
        )
    };

    let decoded = base64::decode(encoded.trim()).ok_or_else(malformed)?;
    let decoded = String::from_utf8(decoded).map_err(|_| malformed())?;
    let (id, secret) = decoded.split_once(':').ok_or_else(malformed)?;

    // §2.3.1: both halves are form-urlencoded before encoding, so a secret
    // containing `+` or `/` survives. Skipping this decode is why such a secret
    // mysteriously fails everywhere else.
    Ok(Some(Credentials { id: url::decode(id), secret: Some(url::decode(secret)) }))
}

thread_local! {
    /// A client nobody can authenticate as, used to spend the same time on an
    /// unknown `client_id` as on a wrong secret. Built once per thread from a
    /// fresh random secret, so nothing here is a credential.
    static DECOY: Client = Client::confidential("", &crate::client::generate_secret());
}

/// Which client this is, having checked what it presented.
///
/// Every failure is `invalid_client` with the same shape: whether the id was
/// unknown or the secret was wrong is not something a caller needs, and telling
/// them apart turns the endpoint into a client-id oracle.
pub async fn authenticate(
    server: &AuthorizationServer,
    credentials: &Credentials,
) -> Result<Client, OAuthError> {
    let refused = || {
        OAuthError::because(
            OAuthErrorCode::InvalidClient,
            "client authentication failed: unknown client, or the wrong secret",
        )
    };

    let Some(client) = server.client(&credentials.id).await else {
        // Verify against a decoy before giving up. Returning here without
        // hashing anything would make an unknown client measurably faster than
        // a wrong secret, which turns the endpoint into a client-id oracle for
        // anyone with a stopwatch — the messages being identical is not enough
        // if the timings are not.
        if let Some(secret) = &credentials.secret {
            DECOY.with(|decoy| decoy.verify_secret(secret));
        }
        return Err(refused());
    };

    match (&credentials.secret, client.is_confidential()) {
        (Some(secret), true) if client.verify_secret(secret) => Ok(client),
        (_, true) => Err(refused()),
        (None, false) => Ok(client),
        // A secret presented for a client that has none. Either the operator
        // registered the wrong kind of client or somebody is probing; both are
        // worth refusing rather than quietly ignoring the extra field.
        (Some(_), false) => Err(OAuthError::because(
            OAuthErrorCode::InvalidClient,
            "this client is registered as public and has no secret; do not send `client_secret`",
        )),
    }
}

/// The same, but insisting the client can actually prove who it is.
///
/// Used by revocation-adjacent endpoints where "authenticated" has to mean
/// something: a public client's only credential is its id, which is printed in
/// every redirect URL its users ever see.
pub async fn authenticate_confidential(
    server: &AuthorizationServer,
    credentials: &Credentials,
) -> Result<Client, OAuthError> {
    let client = authenticate(server, credentials).await?;

    if client.is_confidential() {
        Ok(client)
    } else {
        Err(OAuthError::because(
            OAuthErrorCode::InvalidClient,
            "this endpoint requires a client that can authenticate. A public client's only \
             credential is its id, which is public by construction.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Client, MemoryClientStore};
    use rustlavel_http::Method;

    fn basic_header(id: &str, secret: &str) -> String {
        let raw = format!("{}:{}", url::encode(id), url::encode(secret));
        format!("Basic {}", base64::encode(raw.as_bytes()))
    }

    fn read(request: &mut Request) -> Result<Credentials, OAuthError> {
        let params = Params::from_body(request);
        presented(request, &params)
    }

    fn server() -> AuthorizationServer {
        AuthorizationServer::new(
            MemoryClientStore::new()
                .with(Client::confidential("web", "s3cr+t/val=ue"))
                .with(Client::public("spa")),
        )
    }

    #[test]
    fn reads_http_basic_credentials() {
        let mut request = Request::new(Method::Post, "/token")
            .with_header("authorization", basic_header("web", "s3cret"))
            .with_form(&[("grant_type", "client_credentials")]);

        let credentials = read(&mut request).expect("read");
        assert_eq!(credentials.id, "web");
        assert_eq!(credentials.secret.as_deref(), Some("s3cret"));
    }

    #[test]
    fn a_basic_secret_containing_plus_or_slash_survives() {
        // The §2.3.1 detail everybody skips. Without the urlencode round trip,
        // this secret arrives mangled and the grant fails with a message that
        // points nowhere near the cause.
        let mut request = Request::new(Method::Post, "/token")
            .with_header("authorization", basic_header("web", "s3cr+t/val=ue"))
            .with_form(&[]);

        let credentials = read(&mut request).expect("read");
        assert_eq!(credentials.secret.as_deref(), Some("s3cr+t/val=ue"));
    }

    #[test]
    fn reads_form_body_credentials() {
        let mut request = Request::new(Method::Post, "/token")
            .with_form(&[("client_id", "web"), ("client_secret", "s3cret")]);

        let credentials = read(&mut request).expect("read");
        assert_eq!(credentials.id, "web");
        assert_eq!(credentials.secret.as_deref(), Some("s3cret"));
    }

    #[test]
    fn a_public_client_presents_only_its_id() {
        let mut request = Request::new(Method::Post, "/token").with_form(&[("client_id", "spa")]);

        let credentials = read(&mut request).expect("read");
        assert_eq!(credentials.id, "spa");
        assert!(credentials.secret.is_none());
    }

    #[test]
    fn presenting_two_sets_of_credentials_is_refused() {
        let mut request = Request::new(Method::Post, "/token")
            .with_header("authorization", basic_header("web", "s3cret"))
            .with_form(&[("client_id", "web"), ("client_secret", "other")]);

        let error = read(&mut request).unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::InvalidClient);
        assert!(error.to_string().contains("exactly one method"));
    }

    #[test]
    fn a_body_client_id_that_disagrees_with_the_header_is_refused() {
        let mut request = Request::new(Method::Post, "/token")
            .with_header("authorization", basic_header("web", "s3cret"))
            .with_form(&[("client_id", "spa")]);

        assert!(read(&mut request).is_err());
    }

    #[test]
    fn a_request_with_no_credentials_at_all_is_refused() {
        let mut request = Request::new(Method::Post, "/token").with_form(&[("token", "abc")]);

        let error = read(&mut request).unwrap_err();
        assert!(error.to_string().contains("no client credentials"));
    }

    #[test]
    fn a_malformed_basic_header_is_refused_rather_than_ignored() {
        let mut request = Request::new(Method::Post, "/token")
            .with_header("authorization", "Basic ????")
            .with_form(&[]);

        assert!(read(&mut request).is_err());
    }

    #[test]
    fn a_bearer_authorization_header_is_not_read_as_client_credentials() {
        // The revoke endpoint may be called with a bearer token in the header
        // by a confused client; it must fall through to the body, not be
        // half-decoded as Basic.
        let mut request = Request::new(Method::Post, "/revoke")
            .with_header("authorization", "Bearer abc")
            .with_form(&[("client_id", "spa")]);

        assert_eq!(read(&mut request).expect("read").id, "spa");
    }

    #[tokio::test]
    async fn a_confidential_client_authenticates_with_its_secret_and_not_without() {
        let server = server();

        let good = Credentials { id: "web".into(), secret: Some("s3cr+t/val=ue".into()) };
        assert!(authenticate(&server, &good).await.is_ok());

        let wrong = Credentials { id: "web".into(), secret: Some("nope".into()) };
        assert!(authenticate(&server, &wrong).await.is_err());

        let none = Credentials { id: "web".into(), secret: None };
        assert!(authenticate(&server, &none).await.is_err());
    }

    #[tokio::test]
    async fn an_unknown_client_fails_exactly_like_a_wrong_secret() {
        // Otherwise the endpoint answers "does this client id exist?" for
        // anyone who asks.
        let server = server();

        let unknown = authenticate(&server, &Credentials { id: "ghost".into(), secret: Some("x".into()) })
            .await
            .unwrap_err();
        let wrong = authenticate(&server, &Credentials { id: "web".into(), secret: Some("x".into()) })
            .await
            .unwrap_err();

        assert_eq!(unknown.code, wrong.code);
        assert_eq!(unknown.description, wrong.description);
    }

    #[tokio::test]
    async fn a_public_client_authenticates_by_id_alone_but_may_not_send_a_secret() {
        let server = server();

        assert!(authenticate(&server, &Credentials { id: "spa".into(), secret: None }).await.is_ok());

        let error =
            authenticate(&server, &Credentials { id: "spa".into(), secret: Some("x".into()) })
                .await
                .unwrap_err();
        assert!(error.to_string().contains("registered as public"));
    }

    #[tokio::test]
    async fn endpoints_that_need_real_authentication_refuse_a_public_client() {
        let server = server();

        let error =
            authenticate_confidential(&server, &Credentials { id: "spa".into(), secret: None })
                .await
                .unwrap_err();
        assert!(error.to_string().contains("public by construction"));

        let good = Credentials { id: "web".into(), secret: Some("s3cr+t/val=ue".into()) };
        assert!(authenticate_confidential(&server, &good).await.is_ok());
    }
}
