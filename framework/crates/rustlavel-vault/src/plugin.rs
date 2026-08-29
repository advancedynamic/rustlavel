//! Turning the secret store on.
//!
//! ```ignore
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     let vault = Vault::from_env()?.resolving("database").resolving("mail");
//!
//!     let app = App::new()?;
//!     // Before anything reads configuration: swap every `vault:` reference
//!     // for the secret it names, or refuse to start.
//!     vault.resolve(app.config()).await?;
//!
//!     app.plugin(vault).serve().await
//! }
//! ```
//!
//! Resolution is a step the application takes, not something the plugin does
//! behind its back, and that is deliberate. `Plugin::register` is synchronous —
//! reading a secret is not — and more importantly a boot that carried on
//! because a secret could not be fetched is the exact failure this package
//! exists to prevent. Written this way, the `?` is visible in `main`.
//!
//! What registering does do is put the client and the [`Renewer`] where the
//! rest of the application can reach them, and say so loudly when the store is
//! being talked to in a way that leaks the token.

use crate::client::VaultClient;
use crate::renew::Renewer;
use crate::resolve::{self, Resolver};
use rustlavel_core::{Config, Result};
use rustlavel_http::plugin::{Plugin, Setup};
use std::sync::Arc;

/// The environment variables Vault and OpenBao read to skip certificate
/// verification. Set, they turn TLS into an unauthenticated tunnel.
const SKIP_VERIFY: &[&str] = &["VAULT_SKIP_VERIFY", "BAO_SKIP_VERIFY"];

/// The secret store, as an application enables it.
///
/// | Config key       | Meaning                                                    |
/// |------------------|------------------------------------------------------------|
/// | `vault.resolve`  | Configuration roots to search for references, as an array. |
pub struct Vault {
    client: VaultClient,
    roots: Vec<String>,
    renewer: Arc<Renewer>,
    plain_http_is_fine: bool,
}

impl Vault {
    pub fn new(client: VaultClient) -> Vault {
        Vault {
            client,
            roots: Vec::new(),
            renewer: Arc::new(Renewer::new()),
            plain_http_is_fine: false,
        }
    }

    /// Take the address, token and namespace from the environment, the way
    /// every other Vault and OpenBao tool does.
    pub fn from_env() -> Result<Vault> {
        Ok(Vault::new(VaultClient::from_env()?))
    }

    /// Also search this configuration subtree for `vault:` references.
    ///
    /// Named rather than discovered because [`Config`] cannot be enumerated,
    /// and because a resolver that walked everything would read secrets for
    /// keys nobody asked it to touch.
    pub fn resolving(mut self, root: impl Into<String>) -> Vault {
        self.roots.push(root.into());
        self
    }

    /// Do not warn about a plain-HTTP address.
    ///
    /// Spelled out in full because of what it means: the token goes over the
    /// wire in clear text, and a token is the whole store. Loopback addresses
    /// never warn in the first place, so this is only for a network you have
    /// decided to trust.
    pub fn even_over_http(mut self) -> Vault {
        self.plain_http_is_fine = true;
        self
    }

    pub fn client(&self) -> &VaultClient {
        &self.client
    }

    /// The renewer this plugin registers.
    ///
    /// Whatever issues a lease — a login, a dynamic database credential — hands
    /// it here, and it is kept alive until the application stops.
    pub fn renewer(&self) -> &Arc<Renewer> {
        &self.renewer
    }

    /// Swap every `vault:` reference in configuration for the secret it names,
    /// and say how many were replaced.
    ///
    /// The roots come from [`Vault::resolving`], or from `vault.resolve` in
    /// configuration when the builder named none — a value written in
    /// `main.rs` is a decision and a value in a config file is a default.
    pub async fn resolve(&self, config: &Config) -> Result<usize> {
        let roots = if self.roots.is_empty() { configured_roots(config) } else { self.roots.clone() };
        if roots.is_empty() {
            return Ok(0);
        }

        let borrowed: Vec<&str> = roots.iter().map(String::as_str).collect();
        let resolved = Resolver::new(&self.client).config(config, &borrowed).await?;

        if resolved > 0 {
            rustlavel_core::info!(
                "resolved {resolved} configuration {} from the secret store",
                plural(resolved)
            );
        }
        Ok(resolved)
    }

