//! The HTTP surface: five routes, and the pieces they share.
//!
//! Each endpoint module keeps its protocol logic in a plain function that takes
//! parsed parameters and returns a `Result`, with a thin handler on top that
//! turns that into a `Response`. The split is what lets the interesting cases —
//! a replayed code, a reused refresh token — be tested without a router in the
//! way, while [`crate::plugin`]'s tests still drive the whole thing through
//! `TestClient`.

pub mod authorize;
pub mod client_auth;
pub mod discovery;
pub mod introspect;
pub mod params;
pub mod revoke;
pub mod token;

use crate::server::{AuthorizationServer, DISCOVERY_PATH};
use rustlavel_http::Router;

/// Mount every endpoint. Called by [`crate::OAuthProvider`] and by
/// [`AuthorizationServer::install`].
pub fn register(server: &AuthorizationServer, router: &mut Router) {
    let mount = server.settings().mount.clone();

    // GET renders the consent screen; POST is that screen coming back. Same
    // path, so the form posts to where it came from and there is one URL in the
    // discovery document.
    //
    // These two are the only endpoints here a *browser* reaches, so they are
    // the only ones that carry `Csrf`. Without it the consent form is forgeable:
    // an attacker's page auto-submits "approve" in the victim's logged-in
    // browser and walks away with an authorization code for its own client.
    // Rendering the hidden `_token` field without checking it would be worse
    // than not rendering it, because the form would look protected.
    //
    // The rest of the mount — token, revoke, introspect — is machine-to-machine,
    // has no session and no cookie to ride on, and authenticates with client
    // credentials instead. CSRF there would reject every legitimate caller.
    let authorize = server.clone();
    let decide = server.clone();
    router.group(&mount, |browser_facing| {
        browser_facing.middleware(rustlavel_auth::Csrf::new());

        browser_facing
            .get("/authorize", move |request| {
                let server = authorize.clone();
                async move { authorize::show(server, request).await }
            })
            .name("oauth.authorize")
            .describe("Begin an authorization code flow");

        browser_facing
            .post("/authorize", move |request| {
                let server = decide.clone();
                async move { authorize::decide(server, request).await }
            })
            .name("oauth.authorize.decide")
            .describe("Record the user's decision on the consent screen");
    });

    let issue = server.clone();
    router
        .post(&format!("{mount}/token"), move |request| {
            let server = issue.clone();
            async move { token::issue(server, request).await }
        })
        .name("oauth.token")
        .describe("Exchange a grant for an access token");

    let revoking = server.clone();
    router
        .post(&format!("{mount}/revoke"), move |request| {
            let server = revoking.clone();
            async move { revoke::revoke(server, request).await }
        })
        .name("oauth.revoke")
        .describe("Revoke an access or refresh token");

    let introspecting = server.clone();
    router
        .post(&format!("{mount}/introspect"), move |request| {
            let server = introspecting.clone();
            async move { introspect::introspect(server, request).await }
        })
        .name("oauth.introspect")
        .describe("Ask whether a token is live, and what it allows");

    // RFC 8414 fixes this at the root of the issuer, not under the mount point.
    let discovering = server.clone();
    router
        .get(DISCOVERY_PATH, move |request| {
            let server = discovering.clone();
            async move { discovery::document(server, request).await }
        })
        .name("oauth.discovery")
        .describe("The authorization server metadata document");
}
