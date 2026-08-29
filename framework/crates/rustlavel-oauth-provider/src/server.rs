//! The authorisation server: the stores, the settings, and the few operations
//! that more than one endpoint needs.
//!
//! ```ignore
//! let server = AuthorizationServer::new(clients)
//!     .issued_by("https://accounts.example.com")
//!     .access_ttl(3600);
//!
//! App::new()?.plugin(OAuthProvider::new(server)).serve().await
//! ```
//!
//! Cloning is cheap and shares everything: each handler holds its own clone,
//! and they all see the same stores.

use crate::client::{Client, ClientStore, MemoryClientStore};
use crate::clock::Clock;
use crate::code::{self, CodeStore, MemoryCodeStore};
use crate::consent::{ConsentStore, MemoryConsentStore};
use crate::store::digest;
use crate::token::{
    self, AccessToken, MemoryTokenStore, RefreshToken, TokenStore, generate as generate_token,
};
use rustlavel_core::{Config, Result};
use rustlavel_http::Router;
use rustlavel_oauth::{OAuthError, Scopes, TokenResponse};
use std::sync::Arc;

/// Where the endpoints are mounted when nothing says otherwise.
pub const DEFAULT_MOUNT: &str = "/oauth";

/// RFC 8414 fixes this path exactly; it is not configurable.
pub const DISCOVERY_PATH: &str = "/.well-known/oauth-authorization-server";

/// Everything tunable, and what each knob costs.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The `iss` this server publishes, and the base of its discovery document.
    /// It must be the https origin clients actually reach.
    pub issuer: String,
    /// Path prefix for `/authorize`, `/token`, `/revoke` and `/introspect`.
    pub mount: String,
    /// Where an unauthenticated visitor is sent before they can consent.
    pub login_path: String,
    /// How long an authorisation code lives. Longer is not more convenient; it
    /// is more time for an intercepted code to be used.
    pub code_ttl: u64,
    /// How long a spent code is kept so a replay is still recognised.
    pub code_retention: u64,
    /// How long an access token lives.
    pub access_ttl: u64,
    /// How long a refresh token lives. This is the real session length for an
    /// application that keeps refreshing.
    pub refresh_ttl: u64,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            issuer: "http://localhost:8000".to_string(),
            mount: DEFAULT_MOUNT.to_string(),
            login_path: rustlavel_auth::guard::DEFAULT_LOGIN_PATH.to_string(),
            code_ttl: code::DEFAULT_TTL,
            code_retention: code::DEFAULT_RETENTION,
            access_ttl: token::DEFAULT_ACCESS_TTL,
            refresh_ttl: token::DEFAULT_REFRESH_TTL,
        }
    }
}

impl Settings {
    /// Read what an operator put in configuration.
    ///
    /// | Key                  | Meaning                                  |
    /// |----------------------|------------------------------------------|
    /// | `oauth.issuer`       | The published issuer. Falls back to `app.url`. |
    /// | `oauth.route`        | Where the endpoints mount (`/oauth`).    |
    /// | `oauth.login`        | Where to send a visitor who is not signed in. |
    /// | `oauth.code_ttl`     | Authorisation code lifetime, seconds (60). |
    /// | `oauth.access_ttl`   | Access token lifetime, seconds (3600).   |
    /// | `oauth.refresh_ttl`  | Refresh token lifetime, seconds (30 days). |
    pub fn from_config(config: &Config) -> Settings {
        let defaults = Settings::default();
        Settings {
            issuer: config.string(
                "oauth.issuer",
                &config.string("app.url", &defaults.issuer),
            ),
            mount: normalise_mount(config.string("oauth.route", &defaults.mount)),
            login_path: config.string("oauth.login", &defaults.login_path),
            code_ttl: positive(config.int("oauth.code_ttl", defaults.code_ttl as i64), defaults.code_ttl),
            code_retention: defaults.code_retention,
            access_ttl: positive(
                config.int("oauth.access_ttl", defaults.access_ttl as i64),
                defaults.access_ttl,
            ),
            refresh_ttl: positive(
                config.int("oauth.refresh_ttl", defaults.refresh_ttl as i64),
                defaults.refresh_ttl,
            ),
        }
    }

    /// The full path of one endpoint: `settings.endpoint("authorize")`.
    pub fn endpoint(&self, name: &str) -> String {
        format!("{}/{name}", self.mount)
    }

