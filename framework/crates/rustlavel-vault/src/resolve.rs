//! Secrets that arrive as configuration.
//!
//! The point of the package in one line of `.env`:
//!
//! ```text
//! DATABASE_URL=vault:secret/data/myapp#database_url
//! ```
//!
//! At boot that reference is swapped for the value stored at
//! `secret/data/myapp` under the field `database_url`, and nothing but the
//! reference was ever written down. Every distinct path is fetched once however
//! many fields are taken from it, because a secret with a username, a password
//! and a host is one secret and should cost one round trip.
//!
//! **A reference that cannot be resolved is a hard error at boot.** Not an
//! empty string, not a warning. An empty database password does not fail where
//! it was configured; it fails minutes later inside a connection pool, with a
//! message about authentication that names neither the secret nor the key, and
//! somebody spends an afternoon on it. Failing at boot costs one restart and
//! names both.
//!
//! For the same reason there is **no fallback syntax**. `vault:path#field|value`
//! would be convenient and is exactly the feature that turns a store outage
//! into a process running on the wrong credentials — the syntax cannot tell a
//! harmless default apart from a password, so it is refused, with an error that
//! explains why rather than a field name nobody stored.

use crate::client::{VaultClient, VaultResponse};
use crate::lease::Lease;
use rustlavel_core::{Config, Error, Json, Result};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

/// What marks a configuration value as a reference rather than a value.
pub const PREFIX: &str = "vault:";

/// A boxed future, so [`SecretSource`] can be used through a trait object.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A `vault:` reference: which secret, and which field inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    pub path: String,
    pub field: String,
}

impl SecretRef {
    /// Whether a value is meant as a reference at all.
    ///
    /// Only an exact `vault:` prefix counts, which is what keeps an ordinary
    /// value that merely contains a colon — `postgres://db:5432/app`,
    /// `redis://cache:6379` — from being mistaken for one.
    pub fn is_reference(value: &str) -> bool {
        value.starts_with(PREFIX)
    }

    /// Parse one, or say precisely what is wrong with it.
    pub fn parse(value: &str) -> Result<SecretRef> {
        let rest = value.strip_prefix(PREFIX).ok_or_else(|| {
            Error::msg(format!("`{value}` is not a secret reference; one starts with `{PREFIX}`"))
        })?;

        // Checked before the split so `vault:path|default` — no field at all —
        // gets the explanation too, rather than the generic "needs a field".
        if rest.contains('|') {
            return Err(Error::msg(format!(
                "`{value}` looks like it carries a fallback, and there is deliberately no \
                 fallback syntax. A default that stands in for a secret the store did not give \
                 up means booting on the wrong credential and failing later somewhere else; if \
                 the value is harmless enough to have a default, it is not a secret and belongs \
                 in configuration"
            )));
        }

        let (path, field) = rest.split_once('#').ok_or_else(|| {
            Error::msg(format!(
                "`{value}` names a path but no field. A reference reads \
                 `vault:secret/data/myapp#database_url` — the part after `#` is the field \
                 inside the secret"
            ))
        })?;

        let (path, field) = (path.trim(), field.trim());
        if path.is_empty() || field.is_empty() {
            return Err(Error::msg(format!(
                "`{value}` is not a complete secret reference: it needs a path and a field, as \
                 in `vault:secret/data/myapp#database_url`"
            )));
        }

        Ok(SecretRef { path: path.to_string(), field: field.to_string() })
    }
}

impl std::fmt::Display for SecretRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{PREFIX}{}#{}", self.path, self.field)
    }
}

/// The fields stored at one path.
///
/// No `#[derive(Debug)]`, and there will not be one: this is the value the
/// whole package exists to keep out of logs, and a derived `Debug` is how it
/// reaches one — through a `dbg!` left behind, a panic message, or a struct
/// somewhere upstream that derived `Debug` over a field holding this.
#[derive(Clone)]
pub struct Secret {
    path: String,
    fields: BTreeMap<String, String>,
    lease: Lease,
}

