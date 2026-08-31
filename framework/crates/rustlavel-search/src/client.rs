//! Talking to Elasticsearch or OpenSearch.
//!
//! One client for both. OpenSearch is a fork of Elasticsearch 7.10, and every
//! endpoint this crate uses — `_search`, `_bulk`, `_doc`, `_mapping`,
//! `_refresh`, `_cluster/health` — has the same URL, the same request body and
//! the same response shape on both. Nothing here is compiled against either
//! project; it is an HTTP API, and this speaks it. [`ClusterInfo::distribution`]
//! reports which one answered, for the rare caller that has to care.

use crate::error::{Result, SearchError};
use crate::query::Search;
use crate::response::SearchResults;
use rustlavel_client::Client;
use rustlavel_core::Json;
use rustlavel_http::Method;
use std::time::Duration;

/// How a request proves who it is.
///
/// No `Debug`: every variant but `None` holds a credential.
#[derive(Clone)]
enum Auth {
    None,
    /// A pre-built `Authorization` header value, so the secret is encoded once
    /// rather than on every request.
    Header(String),
}

/// A connection to a cluster.
///
/// Cheap to clone — everything in it is shared or small — so one client can
/// live in the application context and be handed to every handler.
#[derive(Clone)]
pub struct SearchClient {
    base_url: String,
    auth: Auth,
    http: Client,
    retries: u32,
}

impl SearchClient {
    /// Point at a cluster: `http://localhost:9200`.
    ///
    /// A trailing slash is trimmed, because `{base}//_search` is a 400 that
    /// says nothing about slashes.
    pub fn new(base_url: impl Into<String>) -> SearchClient {
        SearchClient {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth: Auth::None,
            // A search is meant to be fast; a long default only turns a hung
            // cluster into a hung application. Raise it for a heavy
            // reindex or an expensive aggregation.
            http: Client::new().timeout(Duration::from_secs(30)),
            retries: 2,
        }
    }

    /// Authenticate with a username and password, the way `elastic` and a
    /// native realm user do.
    pub fn basic_auth(mut self, username: &str, password: &str) -> SearchClient {
        let encoded = base64(format!("{username}:{password}").as_bytes());
        self.auth = Auth::Header(format!("Basic {encoded}"));
        self
    }

    /// Authenticate with an API key.
    ///
    /// This is the `encoded` field of what `POST /_security/api_key` returned —
    /// already base64 of `id:api_key`, so it is passed through untouched.
    /// Encoding it a second time produces a 401 that blames the credentials
    /// rather than the encoding.
    pub fn api_key(mut self, encoded: impl Into<String>) -> SearchClient {
        self.auth = Auth::Header(format!("ApiKey {}", encoded.into()));
        self
    }

