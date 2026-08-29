//! rustlavel-client: the outbound HTTP client.
//!
//! Written on Tokio's TCP the same way the server is, with TLS delegated to
//! rustls — the framework writes its own protocols but never its own
//! cryptography. It exists because the AI and MCP packages need to call out,
//! and because an application often does too.
//!
//! ```ignore
//! let response = Client::new()
//!     .post("https://api.example.com/v1/things")
//!     .header("authorization", format!("Bearer {token}"))
//!     .json(Json::object([("name", "widget".into())]))
//!     .send()
//!     .await?;
//! ```

pub mod fake;
pub mod stream;
pub mod url;

use rustlavel_core::events::Event;
use rustlavel_core::{Error, Json, Result};
use rustlavel_http::{Headers, Method, Status};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use url::Url;

pub use fake::{Fake, FakeResponse};
pub use stream::{Body, ServerSentEvent, SseReader};

/// A response from an outbound request.
#[derive(Debug, Clone)]
pub struct ClientResponse {
    pub status: Status,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl ClientResponse {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Result<Json> {
        Json::parse(&self.text())
    }

    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// Turn a non-2xx response into an error, keeping the body — an API's
    /// error message is usually the only thing that explains the failure.
    pub fn error_for_status(self) -> Result<ClientResponse> {
        if self.is_success() {
            return Ok(self);
        }
        let body = self.text();
        let excerpt = if body.len() > 500 { format!("{}…", &body[..500]) } else { body };
        Err(Error::msg(format!("HTTP {}: {excerpt}", self.status)))
    }
}

/// Shared settings for outbound requests.
#[derive(Clone)]
pub struct Client {
    timeout: Duration,
    /// How many times to retry a request that failed to connect or timed out.
    retries: u32,
    default_headers: Headers,
    max_body_bytes: usize,
    fake: Option<Arc<Fake>>,
}

impl Default for Client {
    fn default() -> Self {
        let mut default_headers = Headers::new();
        default_headers.set("user-agent", concat!("rustlavel/", env!("CARGO_PKG_VERSION")));
        default_headers.set("accept", "*/*");

        Client {
            timeout: Duration::from_secs(30),
            retries: 0,
            default_headers,
            max_body_bytes: 32 * 1024 * 1024,
            fake: None,
        }
    }
}

impl Client {
    pub fn new() -> Self {
        Client::default()
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Retry connection failures and timeouts, with exponential backoff.
    ///
    /// Only transport failures are retried; a 500 is not, because the request
    /// may already have had an effect on the server.
    pub fn retries(mut self, retries: u32) -> Self {
        self.retries = retries;
        self
    }

    pub fn default_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.default_headers.set(name, value);
        self
    }

    /// Answer from a script instead of the network, for tests.
    ///
    /// This is `Http::fake()` — an application's tests should never depend on
    /// a third-party API being up.
    pub fn faking(mut self, fake: Fake) -> Self {
        self.fake = Some(Arc::new(fake));
        self
    }

    pub fn fake(&self) -> Option<&Arc<Fake>> {
        self.fake.as_ref()
    }

    pub fn request(&self, method: Method, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder {
            client: self.clone(),
            method,
            url: url.into(),
            headers: self.default_headers.clone(),
            body: Vec::new(),
        }
    }

    pub fn get(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(Method::Get, url)
    }

    pub fn post(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(Method::Post, url)
    }

    pub fn put(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(Method::Put, url)
    }

    pub fn patch(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(Method::Patch, url)
    }

    pub fn delete(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(Method::Delete, url)
    }
}

/// One outbound request being assembled.
pub struct RequestBuilder {
    client: Client,
    method: Method,
    url: String,
    headers: Headers,
    body: Vec<u8>,
}

impl RequestBuilder {
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.set(name, value);
        self
    }

