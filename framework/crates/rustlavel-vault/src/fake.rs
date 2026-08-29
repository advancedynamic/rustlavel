//! `Vault::fake()` — a secret store that answers from a script.
//!
//! This is `Http::fake()` and `Ai::fake()` for secrets. An application's tests
//! must never need a running Vault: a store is a server, a token, an unseal
//! ceremony and a policy, and a test suite that needs all four is a test suite
//! nobody runs.
//!
//! ```ignore
//! let store = Fake::new()
//!     .secret("secret/data/myapp", [("database_url", "postgres://…")])
//!     .store();
//!
//! boot(store.client()).await?;
//!
//! store.assert_read_once("secret/data/myapp");
//! ```
//!
//! The script is answered at the HTTP layer rather than above it, so everything
//! under test goes through the real [`VaultClient`] — the same envelope
//! parsing, the same error mapping, the same headers. A fake that shortcut that
//! would pass while the real client failed, which is the one thing a fake must
//! not do.

use crate::client::VaultClient;
use crate::plugin::Vault;
use crate::resolve::{BoxFuture, Secret, SecretSource};
use rustlavel_client::FakeResponse;
use rustlavel_core::{Json, Result};
use std::time::Duration;

/// The address a faked client points at. It is never connected to; it exists so
/// a URL in a failed assertion reads like a real one.
pub const FAKE_ADDRESS: &str = "https://vault.fake:8200";

/// A script of paths to answers.
///
/// The longest path that matches wins, whatever order it was written in. That
/// matters because the underlying HTTP fake matches a URL by substring:
/// `secret/data/app` would otherwise answer a read of `secret/data/app2`, and
/// a test that quietly read the wrong secret is worse than one that fails.
#[derive(Default)]
pub struct Fake {
    routes: Vec<(String, FakeResponse)>,
    fallback: Option<FakeResponse>,
}

impl Fake {
    pub fn new() -> Fake {
        Fake::default()
    }

    /// A secret in the shape version 2 of the KV engine answers with — the
    /// fields nested under `data.data`, which is what `secret/data/…` returns.
    pub fn secret<K, V, I>(self, path: &str, fields: I) -> Fake
    where
        K: Into<String>,
        V: Into<String>,
        I: IntoIterator<Item = (K, V)>,
    {
        self.answers(path, kv2(fields, Json::Null, 0, false))
    }

    /// A secret that comes with a lease, the way a dynamic credential does.
    ///
    /// `duration` is what the store says the lease is good for; the renewer
    /// takes it from here.
    pub fn leased<K, V, I>(
        self,
        path: &str,
        fields: I,
        lease_id: &str,
        duration: Duration,
    ) -> Fake
    where
        K: Into<String>,
        V: Into<String>,
        I: IntoIterator<Item = (K, V)>,
    {
        self.answers(path, kv2(fields, Json::from(lease_id), duration.as_secs(), true))
    }

    /// Answer this path with a body written out by hand — for an envelope this
    /// builder does not have a name for.
    pub fn answers(mut self, path: &str, body: Json) -> Fake {
        self.routes.push((pattern(path), FakeResponse::json(body)));
        self
    }

