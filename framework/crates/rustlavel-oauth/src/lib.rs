//! OAuth 2.1 for Rustlavel — the protocol vocabulary, and the client half.
//!
//! This crate is what you reach for when your application signs users in
//! *through* somebody else: Google, GitHub, or any provider that speaks the
//! authorisation code flow. The other half — being a provider yourself — lives
//! in `rustlavel-oauth-provider`, which depends on the types defined here so
//! both ends agree on the wire format by construction.
//!
//! Written from scratch on RFC 6749, 6750, 7636 and 7009, save for SHA-256,
//! which comes from `sha2` by way of `rustlavel-auth`.

pub mod error;
pub mod pkce;
pub mod scope;
pub mod token;
pub mod url;

pub use error::{OAuthError, OAuthErrorCode};
pub use pkce::{ChallengeMethod, Pkce};
pub use scope::Scopes;
pub use token::TokenResponse;

pub use rustlavel_core::{Error, Result};