    pub fn bearer(self, token: &str) -> Self {
        self.header("authorization", format!("Bearer {token}"))
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    pub fn json(self, value: Json) -> Self {
        self.header("content-type", "application/json").body(value.to_string())
    }

    /// Ask for a server-sent event stream.
    pub fn accept_events(self) -> Self {
        self.header("accept", "text/event-stream")
    }

    pub fn method(&self) -> Method {
        self.method
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn headers(&self) -> &Headers {
        &self.headers
    }

    pub fn body_bytes(&self) -> &[u8] {
        &self.body
    }

    /// Send the request and read the whole response.
    pub async fn send(self) -> Result<ClientResponse> {
        let started = Instant::now();
        let method = self.method;
        let url = self.url.clone();

        // A faked client never opens a socket, so a test cannot accidentally
        // depend on the network.
        if let Some(fake) = self.client.fake.clone() {
            let response = fake.respond(&self)?;
            record(method, &url, Some(response.status), started);
            return Ok(response);
        }

        let mut attempt = 0;
        loop {
            match self.send_once().await {
                Ok(response) => {
                    record(method, &url, Some(response.status), started);
                    return Ok(response);
                }
                Err(error) if attempt < self.client.retries && is_retryable(&error) => {
                    let backoff = Duration::from_millis(100 * 2u64.pow(attempt));
                    rustlavel_core::debug!("retrying {method} {url} after {error}");
                    tokio::time::sleep(backoff).await;
                    attempt += 1;
                }
                Err(error) => {
                    record(method, &url, None, started);
                    return Err(error);
                }
            }
        }
    }

    /// Send and return the body as a stream, for server-sent events.
    pub async fn stream(self) -> Result<Body> {
        if let Some(fake) = self.client.fake.clone() {
            let response = fake.respond(&self)?;
            return Ok(Body::from_bytes(response.status, response.headers, response.body));
        }

        let url = Url::parse(&self.url)?;
        let stream = connect(&url, self.client.timeout).await?;
        let request = self.wire(&url);

        stream::open(stream, request, self.client.timeout).await
    }

    async fn send_once(&self) -> Result<ClientResponse> {
        let url = Url::parse(&self.url)?;
        let mut stream = connect(&url, self.client.timeout).await?;
        let request = self.wire(&url);

        let exchange = async {
            stream.write_all(&request).await.map_err(Error::Io)?;
            stream.flush().await.map_err(Error::Io)?;
            read_response(&mut stream, self.client.max_body_bytes).await
        };

        tokio::time::timeout(self.client.timeout, exchange)
            .await
            .map_err(|_| Error::msg(format!("{} {} timed out", self.method, self.url)))?
    }

    /// Serialize the request onto the wire.
    fn wire(&self, url: &Url) -> Vec<u8> {
        let mut head = format!("{} {} HTTP/1.1\r\n", self.method, url.target);
        head.push_str(&format!("host: {}\r\n", url.authority()));

        for (name, value) in self.headers.iter() {
            if name == "host" || name == "content-length" || name == "connection" {
                continue;
            }
            head.push_str(&format!("{name}: {value}\r\n"));
        }

        // One request per connection: pooling outbound connections is not worth
        // the complexity until something measures it.
        head.push_str("connection: close\r\n");
        if !self.body.is_empty() || self.method.takes_body() {
            head.push_str(&format!("content-length: {}\r\n", self.body.len()));
        }
        head.push_str("\r\n");

        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

fn record(method: Method, url: &str, status: Option<Status>, started: Instant) {
    if !rustlavel_core::events::has_subscribers() {
        return;
    }
    let mut event = Event::new("http.client")
        .with("method", method.as_str())
        .with("url", url)
        .took(started.elapsed());
    if let Some(status) = status {
        event = event.with("status", status.code());
    }
    event.dispatch();
}

/// Whether a failure is worth trying again.
fn is_retryable(error: &Error) -> bool {
    let text = error.to_string();
    text.contains("timed out")
        || text.contains("Connection refused")
        || text.contains("connection reset")
        || text.contains("Temporary failure")
}

/// Either a plain or a TLS-wrapped connection.
///
/// An enum rather than a boxed trait object: there are exactly two cases, and
/// this keeps the read path free of dynamic dispatch.
pub enum Connection {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl Connection {
    pub async fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Connection::Plain(stream) => stream.write_all(bytes).await,
            Connection::Tls(stream) => stream.write_all(bytes).await,
        }
    }

    pub async fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Connection::Plain(stream) => stream.flush().await,
            Connection::Tls(stream) => stream.flush().await,
        }
    }

    pub async fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Connection::Plain(stream) => stream.read(buffer).await,
            Connection::Tls(stream) => stream.read(buffer).await,
        }
    }
}

/// Open a connection, negotiating TLS when the URL asks for it.
pub async fn connect(url: &Url, timeout: Duration) -> Result<Connection> {
    let address = url.socket_address();

    let tcp = tokio::time::timeout(timeout, TcpStream::connect(&address))
        .await
        .map_err(|_| Error::msg(format!("connecting to {address} timed out")))?
        .map_err(|e| Error::msg(format!("cannot connect to {address}: {e}")))?;

    let _ = tcp.set_nodelay(true);

    if !url.secure {
        return Ok(Connection::Plain(tcp));
    }

    let connector = tls_connector();
    let server_name = rustls::pki_types::ServerName::try_from(url.host.clone())
        .map_err(|_| Error::msg(format!("`{}` is not a valid TLS server name", url.host)))?;

    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| Error::msg(format!("TLS handshake with {} failed: {e}", url.host)))?;

    Ok(Connection::Tls(Box::new(tls)))
}