    /// Swap every `vault:` reference in the process environment.
    ///
    /// For an application whose secrets arrive as `DATABASE_URL=vault:…`. Call
    /// it before the application boots, so `.env` and the configuration
    /// defaults see values rather than references.
    pub async fn resolve_env(&self) -> Result<usize> {
        let resolved = resolve::resolve_env(&self.client).await?;
        if resolved > 0 {
            rustlavel_core::info!(
                "resolved {resolved} environment {} from the secret store",
                plural(resolved)
            );
        }
        Ok(resolved)
    }

    /// Say so when the store is being reached in a way that gives the token
    /// away. Warnings, not refusals: refusing to register would leave the
    /// application with no secrets at all, which is a worse outcome than a
    /// loud log line somebody can act on.
    fn warn_about_unsafe_settings(&self, config: &Config) {
        if !self.plain_http_is_fine && sends_the_token_in_the_clear(self.client.address()) {
            rustlavel_core::warn!(
                "the secret store address is `{}`, which is plain HTTP over a network. The token \
                 travels in clear text and a token is the whole store — anyone who can read the \
                 traffic can read every secret it guards. Use https, or `.even_over_http()` if \
                 the address really is behind a TLS proxy on this host.",
                self.client.address()
            );
        }

        for name in SKIP_VERIFY {
            if std::env::var(name).is_ok_and(|value| truthy(&value)) {
                rustlavel_core::warn!(
                    "{name} is set, so the certificate the secret store presents is not checked. \
                     That makes the connection encrypted but not authenticated: anything that \
                     can intercept it can present its own certificate, take the token and answer \
                     with secrets of its own choosing."
                );
            }
        }

        if config.is_production() && !self.client.has_token() {
            rustlavel_core::warn!(
                "no token for the secret store. Every read will be refused until something \
                 authenticates — set VAULT_TOKEN, or log in before the first read."
            );
        }
    }
}

impl Plugin for Vault {
    fn name(&self) -> &'static str {
        "vault"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        self.warn_about_unsafe_settings(setup.config);

        // Both as state, so a handler resolves the client with
        // `req.state::<VaultClient>()` and whatever issues a lease can put it
        // on the renewer without threading either through the application.
        let renewer = self.renewer.clone();
        setup.state(self.client.clone());
        setup.state(renewer);
    }
}

/// Deliberately hand-written: the client's own `Debug` redacts the token, and
/// deriving here would be one refactor away from printing it.
impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("client", &self.client)
            .field("roots", &self.roots)
            .field("renewer", &self.renewer)
            .finish()
    }
}

fn configured_roots(config: &Config) -> Vec<String> {
    config
        .get("vault.resolve")
        .as_ref()
        .and_then(rustlavel_core::Json::as_array)
        .map(|roots| {
            roots.iter().filter_map(rustlavel_core::Json::as_str).map(str::to_string).collect()
        })
        .unwrap_or_default()
}

/// Whether an address means the token crosses a network unencrypted.
///
/// Loopback is exempt, and that is not a loophole: `http://127.0.0.1:8200` is
/// the standard shape of a Vault Agent or a TLS-terminating sidecar, the
/// traffic never leaves the host, and warning about it would train people to
/// ignore the warning that matters.
fn sends_the_token_in_the_clear(address: &str) -> bool {
    let Some(rest) = address.strip_prefix("http://") else { return false };
    let host = rest.split(['/', '?']).next().unwrap_or(rest);
    let host = host.rsplit_once(':').map_or(host, |(head, _)| head);

    !matches!(host.trim_matches(['[', ']']), "localhost" | "::1")
        && !host.starts_with("127.")
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "value" } else { "values" }
}