impl Secret {
    pub fn new(
        path: impl Into<String>,
        fields: BTreeMap<String, String>,
        lease: Lease,
    ) -> Secret {
        Secret { path: path.into(), fields, lease }
    }

    /// Unwrap whatever the store answered a read with.
    pub fn from_response(path: &str, response: &VaultResponse) -> Secret {
        Secret {
            path: path.to_string(),
            fields: fields_of(&response.data),
            lease: response.lease.clone(),
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }

    /// The field names, which are safe to print — and the single most useful
    /// thing to say when a reference names a field that is not there.
    pub fn names(&self) -> Vec<&str> {
        self.fields.keys().map(String::as_str).collect()
    }

    pub fn lease(&self) -> &Lease {
        &self.lease
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// Names, never values.
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret")
            .field("path", &self.path)
            .field("fields", &self.names())
            .field("values", &"<redacted>")
            .finish()
    }
}

/// Where a resolver gets its secrets.
///
/// One method on purpose. The KV engine, a fake, or an application's own
/// wrapper around something this crate has never heard of can all satisfy it,
/// and the resolver never learns which mount or which engine version it is
/// reading from.
pub trait SecretSource: Send + Sync {
    fn read<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Secret>>;
}

impl SecretSource for VaultClient {
    fn read<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Secret>> {
        Box::pin(async move { Ok(Secret::from_response(path, &self.get(path).await?)) })
    }
}

/// Resolves references, fetching each distinct path once.
pub struct Resolver<'a> {
    source: &'a dyn SecretSource,
    fetched: BTreeMap<String, Secret>,
}

