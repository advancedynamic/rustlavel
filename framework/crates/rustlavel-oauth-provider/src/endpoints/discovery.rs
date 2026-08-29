//! `GET /.well-known/oauth-authorization-server` — RFC 8414.
//!
//! One document that says where the endpoints are and what this server will
//! agree to. It is small, and it turns "read our integration guide" into a
//! `GET`: a client library can find the token endpoint, discover that PKCE is
//! required and that only `S256` is accepted, and fail at configuration time
//! rather than at the end of a redirect.
//!
//! The path is fixed by §3 and is not configurable — the point of a well-known
//! URI is that it is well known. It sits at the root of the issuer, not under
//! the endpoints' mount point, for the same reason.
//!
//! What is advertised is what the code actually does. A discovery document that
//! promises a grant the server refuses is worse than none: a client will build
//! against it.

use crate::server::AuthorizationServer;
use rustlavel_core::Json;
use rustlavel_http::{Request, Response};

/// `GET /.well-known/oauth-authorization-server`.
pub async fn document(server: AuthorizationServer, _request: Request) -> Response {
    Response::json(metadata(&server))
        // Public and unchanging between deploys; an hour saves a round trip
        // per client boot without making a rotation slow to take effect.
        .with_header("cache-control", "public, max-age=3600")
}

pub fn metadata(server: &AuthorizationServer) -> Json {
    let settings = server.settings();
    let strings = |values: &[&str]| {
        Json::Array(values.iter().map(|value| Json::String((*value).to_string())).collect())
    };

    Json::object([
        ("issuer", Json::String(settings.issuer.clone())),
        ("authorization_endpoint", Json::String(settings.endpoint_url("authorize"))),
        ("token_endpoint", Json::String(settings.endpoint_url("token"))),
        ("revocation_endpoint", Json::String(settings.endpoint_url("revoke"))),
        ("introspection_endpoint", Json::String(settings.endpoint_url("introspect"))),
        ("response_types_supported", strings(&["code"])),
        (
            "grant_types_supported",
            strings(&["authorization_code", "refresh_token", "client_credentials"]),
        ),
        // S256 only, and advertised as such: a client that reads this cannot
        // discover `plain` and then be surprised at the authorization step.
        ("code_challenge_methods_supported", strings(&["S256"])),
        (
            "token_endpoint_auth_methods_supported",
            strings(&["client_secret_basic", "client_secret_post", "none"]),
        ),
        (
            "revocation_endpoint_auth_methods_supported",
            strings(&["client_secret_basic", "client_secret_post", "none"]),
        ),
        (
            "introspection_endpoint_auth_methods_supported",
            strings(&["client_secret_basic", "client_secret_post"]),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::MemoryClientStore;

    fn metadata_for(issuer: &str) -> Json {
        metadata(&AuthorizationServer::new(MemoryClientStore::new()).issued_by(issuer))
    }

    fn list(document: &Json, field: &str) -> Vec<String> {
        document
            .get(field)
            .and_then(Json::as_array)
            .expect("a list")
            .iter()
            .filter_map(Json::as_str)
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn every_endpoint_is_absolute_and_under_the_issuer() {
        let document = metadata_for("https://accounts.test");

        assert_eq!(document.get("issuer").and_then(Json::as_str), Some("https://accounts.test"));
        for field in [
            "authorization_endpoint",
            "token_endpoint",
            "revocation_endpoint",
            "introspection_endpoint",
        ] {
            let url = document.get(field).and_then(Json::as_str).expect(field);
            assert!(url.starts_with("https://accounts.test/oauth/"), "{field} was {url}");
        }
    }

    #[test]
    fn a_custom_mount_point_moves_every_advertised_endpoint() {
        let server = AuthorizationServer::new(MemoryClientStore::new())
            .issued_by("https://accounts.test")
            .at("/id");
        let document = metadata(&server);

        assert_eq!(
            document.get("token_endpoint").and_then(Json::as_str),
            Some("https://accounts.test/id/token")
        );
    }

    #[test]
    fn it_advertises_only_what_the_server_actually_accepts() {
        // A document promising a grant the server refuses is worse than none:
        // a client will build against it.
        let document = metadata_for("https://accounts.test");

        assert_eq!(list(&document, "response_types_supported"), ["code"]);
        assert_eq!(list(&document, "code_challenge_methods_supported"), ["S256"]);

        let grants = list(&document, "grant_types_supported");
        assert!(grants.contains(&"authorization_code".to_string()));
        assert!(grants.contains(&"refresh_token".to_string()));
        assert!(!grants.contains(&"password".to_string()));
        assert!(!grants.contains(&"implicit".to_string()));
    }

    #[test]
    fn introspection_does_not_advertise_unauthenticated_access() {
        // `none` appears for the token endpoint, where a public client is
        // legitimate, and must not appear here.
        let document = metadata_for("https://accounts.test");

        assert!(list(&document, "token_endpoint_auth_methods_supported").contains(&"none".into()));
        assert!(
            !list(&document, "introspection_endpoint_auth_methods_supported")
                .contains(&"none".into())
        );
    }
}