fn truthy(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::Fake;
    use rustlavel_core::{Context, ContextBuilder, Json};
    use rustlavel_http::Router;

    fn register(vault: Vault, config: &Config) -> Context {
        let mut router = Router::new();
        let mut context: Option<ContextBuilder> = Some(Context::builder().config(config.clone()));
        let mut setup = Setup { router: &mut router, config, context: &mut context };

        Box::new(vault).register(&mut setup);

        context.expect("context builder").build()
    }

    #[test]
    fn registers_the_client_and_the_renewer_so_handlers_can_reach_them() {
        let store = Fake::new().secret("secret/data/x", [("a", "1")]).store();
        let context = register(Vault::new(store.client().clone()), &Config::new());

        assert!(context.state::<VaultClient>().is_some(), "no client for a handler to use");
        assert!(context.state::<Arc<Renewer>>().is_some(), "nothing to hand a lease to");
    }

    #[test]
    fn a_plain_http_address_on_a_network_is_unsafe_and_loopback_is_not() {
        assert!(sends_the_token_in_the_clear("http://vault.internal:8200"));
        assert!(sends_the_token_in_the_clear("http://10.0.0.5:8200/"));
        assert!(!sends_the_token_in_the_clear("https://vault.internal:8200"));
        // The Vault Agent and TLS sidecar shape, which is safe and common.
        assert!(!sends_the_token_in_the_clear("http://127.0.0.1:8200"));
        assert!(!sends_the_token_in_the_clear("http://localhost:8200"));
        assert!(!sends_the_token_in_the_clear("http://[::1]:8200"));
    }

    #[test]
    fn a_skip_verify_setting_is_recognised_however_it_is_spelled() {
        assert!(truthy("1"));
        assert!(truthy("TRUE"));
        assert!(truthy(" yes "));
        assert!(!truthy("0"));
        assert!(!truthy(""));
    }

    #[tokio::test]
    async fn resolves_the_roots_it_was_told_about_and_leaves_the_rest() {
        let store = Fake::new()
            .secret("secret/data/myapp", [("password", "s3cr3t")])
            .secret("secret/data/other", [("password", "unused")])
            .store();

        let config = Config::new();
        config.set("database.password", "vault:secret/data/myapp#password");
        config.set("mail.password", "vault:secret/data/other#password");

        let vault = Vault::new(store.client().clone()).resolving("database");
        assert_eq!(vault.resolve(&config).await.unwrap(), 1);

        assert_eq!(config.string("database.password", ""), "s3cr3t");
        assert_eq!(config.string("mail.password", ""), "vault:secret/data/other#password");
        store.assert_not_read("secret/data/other");
    }

    #[tokio::test]
    async fn configuration_supplies_the_roots_when_the_builder_named_none() {
        let store = Fake::new().secret("secret/data/myapp", [("password", "s3cr3t")]).store();

        let config = Config::new();
        config.set("vault.resolve", Json::Array(vec![Json::from("database")]));
        config.set("database.password", "vault:secret/data/myapp#password");

        assert_eq!(Vault::new(store.client().clone()).resolve(&config).await.unwrap(), 1);
        assert_eq!(config.string("database.password", ""), "s3cr3t");
    }

    #[tokio::test]
    async fn resolving_nothing_reads_nothing() {
        let store = Fake::new().store();

        assert_eq!(Vault::new(store.client().clone()).resolve(&Config::new()).await.unwrap(), 0);
        store.assert_reads(0);
    }

    #[tokio::test]
    async fn an_unresolvable_reference_refuses_to_boot() {
        let store = Fake::new().denies("secret/data/myapp").store();
        let config = Config::new();
        config.set("database.password", "vault:secret/data/myapp#password");

        let error = Vault::new(store.client().clone())
            .resolving("database")
            .resolve(&config)
            .await
            .expect_err("a refused secret must stop the boot")
            .to_string();

        assert!(error.contains("database.password"), "got {error}");
        assert!(error.contains("secret/data/myapp"), "got {error}");
    }

    #[test]
    fn the_plugin_never_prints_the_token() {
        let store = Fake::new().store();
        let printed = format!("{:?}", Vault::new(store.client().clone()));

        assert!(!printed.contains("fake-token"), "got {printed}");
        assert!(printed.contains("redacted"));
    }
}