    /// The absolute URL of one endpoint, for the discovery document.
    pub fn endpoint_url(&self, name: &str) -> String {
        format!("{}{}", self.issuer.trim_end_matches('/'), self.endpoint(name))
    }
}

/// A lifetime of zero would issue tokens that are already expired, so a
/// nonsensical configured value falls back rather than taking the server down.
fn positive(configured: i64, fallback: u64) -> u64 {
    if configured > 0 { configured as u64 } else { fallback }
}

/// A mount point is a path prefix: leading slash, no trailing one.
fn normalise_mount(mount: String) -> String {
    let trimmed = mount.trim().trim_matches('/');
    if trimmed.is_empty() { DEFAULT_MOUNT.to_string() } else { format!("/{trimmed}") }
}

/// The OAuth 2.1 authorisation server.
#[derive(Clone)]
pub struct AuthorizationServer {
    clients: Arc<dyn ClientStore>,
    codes: Arc<dyn CodeStore>,
    tokens: Arc<dyn TokenStore>,
    consent: Arc<dyn ConsentStore>,
    settings: Settings,
    clock: Clock,
}

impl AuthorizationServer {
    /// A server over the given client registry, with everything else in memory.
    ///
    /// Swap each store out with [`AuthorizationServer::storing_codes`] and its
    /// siblings; the in-memory ones are for tests and development.
    pub fn new(clients: impl ClientStore) -> AuthorizationServer {
        AuthorizationServer {
            clients: Arc::new(clients),
            codes: Arc::new(MemoryCodeStore::new()),
            tokens: Arc::new(MemoryTokenStore::new()),
            consent: Arc::new(MemoryConsentStore::new()),
            settings: Settings::default(),
            clock: Clock::system(),
        }
    }

    /// A server with an empty in-memory registry, for a test that registers as
    /// it goes.
    pub fn in_memory() -> AuthorizationServer {
        AuthorizationServer::new(MemoryClientStore::new())
    }

    pub fn storing_codes(mut self, codes: impl CodeStore) -> AuthorizationServer {
        self.codes = Arc::new(codes);
        self
    }

    pub fn storing_tokens(mut self, tokens: impl TokenStore) -> AuthorizationServer {
        self.tokens = Arc::new(tokens);
        self
    }

    pub fn storing_consent(mut self, consent: impl ConsentStore) -> AuthorizationServer {
        self.consent = Arc::new(consent);
        self
    }

    /// Everything from configuration at once.
    pub fn configured(mut self, config: &Config) -> AuthorizationServer {
        self.settings = Settings::from_config(config);
        self
    }

    pub fn with_settings(mut self, settings: Settings) -> AuthorizationServer {
        self.settings = settings;
        self
    }

    /// The issuer identifier clients will see. Use the https origin they reach.
    pub fn issued_by(mut self, issuer: impl Into<String>) -> AuthorizationServer {
        self.settings.issuer = issuer.into();
        self
    }

    /// Mount the endpoints somewhere other than `/oauth`.
    pub fn at(mut self, mount: impl Into<String>) -> AuthorizationServer {
        self.settings.mount = normalise_mount(mount.into());
        self
    }

    /// Where to send a visitor who has not signed in yet.
    pub fn login_at(mut self, path: impl Into<String>) -> AuthorizationServer {
        self.settings.login_path = path.into();
        self
    }

    pub fn code_ttl(mut self, seconds: u64) -> AuthorizationServer {
        self.settings.code_ttl = seconds;
        self
    }

    pub fn access_ttl(mut self, seconds: u64) -> AuthorizationServer {
        self.settings.access_ttl = seconds;
        self
    }