    /// Authenticate with a bearer token, for a cluster behind an OAuth or
    /// service-token setup.
    pub fn bearer(mut self, token: impl Into<String>) -> SearchClient {
        self.auth = Auth::Header(format!("Bearer {}", token.into()));
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> SearchClient {
        self.http = self.http.timeout(timeout);
        self
    }

    /// How many times to repeat a request the cluster refused in a way that
    /// repeating can fix. See [`SearchError::is_retryable`] for which those
    /// are; a mapping error is never one of them.
    pub fn retries(mut self, retries: u32) -> SearchClient {
        self.retries = retries;
        self
    }

    /// Answer from a script instead of the network, for tests.
    pub fn faking(mut self, fake: rustlavel_client::Fake) -> SearchClient {
        self.http = self.http.faking(fake);
        self
    }

    /// The script this client is answering from, for assertions in a test.
    pub fn fake(&self) -> Option<&std::sync::Arc<rustlavel_client::Fake>> {
        self.http.fake()
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The full URL for an API path.
    ///
    /// Leading slashes are trimmed so `_search` and `/_search` both work — the
    /// difference is invisible in a config file and produces a 400.
    pub fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    /// Whether the cluster is up and what state it is in.
    ///
    /// Use this in a readiness check rather than [`SearchClient::info`]: the
    /// root endpoint answers while shards are still recovering, and a cluster
    /// that answers is not the same as a cluster that can serve a search.
    pub async fn health(&self) -> Result<ClusterHealth> {
        let body = self.json(Method::Get, "_cluster/health", "_cluster/health", None).await?;
        ClusterHealth::parse(&body)
    }

    /// Which server this is, and which version.
    pub async fn info(&self) -> Result<ClusterInfo> {
        let body = self.json(Method::Get, "", "/", None).await?;

        Ok(ClusterInfo {
            name: string_at(&body, "name"),
            cluster_name: string_at(&body, "cluster_name"),
            version: string_at(&body, "version.number"),
            // OpenSearch adds `distribution`; Elasticsearch has never sent it,
            // so its absence is the answer rather than missing information.
            distribution: body
                .get("version.distribution")
                .and_then(Json::as_str)
                .unwrap_or("elasticsearch")
                .to_string(),
        })
    }

    /// Run a search and read the hits back.
    ///
    /// `index` may name several indices (`posts,comments`) or a pattern
    /// (`logs-*`), which is what the cluster's own URL accepts.
    pub async fn search(&self, index: &str, search: &Search) -> Result<SearchResults> {
        let path = format!("{}/_search", encode(index));
        let body = self.json(Method::Post, &path, index, Some(search.body())).await?;
        SearchResults::parse(&body)
    }

    /// How many documents match, without fetching any of them.
    ///
    /// Unlike the total on a search this is never approximate, because
    /// `_count` has no `track_total_hits` cutoff to stop early at.
    pub async fn count(&self, index: &str, search: &Search) -> Result<u64> {
        let path = format!("{}/_count", encode(index));
        let body = self.json(Method::Post, &path, index, Some(search.count_body())).await?;

        body.get("count")
            .and_then(Json::as_i64)
            .map(|count| count.max(0) as u64)
            .ok_or_else(|| SearchError::Malformed {
                context: path,
                message: "the reply has no `count`".to_string(),
            })
    }

    /// Send a request and parse a successful JSON body.
    ///
    /// `context` names the resource for an error message, because several
    /// failures — a version conflict above all — identify the index in the
    /// body but never the document.
    pub(crate) async fn json(
        &self,
        method: Method,
        path: &str,
        context: &str,
        body: Option<Json>,
    ) -> Result<Json> {
        let raw = self.send(method, path, context, body).await?;

        if !raw.is_success() {
            return Err(raw.error(context));
        }
        raw.into_json(path)
    }

    /// Send a request whose body is newline-delimited JSON, which `_bulk` and
    /// `_msearch` require and which is not valid JSON as a whole.
    pub(crate) async fn ndjson(&self, path: &str, context: &str, body: String) -> Result<Json> {
        let raw = self
            .send_payload(Method::Post, path, context, Some(Payload::Ndjson(body)))
            .await?;

        if !raw.is_success() {
            return Err(raw.error(context));
        }
        raw.into_json(path)
    }

    /// Whether something is there, asked with a `GET`.
    ///
    /// The cluster's own answer to "does this exist" is a `HEAD`, and that is
    /// what this used to send — until a real 8.15 refused to finish the reply.
    /// Elasticsearch answers a `HEAD` with `Transfer-Encoding: chunked` and
    /// then writes no body at all, not even the terminating zero-length chunk,
    /// so a client reading the body as the framing promises waits for bytes
    /// that never arrive. Every existence check failed as "chunked body ended
    /// early", and only against a live server: a fake has no framing to get
    /// wrong.
    ///
    /// So this asks with a `GET` and lets the caller keep the response small —
    /// `filter_path` trims an index reply to `{}`, `_source=false` leaves a
    /// document reply at a few fields. Slightly more bytes, and it works.
    pub(crate) async fn exists_at(&self, path: &str, context: &str) -> Result<bool> {
        let raw = self.send(Method::Get, path, context, None).await?;

        match raw.status {
            _ if raw.is_success() => Ok(true),
            404 => Ok(false),
            _ => Err(raw.error(context)),
        }
    }

    /// Send a request and hand back whatever came, retried but not judged.
    ///
    /// A failing status is a successful `Ok(Raw)` here. Two callers need to
    /// see the status before it becomes an error: a `GET` of a missing
    /// document is a 404 that means "no", and a `_bulk` is a 200 that may
    /// still have lost documents.
    pub(crate) async fn send(
        &self,
        method: Method,
        path: &str,
        context: &str,
        body: Option<Json>,
    ) -> Result<Raw> {
        self.send_payload(method, path, context, body.map(Payload::Json)).await
    }

    /// Send a request, retrying the failures that repeating can fix.
    async fn send_payload(
        &self,
        method: Method,
        path: &str,
        context: &str,
        body: Option<Payload>,
    ) -> Result<Raw> {
        let mut attempt = 0;

        loop {
            let (failed, error) = match self.send_once(method, path, body.clone()).await {
                Ok(raw) if raw.is_success() => return Ok(raw),
                Ok(raw) => {
                    let error = raw.error(context);
                    (Some(raw), error)
                }
                Err(error) => (None, error),
            };

            // A transport failure is retried only for a read. A write that
            // timed out may well have been applied, and repeating it would
            // index the document twice; a refusal the cluster actually
            // answered with is safe to repeat, because the work never started.
            let safe = matches!(method, Method::Get | Method::Head);
            let worth_retrying = match &error {
                SearchError::Transport(_) => safe,
                other => other.is_retryable(),
            };

            if attempt >= self.retries || !worth_retrying {
                // The failing response is handed back rather than classified,
                // so a caller that reads a 404 as an answer still can.
                return match failed {
                    Some(raw) => Ok(raw),
                    None => Err(error),
                };
            }

            // A tripped circuit breaker clears in the time it takes a GC to
            // finish, so the backoff starts small and doubles.
            tokio::time::sleep(Duration::from_millis(200 * 2u64.pow(attempt))).await;
            attempt += 1;
        }
    }

    async fn send_once(&self, method: Method, path: &str, body: Option<Payload>) -> Result<Raw> {
        let mut request = self.http.request(method, self.url(path));

        if let Auth::Header(value) = &self.auth {
            request = request.header("authorization", value.clone());
        }

        match body {
            Some(Payload::Json(value)) => request = request.json(value),
            // `application/x-ndjson` is what `_bulk` is defined to take.
            // Elasticsearch 8.15 happens to parse a body labelled
            // `application/json` anyway, but that is a leniency rather than a
            // promise: earlier versions, OpenSearch's stricter settings and
            // proxies in between all refuse it, and the resulting error talks
            // about content types rather than about the body.
            Some(Payload::Ndjson(text)) => {
                request = request.header("content-type", "application/x-ndjson").body(text)
            }
            None => {}
        }

        let response =
            request.send().await.map_err(|e| SearchError::Transport(e.to_string()))?;

        Ok(Raw { status: response.status.code(), body: response.text() })
    }
}

/// Deliberately hand-written, so a password or API key cannot reach a log.
impl std::fmt::Debug for SearchClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchClient")
            .field("base_url", &self.base_url)
            .field(
                "auth",
                &match &self.auth {
                    Auth::None => "<none>",
                    Auth::Header(_) => "<redacted>",
                },
            )
            .field("retries", &self.retries)
            .finish()
    }
}

