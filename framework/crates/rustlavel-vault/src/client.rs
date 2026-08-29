//! Talking to OpenBao or HashiCorp Vault.
//!
//! One client for both: OpenBao is a fork of Vault from before the licence
//! changed, and the HTTP API is the same one. Nothing here is compiled against
//! either project — it is an HTTP API, and this speaks it.

use crate::error::VaultError;
use crate::lease::Lease;
use rustlavel_client::Client;
use rustlavel_core::{Json, Result};
use rustlavel_http::Method;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// The header every request carries the token in.
pub const TOKEN_HEADER: &str = "X-Vault-Token";
/// Namespaces are an Enterprise feature in Vault and present in OpenBao.
pub const NAMESPACE_HEADER: &str = "X-Vault-Namespace";
/// What a KV v2 patch must declare, or Vault answers 415.
pub const MERGE_PATCH_CONTENT_TYPE: &str = "application/merge-patch+json";

/// What the store answered, unwrapped from the envelope every reply shares.
#[derive(Debug, Clone)]
pub struct VaultResponse {
    /// The `data` object — the part callers actually want.
    pub data: Json,
    /// The reply as it arrived, kept for fields the envelope does not name.
    raw: Json,
    /// The `auth` object, present on a login.
    pub auth: Json,
    pub lease: Lease,
    /// Warnings the store attached. Worth surfacing: this is how it tells you a
    /// path is deprecated or a policy nearly denied the request.
    pub warnings: Vec<String>,
}