    pub fn refresh_ttl(mut self, seconds: u64) -> AuthorizationServer {
        self.settings.refresh_ttl = seconds;
        self
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The server's clock. A test moves it forward to watch things expire.
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    pub fn now(&self) -> u64 {
        self.clock.now()
    }

    pub fn clients(&self) -> &dyn ClientStore {
        self.clients.as_ref()
    }

    pub fn codes(&self) -> &dyn CodeStore {
        self.codes.as_ref()
    }

    pub fn tokens(&self) -> &dyn TokenStore {
        self.tokens.as_ref()
    }

    pub fn consent(&self) -> &dyn ConsentStore {
        self.consent.as_ref()
    }

    /// Mount every endpoint onto a router.
    ///
    /// Applications go through [`crate::OAuthProvider`]; this is for tests and
    /// for an application that builds its own router.
    pub fn install(&self, router: &mut Router) {
        crate::endpoints::register(self, router);
    }

    // --- Operations more than one endpoint needs. ---

    /// Look up a client, treating a store failure as "no such client".
    ///
    /// A database that is down must not be a database that authorises
    /// everybody, and the caller has one honest answer to give either way.
    pub async fn client(&self, id: &str) -> Option<Client> {
        match self.clients.find(id).await {
            Ok(client) => client,
            Err(error) => {
                rustlavel_core::error!("oauth: the client store failed: {error}");
                None
            }
        }
    }

    /// Issue an access token — and, when asked, a refresh token — into a family.
    ///
    /// The plaintext tokens exist only in the [`TokenResponse`] this returns.
    /// What is stored is their digests.
    pub async fn issue(
        &self,
        client_id: &str,
        user_id: Option<&str>,
        scopes: &Scopes,
        family: &str,
        with_refresh: bool,
    ) -> Result<TokenResponse> {
        let now = self.clock.now();
        let plaintext = generate_token();

        let access = AccessToken::issued(&plaintext, now, self.settings.access_ttl)
            .for_client(client_id)
            .for_user(user_id)
            .granting(scopes.clone())
            .in_family(family);
        let access_id = access.id.clone();
        self.tokens.store_access(access).await?;

        let mut response = TokenResponse::bearer(plaintext)
            .expiring_in(self.settings.access_ttl)
            .with_scopes(scopes.clone());

        if with_refresh {
            let plaintext = generate_token();
            let refresh = RefreshToken::issued(&plaintext, now, self.settings.refresh_ttl)
                .for_client(client_id)
                .for_user(user_id)
                .granting(scopes.clone())
                .in_family(family)
                .alongside(access_id);
            self.tokens.store_refresh(refresh).await?;
            response = response.with_refresh_token(plaintext);
        }

        Ok(response)
    }

    /// Revoke every token descended from one authorisation, and say so in the
    /// log — a family revocation always means a credential leaked somewhere.
    pub async fn revoke_family(&self, family: &str, why: &str) {
        match self.tokens.revoke_family(family).await {
            Ok(revoked) => rustlavel_core::warn!(
                "oauth: revoked {revoked} token(s) in family {family}: {why}"
            ),
            Err(error) => {
                rustlavel_core::error!("oauth: could not revoke family {family}: {error}")
            }
        }
    }

    /// Resolve a presented bearer token, or say why it is not usable.
    ///
    /// Every failure is the same [`OAuthError`]: a resource server has nothing
    /// to do differently for "expired" than for "never existed", and telling
    /// the two apart is exactly what an attacker probing with guessed tokens
    /// would like to know.
    pub async fn validate_bearer(&self, presented: &str) -> std::result::Result<AccessToken, OAuthError> {
        let unusable = || {
            OAuthError::because(
                rustlavel_oauth::OAuthErrorCode::InvalidToken,
                "the access token is invalid, expired, or has been revoked",
            )
        };

        let token = self
            .tokens
            .find_access(&digest(presented))
            .await
            .map_err(|error| {
                rustlavel_core::error!("oauth: the token store failed: {error}");
                unusable()
            })?
            .ok_or_else(unusable)?;

        if token.is_live(self.clock.now()) { Ok(token) } else { Err(unusable()) }
    }

    /// Drop spent codes and dead tokens. Safe to call on a schedule.
    pub async fn purge(&self) -> Result<usize> {
        let now = self.clock.now();
        Ok(self.codes.purge(now).await? + self.tokens.purge(now).await?)
    }
}

impl std::fmt::Debug for AuthorizationServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizationServer")
            .field("settings", &self.settings)
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;

    fn server() -> AuthorizationServer {
        AuthorizationServer::new(
            MemoryClientStore::new().with(Client::public("spa").redirect_uri("https://a.test/cb")),
        )
    }

    #[test]
    fn a_mount_point_is_normalised_into_a_path_prefix() {
        assert_eq!(server().at("oauth2/").settings().mount, "/oauth2");
        assert_eq!(server().at("  ").settings().mount, DEFAULT_MOUNT);
        assert_eq!(server().settings().endpoint("token"), "/oauth/token");
    }