impl<'a> Resolver<'a> {
    pub fn new(source: &'a dyn SecretSource) -> Resolver<'a> {
        Resolver { source, fetched: BTreeMap::new() }
    }

    /// How many distinct paths have been fetched.
    pub fn fetches(&self) -> usize {
        self.fetched.len()
    }

    /// Resolve one value, or `None` if it was not a reference.
    pub async fn value(&mut self, key: &str, value: &str) -> Result<Option<String>> {
        if !SecretRef::is_reference(value) {
            return Ok(None);
        }
        let reference = parse_for(key, value)?;
        Ok(Some(self.field(key, &reference).await?))
    }

    /// Resolve every reference in a flat map, in place, and say how many.
    ///
    /// This is the shape a parsed `.env` comes in, and the shape anything else
    /// keyed by name can be put into.
    pub async fn pairs(&mut self, pairs: &mut BTreeMap<String, String>) -> Result<usize> {
        let found: Vec<(String, String)> = pairs
            .iter()
            .filter(|(_, value)| SecretRef::is_reference(value))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();

        for (key, raw) in &found {
            let reference = parse_for(key, raw)?;
            let value = self.field(key, &reference).await?;
            pairs.insert(key.clone(), value);
        }

        Ok(found.len())
    }

    /// Resolve every reference under each of these configuration paths.
    ///
    /// The roots have to be named because [`Config`] offers no way to
    /// enumerate its top level, and a resolver that guessed at namespaces
    /// would be the runtime magic this framework does without. Each root is a
    /// dotted path and its whole subtree is searched, so `"database"` covers
    /// `database.connections.pgsql.password`.
    pub async fn config(&mut self, config: &Config, roots: &[&str]) -> Result<usize> {
        let mut resolved = 0;

        for root in roots {
            let Some(tree) = config.get(root) else { continue };

            let mut found = Vec::new();
            collect(root, &tree, &mut found);
            if found.is_empty() {
                continue;
            }

            let mut values = BTreeMap::new();
            for (key, raw) in &found {
                let reference = parse_for(key, raw)?;
                values.insert(key.clone(), self.field(key, &reference).await?);
            }

            resolved += values.len();
            // Written back as one subtree rather than key by key, so a
            // reference sitting inside an array survives as an array element
            // instead of turning the array into an object.
            config.set(root, substitute(root, tree, &values));
        }

        Ok(resolved)
    }

    /// The value of one field, fetching the secret if this is the first key to
    /// ask for it.
    async fn field(&mut self, key: &str, reference: &SecretRef) -> Result<String> {
        if !self.fetched.contains_key(&reference.path) {
            let secret = self.source.read(&reference.path).await.map_err(|error| {
                Error::msg(format!("could not resolve `{key}` from `{reference}`: {error}"))
            })?;
            self.fetched.insert(reference.path.clone(), secret);
        }

        let secret = &self.fetched[&reference.path];
        secret.field(&reference.field).map(str::to_string).ok_or_else(|| {
            let names = secret.names();
            let holds = if names.is_empty() {
                "nothing at all".to_string()
            } else {
                names.join(", ")
            };
            Error::msg(format!(
                "`{key}` refers to `{reference}`, but the secret at `{}` has no field \
                 `{}` — it holds: {holds}",
                reference.path, reference.field
            ))
        })
    }
}

/// Resolve every reference in the process environment, in place.
///
/// Called before the application boots, so `Config::with_defaults` and every
/// `env(...)` lookup afterwards see values rather than references.
///
/// Worth knowing what this costs: an environment variable is inherited by every
/// child process and is readable from `/proc/self/environ` on Linux. It is
/// still far better than a secret in a file that gets committed, and it is what
/// `DATABASE_URL=vault:…` means — but an application that can read its secrets
/// through [`Resolver::config`] instead should.
pub async fn resolve_env(source: &dyn SecretSource) -> Result<usize> {
    let mut pairs: BTreeMap<String, String> = std::env::vars()
        .filter(|(_, value)| SecretRef::is_reference(value))
        .collect();
    if pairs.is_empty() {
        return Ok(0);
    }

    let resolved = Resolver::new(source).pairs(&mut pairs).await?;
    for (key, value) in &pairs {
        // SAFETY: called during single-threaded application boot, before any
        // task that might read the environment concurrently — the same
        // contract `rustlavel_core::env::load` is written to.
        unsafe { std::env::set_var(key, value) };
    }

    Ok(resolved)
}

/// Parse a reference, naming the key it was written under.
///
/// The key is what the person editing the file recognises; the reference on its
/// own leaves them searching for which line it came from.
fn parse_for(key: &str, value: &str) -> Result<SecretRef> {
    SecretRef::parse(value).map_err(|error| Error::msg(format!("`{key}`: {error}")))
}

/// Pull the fields out of the `data` object a read answered with.
///
/// Version 2 of the KV engine nests the secret one level down and puts a
/// `metadata` object beside it; version 1 has the fields at the top. Choosing
/// on the shape rather than making the caller declare which mount they are on
/// keeps the mount version out of every reference in the config file.
fn fields_of(data: &Json) -> BTreeMap<String, String> {
    let inner = match (data.get("data"), data.get("metadata")) {
        (Some(nested @ Json::Object(_)), Some(_)) => nested,
        _ => data,
    };

    inner
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(name, value)| text(value).map(|value| (name.clone(), value)))
                .collect()
        })
        .unwrap_or_default()
}