/// The TLS configuration, built once and shared.
///
/// Trust anchors come from webpki-roots rather than the OS store, so behaviour
/// is identical on a developer's laptop and in a scratch container.
///
/// The key exchange groups come from the provider chosen in `Cargo.toml`, and
/// that choice is the one security decision in this function: with
/// `prefer-post-quantum`, X25519MLKEM768 leads the list, so its key share goes
/// out in the first ClientHello rather than costing a HelloRetryRequest. A
/// server that does not know the group ignores it and picks X25519, so nothing
/// is lost against one that has not caught up.
fn tls_connector() -> tokio_rustls::TlsConnector {
    use std::sync::OnceLock;
    static CONNECTOR: OnceLock<tokio_rustls::TlsConnector> = OnceLock::new();

    CONNECTOR
        .get_or_init(|| {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            tokio_rustls::TlsConnector::from(Arc::new(config))
        })
        .clone()
}

/// Read a complete response: status line, headers, then the body.
async fn read_response(connection: &mut Connection, max_body: usize) -> Result<ClientResponse> {
    let mut buffer = Vec::with_capacity(8 * 1024);

    let head_end = loop {
        if let Some(at) = find_head_end(&buffer) {
            break at;
        }
        if !fill(connection, &mut buffer).await? {
            return Err(Error::Protocol("the server closed before sending headers".into()));
        }
        if buffer.len() > 256 * 1024 {
            return Err(Error::Protocol("response headers are too large".into()));
        }
    };

    let (status, headers) = parse_head(&buffer[..head_end])?;
    let mut body = buffer.split_off(head_end);

    if headers.get("transfer-encoding").is_some_and(|te| te.contains("chunked")) {
        body = read_chunked(connection, body, max_body).await?;
    } else if let Some(length) = headers.content_length() {
        if length > max_body {
            return Err(Error::Protocol("response body is too large".into()));
        }
        while body.len() < length {
            if !fill_into(connection, &mut body).await? {
                return Err(Error::Protocol("response body ended early".into()));
            }
        }
        body.truncate(length);
    } else {
        // No length and no chunking: the body runs until the connection closes,
        // which is why every request asks for `connection: close`.
        while fill_into(connection, &mut body).await? {
            if body.len() > max_body {
                return Err(Error::Protocol("response body is too large".into()));
            }
        }
    }

    Ok(ClientResponse { status, headers, body })
}

pub(crate) fn parse_head(head: &[u8]) -> Result<(Status, Headers)> {
    let text = std::str::from_utf8(head)
        .map_err(|_| Error::Protocol("response headers are not UTF-8".into()))?;
    let mut lines = text.split("\r\n");

    let status_line = lines.next().ok_or_else(|| Error::Protocol("empty response".into()))?;
    let code = status_line
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| Error::Protocol(format!("malformed status line: {status_line}")))?;

    let mut headers = Headers::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.append(name.trim(), value.trim());
        }
    }

    Ok((Status(code), headers))
}

async fn read_chunked(
    connection: &mut Connection,
    mut buffer: Vec<u8>,
    max_body: usize,
) -> Result<Vec<u8>> {
    let mut body = Vec::new();

    loop {
        let line_end = loop {
            if let Some(at) = find_crlf(&buffer) {
                break at;
            }
            if !fill_into(connection, &mut buffer).await? {
                return Err(Error::Protocol("chunked body ended early".into()));
            }
        };

        let header: Vec<u8> = buffer.drain(..line_end + 2).collect();
        let size_text = String::from_utf8_lossy(&header[..line_end]);
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| Error::Protocol("invalid chunk size".into()))?;

        if size == 0 {
            return Ok(body);
        }
        if body.len() + size > max_body {
            return Err(Error::Protocol("response body is too large".into()));
        }

        while buffer.len() < size + 2 {
            if !fill_into(connection, &mut buffer).await? {
                return Err(Error::Protocol("chunked body ended early".into()));
            }
        }
        body.extend(buffer.drain(..size));
        buffer.drain(..2);
    }
}

async fn fill(connection: &mut Connection, buffer: &mut Vec<u8>) -> Result<bool> {
    fill_into(connection, buffer).await
}

async fn fill_into(connection: &mut Connection, buffer: &mut Vec<u8>) -> Result<bool> {
    let mut chunk = [0u8; 8192];
    let read = connection.read(&mut chunk).await.map_err(Error::Io)?;
    buffer.extend_from_slice(&chunk[..read]);
    Ok(read > 0)
}

pub(crate) fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n").map(|at| at + 4)
}