impl VaultResponse {
    /// Parse the envelope.
    pub fn parse(body: &str) -> Result<VaultResponse> {
        let json = Json::parse(body)
            .map_err(|e| VaultError::Unexpected { status: 200, message: e.to_string() })?;

        let lease_id = json.get("lease_id").and_then(Json::as_str).unwrap_or("");
        let duration = json.get("lease_duration").and_then(Json::as_i64).unwrap_or(0);
        let renewable = json.get("renewable").and_then(Json::as_bool).unwrap_or(false);

        // A login puts its timing on the auth object rather than at the top
        // level, and reads the other way round. Taking whichever is present
        // avoids every caller having to know which shape it asked for.
        //
        // The *id* is deliberately not taken from the auth object. Vault's
        // nearest equivalent there is `client_token`, and that is the
        // credential itself — putting it in a field of a `Debug` type is how a
        // token reaches a log. A token needs no id to be renewed anyway:
        // `auth/token/renew-self` identifies it by the header it is sent with.
        let auth = json.get("auth").cloned().unwrap_or(Json::Null);
        let (duration, renewable) = if lease_id.is_empty() && !auth.is_null() {
            (
                auth.get("lease_duration").and_then(Json::as_i64).unwrap_or(0),
                auth.get("renewable").and_then(Json::as_bool).unwrap_or(false),
            )
        } else {
            (duration, renewable)
        };

        Ok(VaultResponse {
            data: json.get("data").cloned().unwrap_or(Json::Null),
            raw: json.clone(),
            auth,
            lease: Lease::new(
                lease_id.to_string(),
                Duration::from_secs(duration.max(0) as u64),
                renewable,
            ),
            warnings: json
                .get("warnings")
                .and_then(Json::as_array)
                .map(|items| items.iter().filter_map(Json::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
        })
    }

    /// The whole reply, for the rare caller that needs a field the envelope
    /// does not name.
    pub fn json(&self) -> &Json {
        &self.raw
    }

    /// A string field out of `data`, by dotted path.
    pub fn string(&self, path: &str) -> Option<String> {
        self.data.get(path).and_then(Json::as_str).map(str::to_string)
    }
}

/// A connection to the secret store.
///
/// Cheap to clone — the token is shared, so re-authenticating in one place is
/// visible everywhere, which is what a background renewer needs.
#[derive(Clone)]
pub struct VaultClient {
    address: String,
    namespace: Option<String>,
    token: Arc<RwLock<String>>,
    http: Client,
    retries: u32,
}

impl VaultClient {
    /// Point at a store: `https://vault.internal:8200`.
    pub fn new(address: impl Into<String>) -> VaultClient {
        let address = address.into().trim_end_matches('/').to_string();

        VaultClient {
            address,
            namespace: None,
            token: Arc::new(RwLock::new(String::new())),
            // Long enough for a store under load, short enough that a hung
            // request does not hold a connection open through a whole deploy.
            http: Client::new().timeout(Duration::from_secs(15)),
            retries: 2,
        }
    }

    /// Read the address and token from the environment, the way every Vault
    /// and OpenBao tool does: `VAULT_ADDR`/`BAO_ADDR`, `VAULT_TOKEN`/`BAO_TOKEN`.
    ///
    /// Reading the environment rather than a config file is deliberate — a
    /// token in `.env` is a token in a file somebody will eventually commit.
    pub fn from_env() -> Result<VaultClient> {
        let address = first_set(&["VAULT_ADDR", "BAO_ADDR"]).ok_or_else(|| {
            rustlavel_core::Error::msg(
                "no secret store address. Set VAULT_ADDR (or BAO_ADDR) to something like \
                 https://vault.internal:8200.",
            )
        })?;

        let mut client = VaultClient::new(address);
        if let Some(token) = first_set(&["VAULT_TOKEN", "BAO_TOKEN"]) {
            client.set_token(token);
        }
        if let Some(namespace) = first_set(&["VAULT_NAMESPACE", "BAO_NAMESPACE"]) {
            client = client.namespace(namespace);
        }
        Ok(client)
    }

    pub fn namespace(mut self, namespace: impl Into<String>) -> VaultClient {
        let namespace = namespace.into();
        self.namespace = (!namespace.is_empty()).then_some(namespace);
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> VaultClient {
        self.http = self.http.timeout(timeout);
        self
    }

    /// How many times to retry a request that failed in a way retrying can fix.
    pub fn retries(mut self, retries: u32) -> VaultClient {
        self.retries = retries;
        self
    }

    /// Answer from a script instead of the network, for tests.
    pub fn faking(mut self, fake: rustlavel_client::Fake) -> VaultClient {
        self.http = self.http.faking(fake);
        self
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    /// The script this client is answering from, for assertions in a test.
    pub fn fake(&self) -> Option<&std::sync::Arc<rustlavel_client::Fake>> {
        self.http.fake()
    }

    pub fn set_token(&self, token: impl Into<String>) {
        if let Ok(mut current) = self.token.write() {
            *current = token.into();
        }
    }

    pub fn token(&self) -> String {
        self.token.read().map(|token| token.clone()).unwrap_or_default()
    }

    /// Forget the token — after revoking it, so a later call fails as
    /// "no token" rather than silently reusing a dead one.
    pub fn clear_token(&self) {
        self.set_token("");
    }

    pub fn has_token(&self) -> bool {
        !self.token().is_empty()
    }

    /// The full URL for an API path, which is always under `/v1/`.
    ///
    /// Leading slashes are trimmed so `secret/data/x` and `/secret/data/x` both
    /// work — the difference is invisible in a config file and produces a 404
    /// that looks like a missing secret.
    pub fn url(&self, path: &str) -> String {
        format!("{}/v1/{}", self.address, path.trim_start_matches('/'))
    }

    pub async fn get(&self, path: &str) -> Result<VaultResponse> {
        self.send(Method::Get, path, None).await
    }

    pub async fn post(&self, path: &str, body: Json) -> Result<VaultResponse> {
        self.send(Method::Post, path, Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> Result<VaultResponse> {
        self.send(Method::Delete, path, None).await
    }

    /// A PATCH, which KV v2 uses to change some fields and leave the rest.
    ///
    /// Sent as `application/merge-patch+json`, which is not a detail: Vault
    /// answers a PATCH declaring plain `application/json` with a 415, and the
    /// error says nothing about content types.
    pub async fn patch(&self, path: &str, body: Json) -> Result<VaultResponse> {
        self.send(Method::Patch, path, Some(body)).await
    }

    /// A LIST, which Vault spells as a query parameter rather than a method,
    /// because not every proxy in the world forwards a `LIST` verb.
    pub async fn list(&self, path: &str) -> Result<VaultResponse> {
        self.send(Method::Get, &format!("{path}?list=true"), None).await
    }

    /// Like [`VaultClient::get`], but a missing path is `None` rather than an
    /// error — reading a secret that may not exist yet is ordinary.
    pub async fn get_optional(&self, path: &str) -> Result<Option<VaultResponse>> {
        match self.get(path).await {
            Ok(response) => Ok(Some(response)),
            Err(error) if is_not_found(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn send(&self, method: Method, path: &str, body: Option<Json>) -> Result<VaultResponse> {
        let mut attempt = 0;

        loop {
            let error = match self.send_once(method, path, body.clone()).await {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };

            if attempt >= self.retries || !error.is_retryable() {
                return Err(error.into());
            }

            // A standby catching up is a matter of milliseconds, so the backoff
            // starts small.
            tokio::time::sleep(Duration::from_millis(100 * 2u64.pow(attempt))).await;
            attempt += 1;
        }
    }

    async fn send_once(
        &self,
        method: Method,
        path: &str,
        body: Option<Json>,
    ) -> std::result::Result<VaultResponse, VaultError> {
        let mut request = self.http.request(method, self.url(path));

        let token = self.token();
        if !token.is_empty() {
            request = request.header(TOKEN_HEADER, token);
        }
        if let Some(namespace) = &self.namespace {
            request = request.header(NAMESPACE_HEADER, namespace.clone());
        }
        if let Some(body) = body {
            request = request.json(body);
            if method == Method::Patch {
                // After `json`, which sets `application/json` — see `patch`.
                request = request.header("content-type", MERGE_PATCH_CONTENT_TYPE);
            }
        }

        let response = request.send().await.map_err(|e| VaultError::Transport(e.to_string()))?;
        let status = response.status.code();
        let text = response.text();

        if !response.status.is_success() {
            // The path is passed for the message, and it is the path without
            // the query string: `?list=true` in a "not found" reads as noise.
            let clean = path.split('?').next().unwrap_or(path);
            return Err(VaultError::from_response(status, clean, &text));
        }

        // 204 No Content is a success with nothing in it, which several write
        // endpoints answer with. Parsing an empty body would fail.
        if text.trim().is_empty() {
            return Ok(VaultResponse {
                data: Json::Null,
                raw: Json::Null,
                auth: Json::Null,
                lease: Lease::none(),
                warnings: Vec::new(),
            });
        }

        VaultResponse::parse(&text).map_err(|e| VaultError::Unexpected {
            status,
            message: e.to_string(),
        })
    }
}

/// Deliberately hand-written so the token cannot reach a log.
impl std::fmt::Debug for VaultClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultClient")
            .field("address", &self.address)
            .field("namespace", &self.namespace)
            .field("token", &if self.has_token() { "<redacted>" } else { "<none>" })
            .finish()
    }
}

fn first_set(names: &[&str]) -> Option<String> {
    names
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string())
}

/// Whether an `Error` came from a `VaultError::NotFound`.
///
/// The round trip through a string is not elegant, but `Error` is the
/// framework's one error type and this keeps `VaultError` out of every public
/// signature in the crate.
fn is_not_found(error: &rustlavel_core::Error) -> bool {
    error.to_string().starts_with("nothing is stored at")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_client::{Fake, FakeResponse};

    fn client(fake: Fake) -> VaultClient {
        let client = VaultClient::new("https://vault.test:8200").faking(fake);
        client.set_token("root-token");
        client
    }

    fn json(body: &str) -> Json {
        Json::parse(body).expect("valid JSON in a test")
    }

    #[test]
    fn builds_urls_under_v1_whatever_the_slashes() {
        let client = VaultClient::new("https://vault.test:8200/");

        assert_eq!(client.url("secret/data/x"), "https://vault.test:8200/v1/secret/data/x");
        assert_eq!(client.url("/secret/data/x"), "https://vault.test:8200/v1/secret/data/x");
    }

    #[test]
    fn debug_never_prints_the_token() {
        let client = VaultClient::new("https://vault.test:8200");
        client.set_token("s.verysecrettoken");

        let printed = format!("{client:?}");
        assert!(!printed.contains("verysecrettoken"), "the token reached a log: {printed}");
        assert!(printed.contains("redacted"));
    }

    #[test]
    fn debug_distinguishes_no_token_from_a_hidden_one() {
        // Otherwise "why is it saying permission denied" is impossible to
        // diagnose from a log.
        let printed = format!("{:?}", VaultClient::new("https://vault.test:8200"));
        assert!(printed.contains("none"), "got {printed}");
    }

    #[test]
    fn parses_the_envelope_a_read_comes_in() {
        let response = VaultResponse::parse(
            r#"{"lease_id":"","renewable":false,"lease_duration":0,
                "data":{"data":{"password":"s3cr3t"},"metadata":{"version":1}},
                "warnings":null}"#,
        )
        .unwrap();

        assert_eq!(response.string("data.password").as_deref(), Some("s3cr3t"));
        assert!(!response.lease.exists());
    }

    #[test]
    fn takes_the_timing_off_the_auth_object_on_a_login() {
        // A login puts it there instead of at the top level, and a renewer that
        // looked only at the top level would never renew a token.
        let response = VaultResponse::parse(
            r#"{"lease_id":"","renewable":false,"lease_duration":0,"data":null,
                "auth":{"client_token":"s.abc","lease_duration":3600,"renewable":true}}"#,
        )
        .unwrap();

        assert_eq!(response.lease.duration, Duration::from_secs(3600));
        assert!(response.lease.renewable);
        assert_eq!(response.auth.get("client_token").and_then(Json::as_str), Some("s.abc"));
    }

    #[test]
    fn a_login_never_puts_the_token_into_the_lease() {
        // `Lease` derives `Debug`, so anything stored in it is one `{:?}` away
        // from a log file. Vault's only id-shaped field on an auth reply is the
        // token itself, which is exactly what must not go there.
        let response = VaultResponse::parse(
            r#"{"lease_id":"","lease_duration":3600,"data":null,
                "auth":{"client_token":"s.verysecrettoken","lease_duration":3600,
                        "renewable":true}}"#,
        )
        .unwrap();

        assert!(response.lease.id.is_empty(), "the token became a lease id");
        assert!(
            !format!("{:?}", response.lease).contains("verysecrettoken"),
            "the token reached a Debug output"
        );
    }

    #[test]
    fn keeps_the_warnings_the_store_attached() {
        let response = VaultResponse::parse(
            r#"{"data":null,"warnings":["this mount is deprecated"]}"#,
        )
        .unwrap();

        assert_eq!(response.warnings, vec!["this mount is deprecated".to_string()]);
    }

    #[test]
    fn a_negative_lease_duration_does_not_wrap_into_eternity() {
        let response =
            VaultResponse::parse(r#"{"lease_id":"x","lease_duration":-1,"data":null}"#).unwrap();

        assert_eq!(response.lease.duration, Duration::ZERO);
    }

    #[tokio::test]
    async fn sends_the_token_and_namespace_headers() {
        let client = VaultClient::new("https://vault.test:8200")
            .namespace("team-a")
            .faking(Fake::new().on("vault.test", FakeResponse::json(json("{}"))));
        client.set_token("s.abc");

        client.get("secret/data/x").await.unwrap();

        let sent = client.fake().unwrap().recorded();
        let headers = &sent[0].headers;
        assert_eq!(headers.get(TOKEN_HEADER), Some("s.abc"));
        assert_eq!(headers.get(NAMESPACE_HEADER), Some("team-a"));
    }

    #[tokio::test]
    async fn an_empty_namespace_sends_no_header_at_all() {
        // A blank namespace header is not the same as none, and Vault treats it
        // as a namespace called "".
        let client = VaultClient::new("https://vault.test:8200")
            .namespace("")
            .faking(Fake::new().on("vault.test", FakeResponse::json(json("{}"))));

        client.get("secret/data/x").await.unwrap();

        assert_eq!(client.fake().unwrap().recorded()[0].headers.get(NAMESPACE_HEADER), None);
    }

    #[tokio::test]
    async fn a_missing_secret_can_be_asked_for_without_it_being_an_error() {
        let fake = Fake::new().on(
            "vault.test",
            FakeResponse::text(r#"{"errors":[]}"#).status(404),
        );
        let client = client(fake);

        assert!(client.get_optional("secret/data/missing").await.unwrap().is_none());
        assert!(client.get("secret/data/missing").await.is_err());
    }

    #[tokio::test]
    async fn a_refusal_is_not_retried_and_says_what_it_might_be() {
        let fake = Fake::new().on(
            "vault.test",
            FakeResponse::text(r#"{"errors":["permission denied"]}"#).status(403),
        );
        let client = client(fake);

        let error = client.get("secret/data/x").await.unwrap_err().to_string();

        assert_eq!(client.fake().unwrap().count(), 1, "a refusal must not be retried");
        assert!(error.contains("expired"), "the message should name the likely causes: {error}");
    }

    #[tokio::test]
    async fn a_standby_is_retried() {
        let fake = Fake::new()
            .on("vault.test", FakeResponse::text(r#"{"errors":["standby"]}"#).status(429));
        let client = client(fake).retries(2);

        assert!(client.get("secret/data/x").await.is_err());
        assert_eq!(client.fake().unwrap().count(), 3, "the first attempt plus two retries");
    }

    #[tokio::test]
    async fn a_sealed_store_fails_immediately() {
        let fake = Fake::new()
            .on("vault.test", FakeResponse::text(r#"{"errors":["sealed"]}"#).status(503));
        let client = client(fake).retries(5);

        let error = client.get("secret/data/x").await.unwrap_err().to_string();

        assert_eq!(client.fake().unwrap().count(), 1, "retrying cannot unseal it");
        assert!(error.contains("unsealed"), "got {error}");
    }

    #[tokio::test]
    async fn a_204_with_no_body_is_a_success() {
        // Several write endpoints answer this way; parsing an empty body as
        // JSON would turn a successful write into an error.
        let fake = Fake::new().on("vault.test", FakeResponse::text("").status(204));
        let client = client(fake);

        let response = client.post("secret/data/x", Json::Null).await.unwrap();
        assert!(response.data.is_null());
    }

    #[tokio::test]
    async fn a_list_does_not_leave_the_query_string_in_the_error() {
        let fake = Fake::new().on("vault.test", FakeResponse::text(r#"{"errors":[]}"#).status(404));
        let client = client(fake);

        let error = client.list("secret/metadata").await.unwrap_err().to_string();
        assert!(!error.contains("list=true"), "the query string is noise here: {error}");
    }
}