/// A request body, which is JSON everywhere except `_bulk`.
#[derive(Clone)]
enum Payload {
    Json(Json),
    Ndjson(String),
}

/// A response before anything has been decided about it.
pub(crate) struct Raw {
    pub(crate) status: u16,
    pub(crate) body: String,
}

impl Raw {
    pub(crate) fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// What this response means, once it is known to be a failure.
    pub(crate) fn error(&self, context: &str) -> SearchError {
        SearchError::from_response(self.status, context, &self.body)
    }

    pub(crate) fn into_json(self, path: &str) -> Result<Json> {
        // A few endpoints (`_refresh` on some versions, a HEAD) answer with
        // nothing at all. Parsing that as JSON would turn a success into an
        // error.
        if self.body.trim().is_empty() {
            return Ok(Json::Null);
        }

        Json::parse(&self.body).map_err(|e| SearchError::Malformed {
            context: path.to_string(),
            message: e.to_string(),
        })
    }
}

/// What `_cluster/health` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterHealth {
    pub cluster_name: String,
    pub status: HealthStatus,
    pub number_of_nodes: u32,
    pub active_shards: u32,
    pub unassigned_shards: u32,
    /// The health request itself timed out waiting for the state it was asked
    /// for — not a statement about the cluster.
    pub timed_out: bool,
}

impl ClusterHealth {
    fn parse(body: &Json) -> Result<ClusterHealth> {
        let status = body.get("status").and_then(Json::as_str).ok_or_else(|| {
            SearchError::Malformed {
                context: "_cluster/health".to_string(),
                message: "the reply has no `status`".to_string(),
            }
        })?;

        Ok(ClusterHealth {
            cluster_name: string_at(body, "cluster_name"),
            status: HealthStatus::parse(status),
            number_of_nodes: number_at(body, "number_of_nodes"),
            active_shards: number_at(body, "active_shards"),
            unassigned_shards: number_at(body, "unassigned_shards"),
            timed_out: body.get("timed_out").and_then(Json::as_bool).unwrap_or(false),
        })
    }
}