fn find_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|w| w == b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_request_line_and_headers() {
        let client = Client::new();
        let builder = client
            .post("https://example.com/v1/things?x=1")
            .bearer("secret")
            .json(Json::object([("name", "widget".into())]));

        let wire = String::from_utf8(builder.wire(&Url::parse(builder.url()).unwrap())).unwrap();

        assert!(wire.starts_with("POST /v1/things?x=1 HTTP/1.1\r\n"));
        assert!(wire.contains("host: example.com\r\n"));
        assert!(wire.contains("authorization: Bearer secret\r\n"));
        assert!(wire.contains("content-type: application/json\r\n"));
        assert!(wire.contains("content-length: 17\r\n"));
        assert!(wire.ends_with("\r\n\r\n{\"name\":\"widget\"}"));
    }

    #[test]
    fn parses_a_response_head() {
        let head = b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n";
        let (status, headers) = parse_head(head).unwrap();

        assert_eq!(status, Status::CREATED);
        assert_eq!(headers.content_type(), Some("application/json"));
        assert_eq!(headers.content_length(), Some(2));
    }

    #[test]
    fn a_failed_status_becomes_an_error_carrying_the_body() {
        let response = ClientResponse {
            status: Status(429),
            headers: Headers::new(),
            body: b"{\"error\":\"rate limited\"}".to_vec(),
        };

        let error = response.error_for_status().unwrap_err().to_string();
        assert!(error.contains("429"));
        assert!(error.contains("rate limited"));
    }

    #[test]
    fn only_transport_failures_are_retried() {
        assert!(is_retryable(&Error::msg("connecting to x timed out")));
        assert!(is_retryable(&Error::msg("cannot connect to x: Connection refused (os error 61)")));
        assert!(!is_retryable(&Error::msg("HTTP 500 Internal Server Error: boom")));
    }

    #[tokio::test]
    async fn talks_to_a_real_server_over_plain_http() {
        // The framework's own server answers this, which is the most honest
        // end-to-end check available without the network.
        use rustlavel_http::{Request, Response, Router, Server};
        use rustlavel_core::Context;

        let mut router = Router::new();
        router.post("/echo", |mut req: Request| async move {
            Response::json(Json::object([
                ("saw", Json::from(req.input("name").unwrap_or_default())),
                ("agent", Json::from(req.header("user-agent").unwrap_or("").to_string())),
            ]))
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let server = Server::new(router, Context::default());
        tokio::spawn(async move {
            let _ = server.listen(address.to_string()).await;
        });
        // Give the listener a moment to bind before the client dials it.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let response = Client::new()
            .post(format!("http://{address}/echo"))
            .json(Json::object([("name", "ada".into())]))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();

        let body = response.json().unwrap();
        assert_eq!(body.get("saw").unwrap().as_str(), Some("ada"));
        assert!(body.get("agent").unwrap().as_str().unwrap().starts_with("rustlavel/"));
    }

    #[tokio::test]
    async fn a_connection_failure_is_reported_clearly() {
        let error = Client::new()
            .timeout(Duration::from_millis(500))
            .get("http://127.0.0.1:1/nope")
            .send()
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("127.0.0.1:1"), "{error}");
    }

    /// The one property of the TLS setup worth a test.
    ///
    /// Only the key exchange is at risk from a quantum computer, and it is at
    /// risk *today*: an observer can record a handshake now and decrypt it once
    /// the machine exists. Everything else in TLS — the symmetric cipher, the
    /// certificate signature — either survives Grover comfortably or matters
    /// only while the connection is live.
    ///
    /// So this asserts the hybrid group is offered, and that it is offered
    /// first. Position is not cosmetic: rustls sends a key share only for the
    /// leading groups, and a hybrid group listed last is one the server can
    /// reach only by asking for a second round trip that most will not bother
    /// with. Switching the provider back to `ring` silently loses this, which
    /// is exactly the kind of regression a test should catch.
    #[test]
    fn the_key_exchange_leads_with_a_post_quantum_hybrid() {
        // Built exactly the way `tls_connector` builds it, so this exercises the
        // real resolution — `builder()` picking a provider from the crate
        // features — rather than a provider named here.
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();

        let offered: Vec<String> = config
            .crypto_provider()
            .kx_groups
            .iter()
            .map(|group| format!("{:?}", group.name()))
            .collect();

        assert_eq!(
            offered.first().map(String::as_str),
            Some("X25519MLKEM768"),
            "the post-quantum hybrid must lead the ClientHello; offered: {offered:?}"
        );
        assert!(
            offered.iter().any(|name| name == "X25519"),
            "a classical group must remain, for servers that do not know the hybrid: {offered:?}"
        );
    }
}