    #[test]
    fn endpoint_urls_are_absolute_and_do_not_double_their_slash() {
        let server = server().issued_by("https://accounts.test/");
        assert_eq!(server.settings().endpoint_url("token"), "https://accounts.test/oauth/token");
    }

    #[test]
    fn configuration_supplies_the_issuer_mount_and_lifetimes() {
        let config = Config::new();
        config.set("oauth.issuer", "https://accounts.test");
        config.set("oauth.route", "/id");
        config.set("oauth.access_ttl", 900);
        let settings = Settings::from_config(&config);

        assert_eq!(settings.issuer, "https://accounts.test");
        assert_eq!(settings.mount, "/id");
        assert_eq!(settings.access_ttl, 900);
        assert_eq!(settings.refresh_ttl, token::DEFAULT_REFRESH_TTL, "unset keys keep defaults");
    }

    #[test]
    fn the_issuer_falls_back_to_the_application_url() {
        let config = Config::new();
        config.set("app.url", "https://app.test");

        assert_eq!(Settings::from_config(&config).issuer, "https://app.test");
    }

    #[test]
    fn a_nonsense_lifetime_falls_back_rather_than_issuing_dead_tokens() {
        let config = Config::new();
        config.set("oauth.access_ttl", 0);
        config.set("oauth.code_ttl", -1);
        let settings = Settings::from_config(&config);

        assert_eq!(settings.access_ttl, token::DEFAULT_ACCESS_TTL);
        assert_eq!(settings.code_ttl, code::DEFAULT_TTL);
    }

    #[tokio::test]
    async fn issuing_returns_the_plaintext_and_stores_only_a_digest() {
        let server = server();
        let issued = server
            .issue("spa", Some("7"), &Scopes::of(["read"]), "family-1", true)
            .await
            .expect("issued");

        let access = issued.access_token.clone();
        assert_eq!(access.len(), 43);
        assert!(issued.refresh_token.is_some());
        assert_eq!(issued.expires_in, Some(token::DEFAULT_ACCESS_TTL));

        // The token resolves, and the stored record is not the token.
        let record = server.validate_bearer(&access).await.expect("live");
        assert_eq!(record.user_id.as_deref(), Some("7"));
        assert_ne!(record.hash(), access);
    }

    #[tokio::test]
    async fn client_credentials_get_no_refresh_token() {
        // RFC 6749 §4.4.3: there is no user to stay signed in, and the client
        // can simply ask again with the secret it already has.
        let server = server();
        let issued = server
            .issue("spa", None, &Scopes::of(["read"]), "family-1", false)
            .await
            .expect("issued");

        assert!(issued.refresh_token.is_none());
    }

    #[tokio::test]
    async fn a_bearer_token_stops_working_when_it_expires_or_is_revoked() {
        let server = server().access_ttl(60);
        let issued = server
            .issue("spa", Some("7"), &Scopes::of(["read"]), "family-1", true)
            .await
            .expect("issued");

        server.clock().advance(60);
        let error = server.validate_bearer(&issued.access_token).await.unwrap_err();
        assert_eq!(error.code, rustlavel_oauth::OAuthErrorCode::InvalidToken);
        // And the message says nothing about which of the reasons applied.
        assert!(error.to_string().contains("invalid, expired, or has been revoked"));
    }

    #[tokio::test]
    async fn revoking_a_family_kills_every_token_in_it() {
        let server = server();
        let issued = server
            .issue("spa", Some("7"), &Scopes::of(["read"]), "family-1", true)
            .await
            .expect("issued");

        server.revoke_family("family-1", "test").await;

        assert!(server.validate_bearer(&issued.access_token).await.is_err());
    }

    #[tokio::test]
    async fn an_unknown_bearer_token_is_refused_the_same_way() {
        let error = server().validate_bearer("nope").await.unwrap_err();
        assert_eq!(error.code, rustlavel_oauth::OAuthErrorCode::InvalidToken);
    }

    #[test]
    fn debug_shows_the_settings_and_no_store_contents() {
        let printed = format!("{:?}", server());

        assert!(printed.contains("settings"));
        assert!(!printed.contains("spa"));
    }
}
