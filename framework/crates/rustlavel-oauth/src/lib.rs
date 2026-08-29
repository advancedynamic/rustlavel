//! OAuth 2.1 for Rustlavel — the protocol vocabulary, and the client half.
//!
//! This crate is what you reach for when your application signs users in
//! *through* somebody else: Google, GitHub, or any provider that speaks the
//! authorisation code flow. The other half — being a provider yourself — lives
//! in `rustlavel-oauth-provider`, which depends on the types defined here so
//! both ends agree on the wire format by construction.
//!
//! ```ignore
//! use rustlavel_oauth::prelude::*;
//!
//! let github = OAuthClient::new(Provider::github())
//!     .credentials(config.string("services.github.id", ""), config.string("services.github.secret", ""))
//!     .redirect_uri("https://app.test/auth/github/callback");
//!
//! App::new().plugin(Socialite::new().provider(github).on_login(|user, req| async move {
//!     req.auth().login(&user);
//!     Response::see_other("/dashboard")
//! }));
//! ```
//!
//! Two things are not configurable, because a client that can be talked out of
//! them is a client that will be: PKCE is always sent, always S256, and every
//! callback is checked against a `state` this application issued. Skipping the
//! second is login CSRF — see [`state`] for what that costs.
//!
//! Written from scratch on RFC 6749, 6750, 7009 and 7636, save for SHA-256 and
//! AES-GCM, which come from the RustCrypto crates by way of `rustlavel-auth`.

pub mod client;
pub mod error;
pub mod pkce;
pub mod provider;
pub mod routes;
pub mod scope;
pub mod state;
pub mod token;
pub mod url;
pub mod user;

pub use client::{Authorization, OAuthClient};
pub use error::{OAuthError, OAuthErrorCode};
pub use pkce::{ChallengeMethod, Pkce};
pub use provider::{ClientAuth, Provider, UserMap};
pub use routes::Socialite;
pub use scope::Scopes;
pub use state::{SealedState, SessionState, StateError, StateGuard};
pub use token::TokenResponse;
pub use user::SocialUser;

pub use rustlavel_core::{Error, Result};

/// What a `routes/web.rs` or a controller imports to use this package.
pub mod prelude {
    pub use crate::{
        Authorization, ClientAuth, OAuthClient, OAuthError, OAuthErrorCode, Pkce, Provider,
        Scopes, SocialUser, Socialite, StateError, StateGuard, TokenResponse, UserMap,
    };
}