/// The traffic-light the cluster reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// Every shard, primary and replica, is assigned.
    Green,
    /// Every primary is assigned; some replica is not.
    Yellow,
    /// At least one primary is unassigned — some data cannot be read at all.
    Red,
    /// A colour this client does not know.
    Unknown,
}

impl HealthStatus {
    fn parse(value: &str) -> HealthStatus {
        match value {
            "green" => HealthStatus::Green,
            "yellow" => HealthStatus::Yellow,
            "red" => HealthStatus::Red,
            _ => HealthStatus::Unknown,
        }
    }

    /// Whether every document can be reached.
    ///
    /// Yellow counts. A single-node cluster is yellow the moment an index has
    /// one replica configured, which is the default and which no single node
    /// can ever satisfy — so a readiness check that demanded green would fail
    /// on every developer machine and every scratch container forever.
    pub fn is_usable(self) -> bool {
        matches!(self, HealthStatus::Green | HealthStatus::Yellow)
    }
}

impl std::fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            HealthStatus::Green => "green",
            HealthStatus::Yellow => "yellow",
            HealthStatus::Red => "red",
            HealthStatus::Unknown => "unknown",
        })
    }
}

/// What the root endpoint says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterInfo {
    pub name: String,
    pub cluster_name: String,
    pub version: String,
    /// `elasticsearch` or `opensearch`.
    pub distribution: String,
}

impl ClusterInfo {
    pub fn is_opensearch(&self) -> bool {
        self.distribution == "opensearch"
    }
}

fn string_at(body: &Json, path: &str) -> String {
    body.get(path).and_then(Json::as_str).unwrap_or_default().to_string()
}

fn number_at(body: &Json, path: &str) -> u32 {
    body.get(path).and_then(Json::as_i64).unwrap_or(0).max(0) as u32
}

/// Percent-encode one path segment.
///
/// Document ids come from application data, and an id containing `/`, `?` or a
/// space would otherwise change which endpoint is being called rather than
/// which document. `,`, `*` and `-` are left alone because an index argument
/// legitimately uses all three — `logs-*,metrics-*` is one index expression,
/// not three escaped characters.
pub(crate) fn encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());

    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b',' | b'*' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }

    out
}

