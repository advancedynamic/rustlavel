//! rustlavel-oauth-provider: be an OAuth 2.1 authorization server.
//!
//! This is Laravel Passport's job — letting *other* applications sign users in
//! through yours. The mirror half, signing your users in through somebody
//! else's, is `rustlavel-oauth`, whose types this crate speaks so both ends
//! agree on the wire format by construction.
//!
//! ```ignore
//! use rustlavel_oauth_provider::prelude::*;
//!
//! let clients = MemoryClientStore::new().with(
//!     Client::confidential("checkout", &secret)
//!         .named("Checkout")
//!         .redirect_uri("https://checkout.test/oauth/callback")
//!         .scopes(Scopes::of(["orders.read", "orders.write"])),
//! );
//!
//! App::new()?
//!     .plugin(OAuthProvider::new(
//!         AuthorizationServer::new(clients).issued_by("https://accounts.example.com"),
//!     ))
//!     .serve()
//!     .await
//! ```
//!
//! That mounts six routes: `/oauth/authorize` (`GET` and `POST`),
//! `/oauth/token`, `/oauth/revoke`, `/oauth/introspect`, and the RFC 8414
//! discovery document at `/.well-known/oauth-authorization-server`. Guarding
//! your own API with the tokens it issues is [`RequireToken`].
//!
//! # What this server will not do
//!
//! It is OAuth **2.1**, not 2.0 with the dangerous parts left in:
//!
//! * **PKCE is required**, for every client, and only `S256` is accepted.
//!   `plain` is refused, and an absent `code_challenge_method` is an error
//!   rather than a default — RFC 7636 §4.3 defaults it to `plain`, which would
//!   let a client turn PKCE off by omitting a parameter.
//! * **The implicit flow is gone.** `response_type=token` returns an access
//!   token in a URL fragment, where it lands in browser history and in every
//!   script on the page.
//! * **The password grant is gone.** It asks the user to type their password
//!   into somebody else's application, cannot carry a second factor, and hands
//!   the client a credential that works everywhere.
//! * **Redirect URIs are matched byte for byte** against the registered set. No
//!   prefixes, no wildcards. [`redirect`] explains what each relaxation lets
//!   through.
//! * **A leaked credential takes its family down with it.** A replayed
//!   authorisation code and a reused refresh token both revoke every token
//!   descended from that authorisation, because either one means two parties
//!   hold a credential that was issued to one.
//!
//! # Where the data lives
//!
//! Four traits — [`ClientStore`], [`CodeStore`], [`TokenStore`],
//! [`ConsentStore`] — each with an in-memory implementation for tests and
//! development. There is deliberately **no dependency on `rustlavel-db`**; see
//! [`store`] for why an application backs these with its own tables.
//!
//! # Cryptography
//!
//! SHA-256, from `sha2`, is the only primitive here: every credential is stored
//! as a digest and compared in constant time, and PKCE's `S256` is the same
//! hash. Nothing else is invented. There is no JWT option and no JWKS: an
//! access token here is opaque so that revoking it revokes it, which a
//! self-contained signed token cannot offer.

pub mod client;
pub mod clock;
pub mod code;
pub mod consent;
pub mod endpoints;
pub mod page;
pub mod plugin;
pub mod redirect;
pub mod resource;
pub mod server;
pub mod store;
pub mod token;

pub use client::{Client, ClientStore, MemoryClientStore, generate_secret};
pub use clock::Clock;
pub use code::{AuthorizationCode, CodeStore, Consumption, MemoryCodeStore};
pub use consent::{ConsentStore, Grant, MemoryConsentStore};
pub use plugin::OAuthProvider;
pub use resource::{BearerExt, RequireToken, TokenClaims};
pub use server::{AuthorizationServer, Settings};
pub use store::StoreFuture;
pub use token::{AccessToken, MemoryTokenStore, RefreshToken, TokenStore};

/// The protocol vocabulary, re-exported so an application needs one import.
pub use rustlavel_oauth::{ChallengeMethod, OAuthError, OAuthErrorCode, Scopes, TokenResponse};

pub use rustlavel_core::{Error, Result};

/// What a `main.rs` or a controller imports to use this package.
pub mod prelude {
    pub use crate::{
        AuthorizationServer, BearerExt, Client, ClientStore, CodeStore, ConsentStore, Grant,
        MemoryClientStore, MemoryCodeStore, MemoryConsentStore, MemoryTokenStore, OAuthProvider,
        RequireToken, Scopes, Settings, TokenClaims, TokenStore, generate_secret,
    };
}