/// A scalar as its text. A nested object or array is skipped rather than
/// serialised: a reference names one field, and JSON in a config string is
/// almost always a mistake nobody wants substituted silently.
fn text(value: &Json) -> Option<String> {
    match value {
        Json::String(value) => Some(value.clone()),
        Json::Number(value) => Some(value.to_string()),
        Json::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

/// Every reference in a tree, keyed by the dotted path it sits at.
fn collect(prefix: &str, value: &Json, out: &mut Vec<(String, String)>) {
    match value {
        Json::String(value) if SecretRef::is_reference(value) => {
            out.push((prefix.to_string(), value.clone()));
        }
        Json::Object(map) => {
            for (name, value) in map {
                collect(&format!("{prefix}.{name}"), value, out);
            }
        }
        Json::Array(items) => {
            for (index, value) in items.iter().enumerate() {
                collect(&format!("{prefix}.{index}"), value, out);
            }
        }
        _ => {}
    }
}

/// The same tree with each collected reference replaced.
fn substitute(prefix: &str, value: Json, values: &BTreeMap<String, String>) -> Json {
    match value {
        Json::String(ref text) if SecretRef::is_reference(text) => match values.get(prefix) {
            Some(resolved) => Json::String(resolved.clone()),
            None => value,
        },
        Json::Object(map) => Json::Object(
            map.into_iter()
                .map(|(name, value)| {
                    let child = substitute(&format!("{prefix}.{name}"), value, values);
                    (name, child)
                })
                .collect(),
        ),
        Json::Array(items) => Json::Array(
            items
                .into_iter()
                .enumerate()
                .map(|(index, value)| substitute(&format!("{prefix}.{index}"), value, values))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::Fake;

    #[test]
    fn a_value_that_merely_contains_a_colon_is_not_a_reference() {
        // The whole reason the prefix is exact: a connection string is full of
        // colons and must survive untouched.
        assert!(!SecretRef::is_reference("postgres://user:pass@db:5432/app"));
        assert!(!SecretRef::is_reference("redis://cache:6379"));
        assert!(!SecretRef::is_reference("https://vault.internal:8200"));
        assert!(!SecretRef::is_reference("not-vault:secret/data/x#y"));
        assert!(SecretRef::is_reference("vault:secret/data/x#y"));
    }

    #[test]
    fn parses_a_reference_into_a_path_and_a_field() {
        let parsed = SecretRef::parse("vault:secret/data/myapp#database_url").unwrap();

        assert_eq!(parsed.path, "secret/data/myapp");
        assert_eq!(parsed.field, "database_url");
        assert_eq!(parsed.to_string(), "vault:secret/data/myapp#database_url");
    }

    #[test]
    fn a_reference_with_no_field_says_what_one_looks_like() {
        let error = SecretRef::parse("vault:secret/data/myapp").unwrap_err().to_string();

        assert!(error.contains("no field"), "got {error}");
        assert!(error.contains("vault:secret/data/myapp#database_url"), "got {error}");
    }

    #[test]
    fn an_empty_path_or_field_is_malformed() {
        assert!(SecretRef::parse("vault:#field").is_err());
        assert!(SecretRef::parse("vault:secret/data/x#").is_err());
        assert!(SecretRef::parse("vault:").is_err());
    }

    #[test]
    fn a_fallback_is_refused_and_the_error_argues_the_case() {
        let error =
            SecretRef::parse("vault:secret/data/myapp#password|hunter2").unwrap_err().to_string();

        assert!(error.contains("fallback"), "got {error}");
        assert!(error.contains("wrong credential"), "the error has to make the case: {error}");
        // Without a field either, so the fallback explanation still wins.
        assert!(SecretRef::parse("vault:secret/data/myapp|hunter2").unwrap_err().to_string().contains("fallback"));
    }

    fn store() -> crate::fake::FakeVault {
        Fake::new()
            .secret(
                "secret/data/myapp",
                [
                    ("database_url", "postgres://app:s3cr3t@db:5432/app"),
                    ("username", "app"),
                    ("password", "s3cr3t"),
                ],
            )
            .secret("secret/data/mail", [("password", "mailpass")])
            .store()
    }

    #[tokio::test]
    async fn a_reference_resolves_into_configuration() {
        let store = store();
        let config = Config::new();
        config.set("database.url", "vault:secret/data/myapp#database_url");
        config.set("database.pool", 10);

        let resolved = Resolver::new(&store).config(&config, &["database"]).await.unwrap();

        assert_eq!(resolved, 1);
        assert_eq!(config.string("database.url", ""), "postgres://app:s3cr3t@db:5432/app");
        assert_eq!(config.int("database.pool", 0), 10, "everything else is left alone");
    }

    #[tokio::test]
    async fn several_keys_from_one_path_are_fetched_once() {
        let store = store();
        let config = Config::new();
        config.set("database.username", "vault:secret/data/myapp#username");
        config.set("database.password", "vault:secret/data/myapp#password");
        config.set("database.url", "vault:secret/data/myapp#database_url");
        config.set("mail.password", "vault:secret/data/mail#password");

        let mut resolver = Resolver::new(&store);
        let resolved = resolver.config(&config, &["database", "mail"]).await.unwrap();

        assert_eq!(resolved, 4);
        assert_eq!(resolver.fetches(), 2, "two paths, four keys");
        store.assert_read_once("secret/data/myapp");
        store.assert_read_once("secret/data/mail");
        assert_eq!(config.string("database.password", ""), "s3cr3t");
    }

    #[tokio::test]
    async fn an_unresolvable_reference_stops_the_boot_and_names_both() {
        let store = Fake::new().missing("secret/data/nope").store();
        let config = Config::new();
        config.set("database.url", "vault:secret/data/nope#database_url");

        let error = Resolver::new(&store)
            .config(&config, &["database"])
            .await
            .expect_err("an unresolved secret must not boot")
            .to_string();

        assert!(error.contains("database.url"), "the key has to be named: {error}");
        assert!(error.contains("secret/data/nope"), "the path has to be named: {error}");
        // And nothing was substituted, least of all an empty string.
        assert_eq!(config.string("database.url", ""), "vault:secret/data/nope#database_url");
    }

    #[tokio::test]
    async fn a_missing_field_says_which_fields_the_secret_does_have() {
        let store = store();
        let config = Config::new();
        config.set("database.url", "vault:secret/data/myapp#dsn");

        let error =
            Resolver::new(&store).config(&config, &["database"]).await.unwrap_err().to_string();

        assert!(error.contains("no field `dsn`"), "got {error}");
        assert!(error.contains("database_url"), "the names it does hold are the useful part: {error}");
        assert!(!error.contains("s3cr3t"), "an error must never carry a value: {error}");
    }

    #[tokio::test]
    async fn a_malformed_reference_is_rejected_before_anything_is_fetched() {
        let store = store();
        let config = Config::new();
        config.set("database.url", "vault:secret/data/myapp");

        let error =
            Resolver::new(&store).config(&config, &["database"]).await.unwrap_err().to_string();

        assert!(error.contains("database.url"), "got {error}");
        assert!(error.contains("no field"), "got {error}");
        store.assert_reads(0);
    }

    #[tokio::test]
    async fn resolves_a_flat_map_the_way_a_dotenv_file_comes_in() {
        let store = store();
        let mut pairs = BTreeMap::from([
            ("DATABASE_URL".to_string(), "vault:secret/data/myapp#database_url".to_string()),
            ("MAIL_PASSWORD".to_string(), "vault:secret/data/mail#password".to_string()),
            ("APP_URL".to_string(), "https://example.com:443".to_string()),
        ]);

        let resolved = Resolver::new(&store).pairs(&mut pairs).await.unwrap();

        assert_eq!(resolved, 2);
        assert_eq!(pairs["DATABASE_URL"], "postgres://app:s3cr3t@db:5432/app");
        assert_eq!(pairs["MAIL_PASSWORD"], "mailpass");
        assert_eq!(pairs["APP_URL"], "https://example.com:443");
    }

    #[tokio::test]
    async fn a_reference_nested_in_an_object_or_an_array_is_found_and_kept_in_shape() {
        let store = store();
        let config = Config::new();
        config.set(
            "database.connections.pgsql",
            Json::object([("password", Json::from("vault:secret/data/myapp#password"))]),
        );
        config.set(
            "database.hosts",
            Json::Array(vec![
                Json::from("db-1"),
                Json::from("vault:secret/data/myapp#username"),
            ]),
        );

        Resolver::new(&store).config(&config, &["database"]).await.unwrap();

        assert_eq!(config.string("database.connections.pgsql.password", ""), "s3cr3t");
        let hosts = config.get("database.hosts").unwrap();
        assert!(matches!(hosts, Json::Array(_)), "an array must stay an array: {hosts:?}");
        assert_eq!(hosts.as_array().unwrap()[1].as_str(), Some("app"));
    }

    #[tokio::test]
    async fn reads_a_version_1_secret_as_readily_as_a_version_2_one() {
        // The mount version is not something a reference in a config file
        // should have to know.
        let store = Fake::new()
            .answers(
                "kv/legacy",
                Json::object([("data", Json::object([("password", Json::from("v1pass"))]))]),
            )
            .store();

        let mut pairs =
            BTreeMap::from([("PASSWORD".to_string(), "vault:kv/legacy#password".to_string())]);
        Resolver::new(&store).pairs(&mut pairs).await.unwrap();

        assert_eq!(pairs["PASSWORD"], "v1pass");
    }

    #[tokio::test]
    async fn a_number_or_a_flag_in_a_secret_becomes_its_text() {
        let store = Fake::new()
            .answers(
                "secret/data/db",
                Json::object([(
                    "data",
                    Json::object([
                        ("data", Json::object([("port", Json::from(5432)), ("tls", Json::from(true))])),
                        ("metadata", Json::object([("version", Json::from(1))])),
                    ]),
                )]),
            )
            .store();

        let mut pairs = BTreeMap::from([
            ("PORT".to_string(), "vault:secret/data/db#port".to_string()),
            ("TLS".to_string(), "vault:secret/data/db#tls".to_string()),
        ]);
        Resolver::new(&store).pairs(&mut pairs).await.unwrap();

        assert_eq!(pairs["PORT"], "5432");
        assert_eq!(pairs["TLS"], "true");
    }

    #[tokio::test]
    async fn a_secret_never_prints_its_values() {
        let store = store();
        let secret = store.read("secret/data/myapp").await.unwrap();

        let printed = format!("{secret:?}");
        assert!(!printed.contains("s3cr3t"), "a value reached a log: {printed}");
        assert!(printed.contains("redacted"));
        assert!(printed.contains("password"), "the names are what makes it useful: {printed}");
    }

    /// The one test in this crate that touches the process environment.
    ///
    /// Written against a runtime of its own rather than with `#[tokio::test]`
    /// so the lock is held across a blocking call rather than an await: the
    /// environment is process-wide, and two tests writing it at once is
    /// exactly what `set_var` is unsafe about.
    #[test]
    fn resolves_references_in_the_process_environment() {
        static ENVIRONMENT: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());

        // SAFETY: guarded above, and no other test in this crate reads or
        // writes the environment.
        unsafe {
            std::env::set_var("RUSTLAVEL_VAULT_TEST_URL", "vault:secret/data/myapp#database_url");
            std::env::set_var("RUSTLAVEL_VAULT_TEST_PLAIN", "redis://cache:6379");
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for one test");
        let resolved = runtime.block_on(async { resolve_env(&store()).await.unwrap() });

        assert!(resolved >= 1);
        assert_eq!(
            std::env::var("RUSTLAVEL_VAULT_TEST_URL").unwrap(),
            "postgres://app:s3cr3t@db:5432/app"
        );
        assert_eq!(std::env::var("RUSTLAVEL_VAULT_TEST_PLAIN").unwrap(), "redis://cache:6379");

        // SAFETY: as above.
        unsafe {
            std::env::remove_var("RUSTLAVEL_VAULT_TEST_URL");
            std::env::remove_var("RUSTLAVEL_VAULT_TEST_PLAIN");
        }
    }

    #[tokio::test]
    async fn a_configuration_root_that_holds_nothing_is_not_an_error() {
        let store = store();
        let config = Config::new();

        assert_eq!(Resolver::new(&store).config(&config, &["database"]).await.unwrap(), 0);
        store.assert_reads(0);
    }
}