/// Base64, for the one header that needs it.
///
/// Twenty lines rather than a dependency, and the framework already writes its
/// own in the crates that need it. This is not cryptography — it is an
/// encoding, and the rule about not writing your own applies to the former.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let bits = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;

        out.push(ALPHABET[(bits >> 18) as usize & 63] as char);
        out.push(ALPHABET[(bits >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(bits >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[bits as usize & 63] as char } else { '=' });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_client::{Fake, FakeResponse};

    fn json(body: &str) -> Json {
        Json::parse(body).expect("valid JSON in a test")
    }

    #[test]
    fn builds_urls_whatever_the_slashes() {
        let client = SearchClient::new("http://localhost:9200/");

        assert_eq!(client.url("posts/_search"), "http://localhost:9200/posts/_search");
        assert_eq!(client.url("/posts/_search"), "http://localhost:9200/posts/_search");
    }

    #[test]
    fn debug_never_prints_the_password() {
        let client = SearchClient::new("http://localhost:9200").basic_auth("elastic", "hunter2");

        let printed = format!("{client:?}");
        assert!(!printed.contains("hunter2"), "the password reached a log: {printed}");
        // The encoded form would leak it just as thoroughly.
        assert!(!printed.contains("ZWxhc3RpYzpodW50ZXIy"), "got {printed}");
        assert!(printed.contains("redacted"));
    }

    #[test]
    fn debug_distinguishes_no_credentials_from_hidden_ones() {
        // Otherwise "why is it saying 401" is impossible to diagnose from a log.
        let printed = format!("{:?}", SearchClient::new("http://localhost:9200"));
        assert!(printed.contains("none"), "got {printed}");
    }

    #[test]
    fn base64_matches_the_worked_example_in_rfc_4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"elastic:changeme"), "ZWxhc3RpYzpjaGFuZ2VtZQ==");
    }

    #[test]
    fn an_id_cannot_change_which_endpoint_is_called() {
        // An id is application data. Without encoding, `a/b` would address
        // `/posts/_doc/a/b`, which is a different route entirely.
        assert_eq!(encode("a/b"), "a%2Fb");
        assert_eq!(encode("with space"), "with%20space");
        assert_eq!(encode("q?x=1"), "q%3Fx%3D1");
        assert_eq!(encode("plain-id_1.2~3"), "plain-id_1.2~3");
        // An index expression is passed in the same position and needs these.
        assert_eq!(encode("logs-*,metrics-*"), "logs-*,metrics-*");
    }

    #[tokio::test]
    async fn sends_the_authorization_header_basic_auth_builds() {
        let client = SearchClient::new("http://localhost:9200")
            .basic_auth("elastic", "changeme")
            .faking(Fake::new().on("localhost:9200", FakeResponse::json(json(r#"{"status":"green"}"#))));

        client.health().await.unwrap();

        let sent = client.fake().unwrap().recorded();
        assert_eq!(sent[0].headers.get("authorization"), Some("Basic ZWxhc3RpYzpjaGFuZ2VtZQ=="));
    }

    #[tokio::test]
    async fn an_api_key_is_passed_through_rather_than_encoded_again() {
        // The `encoded` field of a created API key is already base64. Encoding
        // it twice yields a 401 that blames the key.
        let client = SearchClient::new("http://localhost:9200")
            .api_key("VnVhQ2ZHY0JDZGJrUW0tZTVhT3g6dWkybHAyYXhUTm1zeWFrdzl0dk5udw==")
            .faking(Fake::new().fallback(FakeResponse::json(json(r#"{"status":"green"}"#))));

        client.health().await.unwrap();

        assert_eq!(
            client.fake().unwrap().recorded()[0].headers.get("authorization"),
            Some("ApiKey VnVhQ2ZHY0JDZGJrUW0tZTVhT3g6dWkybHAyYXhUTm1zeWFrdzl0dk5udw==")
        );
    }

    #[tokio::test]
    async fn no_credentials_means_no_header_at_all() {
        // An empty `Authorization` is not the same as none, and a cluster with
        // security off treats one as a malformed credential.
        let client = SearchClient::new("http://localhost:9200")
            .faking(Fake::new().fallback(FakeResponse::json(json(r#"{"status":"green"}"#))));

        client.health().await.unwrap();

        assert_eq!(client.fake().unwrap().recorded()[0].headers.get("authorization"), None);
    }

    #[tokio::test]
    async fn reads_the_health_a_single_node_cluster_actually_reports() {
        // Copied from a running 8.15 with one index at default settings. Note
        // the yellow: one node can never assign a replica.
        let body = r#"{"cluster_name":"docker-cluster","status":"yellow","timed_out":false,"number_of_nodes":1,"number_of_data_nodes":1,"active_primary_shards":1,"active_shards":1,"relocating_shards":0,"initializing_shards":0,"unassigned_shards":1,"delayed_unassigned_shards":0,"number_of_pending_tasks":0,"number_of_in_flight_fetch":0,"task_max_waiting_in_queue_millis":0,"active_shards_percent_as_number":50.0}"#;
        let client = SearchClient::new("http://localhost:9200")
            .faking(Fake::new().on("_cluster/health", FakeResponse::json(json(body))));

        let health = client.health().await.unwrap();

        assert_eq!(health.status, HealthStatus::Yellow);
        assert_eq!(health.cluster_name, "docker-cluster");
        assert_eq!(health.unassigned_shards, 1);
        assert!(health.status.is_usable(), "yellow is the normal state of a one-node cluster");
        assert!(!HealthStatus::Red.is_usable());
    }

    #[tokio::test]
    async fn tells_elasticsearch_and_opensearch_apart_without_preferring_either() {
        let elastic = r#"{"name":"e9f1c933ed49","cluster_name":"docker-cluster","cluster_uuid":"n3-0v9hLR4mBTvOqPRVFmA","version":{"number":"8.15.0","build_flavor":"default","build_type":"docker","build_hash":"1a77947f34deddb41af25e6f0ddb8e830159c179","build_date":"2024-08-05T10:05:34.233336849Z","build_snapshot":false,"lucene_version":"9.11.1","minimum_wire_compatibility_version":"7.17.0","minimum_index_compatibility_version":"7.0.0"},"tagline":"You Know, for Search"}"#;
        let opensearch = r#"{"name":"opensearch-node1","cluster_name":"docker-cluster","cluster_uuid":"C1v1zpcYRW6cLtLBEIyOZg","version":{"distribution":"opensearch","number":"2.11.0","build_type":"tar","build_hash":"4dcad6dd1fd45b6bd91f041a041829c8687278fa","build_date":"2023-10-13T02:57:04.940191475Z","build_snapshot":false,"lucene_version":"9.7.0","minimum_wire_compatibility_version":"7.10.0","minimum_index_compatibility_version":"7.0.0"},"tagline":"The OpenSearch Project: https://opensearch.org/"}"#;

        for (body, distribution, version) in [
            (elastic, "elasticsearch", "8.15.0"),
            (opensearch, "opensearch", "2.11.0"),
        ] {
            let client = SearchClient::new("http://localhost:9200")
                .faking(Fake::new().fallback(FakeResponse::json(json(body))));

            let info = client.info().await.unwrap();
            assert_eq!(info.distribution, distribution);
            assert_eq!(info.version, version);
            assert_eq!(info.is_opensearch(), distribution == "opensearch");
        }
    }

    #[tokio::test]
    async fn a_rejected_request_is_retried_and_a_mapping_error_is_not() {
        let rejected = r#"{"error":{"root_cause":[{"type":"circuit_breaking_exception","reason":"[parent] Data too large","bytes_wanted":1,"bytes_limit":0,"durability":"TRANSIENT"}],"type":"circuit_breaking_exception","reason":"[parent] Data too large","bytes_wanted":1,"bytes_limit":0,"durability":"TRANSIENT"},"status":429}"#;
        let broken = r#"{"error":{"root_cause":[{"type":"parsing_exception","reason":"unknown query [matchh]","line":1,"col":30}],"type":"parsing_exception","reason":"unknown query [matchh]","line":1,"col":30},"status":400}"#;

        let client = SearchClient::new("http://localhost:9200")
            .retries(2)
            .faking(Fake::new().fallback(FakeResponse::text(rejected).status(429)));
        assert!(client.search("posts", &Search::new()).await.is_err());
        assert_eq!(client.fake().unwrap().count(), 3, "the first attempt plus two retries");

        let client = SearchClient::new("http://localhost:9200")
            .retries(5)
            .faking(Fake::new().fallback(FakeResponse::text(broken).status(400)));
        assert!(client.search("posts", &Search::new()).await.is_err());
        assert_eq!(client.fake().unwrap().count(), 1, "a broken query cannot be fixed by repeating it");
    }

    #[tokio::test]
    async fn a_search_against_a_missing_index_says_so_by_name() {
        let body = r#"{"error":{"root_cause":[{"type":"index_not_found_exception","reason":"no such index [pots]","resource.type":"index_or_alias","resource.id":"pots","index_uuid":"_na_","index":"pots"}],"type":"index_not_found_exception","reason":"no such index [pots]","resource.type":"index_or_alias","resource.id":"pots","index_uuid":"_na_","index":"pots"},"status":404}"#;
        let client = SearchClient::new("http://localhost:9200")
            .faking(Fake::new().fallback(FakeResponse::text(body).status(404)));

        let error = client.search("pots", &Search::new()).await.unwrap_err();

        assert!(error.is_index_not_found());
        assert!(error.to_string().contains("pots"), "got {error}");
        assert!(!error.is_retryable(), "the index will not appear by itself");
    }

    #[tokio::test]
    async fn counting_does_not_send_the_fields_count_refuses() {
        // `_count` answers 400 for `from`, `size`, `sort` and `aggs`, so the
        // count body is built from the query alone.
        let client = SearchClient::new("http://localhost:9200")
            .faking(Fake::new().fallback(FakeResponse::json(json(r#"{"count":42,"_shards":{"total":1,"successful":1,"skipped":0,"failed":0}}"#))));

        let search = Search::new().size(10).from(20).sort_desc("created_at");
        assert_eq!(client.count("posts", &search).await.unwrap(), 42);

        let sent = client.fake().unwrap().recorded()[0].json().unwrap();
        assert!(sent.get("size").is_none(), "size would make _count answer 400");
        assert!(sent.get("sort").is_none(), "sort would make _count answer 400");
        assert!(sent.get("query").is_some());
    }
}