    /// Nothing is stored at this path.
    pub fn missing(mut self, path: &str) -> Fake {
        self.routes.push((pattern(path), FakeResponse::text(r#"{"errors":[]}"#).status(404)));
        self
    }

    /// The token may not read this path.
    pub fn denies(mut self, path: &str) -> Fake {
        self.routes.push((
            pattern(path),
            FakeResponse::text(r#"{"errors":["permission denied"]}"#).status(403),
        ));
        self
    }

    /// Answer this path with a status of your choosing — anything
    /// [`VaultError`](crate::VaultError) distinguishes.
    pub fn fails(mut self, path: &str, status: u16, message: &str) -> Fake {
        let body = Json::object([("errors", Json::Array(vec![Json::from(message)]))]);
        self.routes.push((pattern(path), FakeResponse::json(body).status(status)));
        self
    }

    /// The store is sealed.
    ///
    /// This replaces the script rather than adding to it: a sealed store
    /// answers nothing, and a script where some paths still worked would be
    /// testing a situation that cannot happen.
    pub fn sealed(self) -> Fake {
        Fake {
            routes: Vec::new(),
            fallback: Some(FakeResponse::text(r#"{"errors":["Vault is sealed"]}"#).status(503)),
        }
    }

    /// The store this script describes.
    pub fn store(self) -> FakeVault {
        let mut routes = self.routes;
        // Longest first. A stable sort, so two paths of the same length keep
        // the order they were written in.
        routes.sort_by_key(|(pattern, _)| std::cmp::Reverse(pattern.len()));

        let mut script = rustlavel_client::Fake::new();
        for (pattern, response) in routes {
            script = script.on(&pattern, response);
        }
        if let Some(fallback) = self.fallback {
            script = script.fallback(fallback);
        }

        // No retries. A fake that retried would sleep through a backoff in a
        // test suite, and would record three reads where the test asserts one.
        let client = VaultClient::new(FAKE_ADDRESS).retries(0).faking(script);
        client.set_token("fake-token");
        FakeVault { client }
    }
}

/// A faked store: a client to hand to the code under test, and the record of
/// what that code asked for.
pub struct FakeVault {
    client: VaultClient,
}

impl FakeVault {
    /// The client to hand to whatever is under test. Cloning it is cheap and
    /// every clone shares this record.
    pub fn client(&self) -> &VaultClient {
        &self.client
    }

    /// The paths that were read, in order, as they were written in the config
    /// file — without the address or the `/v1/` prefix the client adds.
    pub fn reads(&self) -> Vec<String> {
        self.client
            .fake()
            .expect("a faked client always has a script")
            .recorded()
            .iter()
            .map(|request| read_path(&request.url))
            .collect()
    }

    pub fn read_count(&self, path: &str) -> usize {
        self.reads().iter().filter(|read| *read == path).count()
    }

    #[track_caller]
    pub fn assert_read(&self, path: &str) {
        assert!(
            self.read_count(path) > 0,
            "expected a read of {path:?}; the store was asked for {:?}",
            self.reads()
        );
    }

    /// Assert a path was read exactly once.
    ///
    /// The assertion behind the caching: three keys taken from one secret must
    /// cost one round trip, and nothing else here would notice if they stopped.
    #[track_caller]
    pub fn assert_read_once(&self, path: &str) {
        assert_eq!(
            self.read_count(path),
            1,
            "expected exactly one read of {path:?}; the store was asked for {:?}",
            self.reads()
        );
    }

    #[track_caller]
    pub fn assert_not_read(&self, path: &str) {
        assert_eq!(self.read_count(path), 0, "did not expect a read of {path:?}");
    }

    #[track_caller]
    pub fn assert_reads(&self, expected: usize) {
        assert_eq!(
            self.reads().len(),
            expected,
            "unexpected number of reads: {:?}",
            self.reads()
        );
    }
}

impl SecretSource for FakeVault {
    fn read<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Secret>> {
        self.client.read(path)
    }
}

/// Delegated, so a faked store cannot print a token either.
impl std::fmt::Debug for FakeVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FakeVault({:?})", self.client)
    }
}

impl Vault {
    /// A store that answers from an empty script — every read fails loudly.
    pub fn fake() -> FakeVault {
        Fake::new().store()
    }

    /// A store that answers from a script written out in advance.
    pub fn fake_with(fake: Fake) -> FakeVault {
        fake.store()
    }
}

/// A KV version 2 envelope, optionally with a lease.
fn kv2<K, V, I>(fields: I, lease_id: Json, duration: u64, renewable: bool) -> Json
where
    K: Into<String>,
    V: Into<String>,
    I: IntoIterator<Item = (K, V)>,
{
    let data = Json::object(fields.into_iter().map(|(name, value)| (name, Json::from(value.into()))));

    Json::object([
        ("lease_id", lease_id),
        ("lease_duration", Json::from(duration)),
        ("renewable", Json::from(renewable)),
        (
            "data",
            Json::object([("data", data), ("metadata", Json::object([("version", Json::from(1))]))]),
        ),
    ])
}

/// What the client's URL for a path looks like, which is what the underlying
/// HTTP fake matches on.
fn pattern(path: &str) -> String {
    format!("/v1/{}", path.trim_start_matches('/'))
}

/// The store path out of a recorded URL, undoing what the client added.
fn read_path(url: &str) -> String {
    let path = url.split_once("/v1/").map(|(_, rest)| rest).unwrap_or(url);
    path.split('?').next().unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answers_a_read_from_the_script() {
        let store = Fake::new()
            .secret("secret/data/myapp", [("password", "s3cr3t")])
            .store();

        let secret = store.read("secret/data/myapp").await.unwrap();

        assert_eq!(secret.field("password"), Some("s3cr3t"));
        assert_eq!(secret.path(), "secret/data/myapp");
    }

    #[tokio::test]
    async fn records_which_paths_were_read() {
        let store = Fake::new()
            .secret("secret/data/one", [("a", "1")])
            .secret("secret/data/two", [("b", "2")])
            .store();

        store.read("secret/data/one").await.unwrap();
        store.read("secret/data/two").await.unwrap();
        store.read("secret/data/one").await.unwrap();

        assert_eq!(store.reads(), ["secret/data/one", "secret/data/two", "secret/data/one"]);
        assert_eq!(store.read_count("secret/data/one"), 2);
        store.assert_read("secret/data/two");
        store.assert_not_read("secret/data/three");
        store.assert_reads(3);
    }

    #[tokio::test]
    async fn scripts_a_refusal_a_missing_path_and_a_sealed_store() {
        let store = Fake::new().denies("secret/data/secret").missing("secret/data/gone").store();

        let denied = store.read("secret/data/secret").await.unwrap_err().to_string();
        assert!(denied.contains("refused"), "got {denied}");

        let missing = store.read("secret/data/gone").await.unwrap_err().to_string();
        assert!(missing.contains("nothing is stored at"), "got {missing}");

        let sealed = Fake::new().sealed().store();
        let error = sealed.read("secret/data/anything").await.unwrap_err().to_string();
        assert!(error.contains("sealed"), "got {error}");
        assert_eq!(sealed.reads().len(), 1, "a sealed store is not retried");
    }

    #[tokio::test]
    async fn an_unscripted_path_fails_loudly_and_is_not_retried() {
        // A test that quietly passed because a read went somewhere nobody
        // scripted is worse than no test.
        let store = Fake::new().secret("secret/data/known", [("a", "1")]).store();

        let error = store.read("secret/data/surprise").await.unwrap_err().to_string();

        assert!(error.contains("no fake response is scripted"), "got {error}");
        store.assert_reads(1);
    }

    #[tokio::test]
    async fn scripts_a_secret_that_comes_with_a_lease() {
        let store = Fake::new()
            .leased(
                "database/creds/app",
                [("username", "v-token-app-x"), ("password", "generated")],
                "database/creds/app/abc",
                Duration::from_secs(3600),
            )
            .store();

        let secret = store.read("database/creds/app").await.unwrap();

        assert_eq!(secret.lease().id, "database/creds/app/abc");
        assert_eq!(secret.lease().duration, Duration::from_secs(3600));
        assert!(secret.lease().renewable);
    }

    #[tokio::test]
    async fn a_path_is_not_answered_by_a_shorter_one_it_begins_with() {
        let store = Fake::new()
            .secret("secret/data/app", [("which", "app")])
            .secret("secret/data/app2", [("which", "app2")])
            .store();

        assert_eq!(store.read("secret/data/app").await.unwrap().field("which"), Some("app"));
        assert_eq!(store.read("secret/data/app2").await.unwrap().field("which"), Some("app2"));
    }

    #[tokio::test]
    async fn a_status_can_be_scripted_for_any_path() {
        let store = Fake::new().fails("secret/data/x", 429, "standby").store();

        let error = store.read("secret/data/x").await.unwrap_err().to_string();

        assert!(error.contains("standby"), "got {error}");
    }

    #[test]
    fn a_faked_store_never_prints_its_token() {
        let printed = format!("{:?}", Vault::fake());

        assert!(!printed.contains("fake-token"), "got {printed}");
        assert!(printed.contains("redacted"));
    }
}
