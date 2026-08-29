//! The HTTP/1.1 server: accept loop, request parsing, keep-alive, and a
//! graceful shutdown that lets in-flight requests finish.

use crate::error_page;
use crate::headers::Headers;
use crate::method::Method;
use crate::panic;
use crate::request::Request;
use crate::response::Response;
use crate::router::Router;
use crate::status::Status;
use crate::url;
use rustlavel_core::{Context, Error, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::{TcpListener, TcpStream};

/// Guard rails applied to every connection.
#[derive(Debug, Clone)]
pub struct Limits {
    pub max_header_bytes: usize,
    pub max_body_bytes: usize,
    /// How long an idle keep-alive connection is held open.
    pub keep_alive_timeout: Duration,
    /// How long the headers of a single request may take to arrive.
    pub header_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_header_bytes: 64 * 1024,
            max_body_bytes: 10 * 1024 * 1024,
            keep_alive_timeout: Duration::from_secs(15),
            header_timeout: Duration::from_secs(10),
        }
    }
}

pub struct Server {
    router: Arc<Router>,
    context: Context,
    limits: Limits,
}

impl Server {
    pub fn new(mut router: Router, context: Context) -> Self {
        router.finalize();
        let limits = Limits {
            max_body_bytes: context.config().int("server.max_body_bytes", 10 * 1024 * 1024) as usize,
            ..Limits::default()
        };
        Server { router: Arc::new(router), context, limits }
    }

    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Bind and serve until Ctrl-C, then drain in-flight requests.
    pub async fn listen(self, addr: impl Into<String>) -> Result<()> {
        let addr = addr.into();
        let listener = TcpListener::bind(&addr).await.map_err(Error::Io)?;
        let local = listener.local_addr().map_err(Error::Io)?;

        panic::install_hook();
        error_page::set_debug(self.context.config().debug());

        rustlavel_core::info!("Rustlavel serving on http://{local}");
        rustlavel_core::info!("Press Ctrl-C to stop");

        let in_flight = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(self);

        loop {
            let accepted = tokio::select! {
                result = listener.accept() => result,
                _ = tokio::signal::ctrl_c() => break,
            };

            let (stream, peer) = match accepted {
                Ok(pair) => pair,
                // A single failed accept (fd exhaustion, a dropped SYN) should
                // not bring down the listener.
                Err(e) => {
                    rustlavel_core::warn!("accept failed: {e}");
                    continue;
                }
            };

            let server = Arc::clone(&shared);
            let counter = Arc::clone(&in_flight);
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                if let Err(e) = server.serve_connection(stream, peer).await {
                    rustlavel_core::debug!("connection closed: {e}");
                }
                counter.fetch_sub(1, Ordering::SeqCst);
            });
        }

        rustlavel_core::info!("Shutting down, waiting for in-flight requests…");
        let deadline = Instant::now() + Duration::from_secs(10);
        while in_flight.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        rustlavel_core::info!("Goodbye.");
        Ok(())
    }

    async fn serve_connection(&self, stream: TcpStream, peer: SocketAddr) -> Result<()> {
        // Small responses should leave for the client immediately.
        let _ = stream.set_nodelay(true);
        let (mut reader, writer) = stream.into_split();
        let mut writer = BufWriter::new(writer);
        let mut buffer: Vec<u8> = Vec::with_capacity(2048);

        loop {
            let head = match self.read_head(&mut reader, &mut buffer).await? {
                Some(head) => head,
                // Client hung up between requests: a clean end, not an error.
                None => return Ok(()),
            };

            let (mut request, keep_alive) = match self.parse(&head, &mut reader, &mut buffer, peer).await {
                Ok(parsed) => parsed,
                Err(error) => {
                    let response = Response::new(Status::BAD_REQUEST).with_text(error.to_string());
                    writer.write_all(&response.to_bytes(true)).await.map_err(Error::Io)?;
                    writer.flush().await.map_err(Error::Io)?;
                    return Ok(());
                }
            };

            request.context = self.context.clone();
            let is_head = request.method() == Method::Head;
            let response = self.dispatch(request).await;

            let mut response = response;
            if !keep_alive {
                response.headers.set("connection", "close");
            }
            writer.write_all(&response.to_bytes(!is_head)).await.map_err(Error::Io)?;
            writer.flush().await.map_err(Error::Io)?;

            if !keep_alive {
                return Ok(());
            }
        }
    }

    /// Run the router, converting a panic into the error page instead of
    /// letting it kill the connection task.
    async fn dispatch(&self, request: Request) -> Response {
        let started = Instant::now();
        let method = request.method();
        let path = request.path().to_string();

        // Panics are caught, and the request event is dispatched, inside the
        // router — so both behave identically under the test client.
        let response = self.router.dispatch(request).await;
        let elapsed = started.elapsed();

        if rustlavel_core::log::enabled(rustlavel_core::log::Level::Debug) {
            rustlavel_core::debug!(
                "{method} {path} → {} ({:.1}ms)",
                response.status.code(),
                elapsed.as_secs_f64() * 1000.0
            );
        }

        response
    }

    /// Read until the end of the header block, returning the raw head.
    async fn read_head(
        &self,
        reader: &mut tokio::net::tcp::OwnedReadHalf,
        buffer: &mut Vec<u8>,
    ) -> Result<Option<Vec<u8>>> {
        // A connection waiting for its first byte gets the longer keep-alive
        // budget; once bytes arrive the head must complete promptly.
        let mut timeout = self.limits.keep_alive_timeout;

        loop {
            if let Some(end) = find_head_end(buffer) {
                let head = buffer[..end].to_vec();
                buffer.drain(..end);
                return Ok(Some(head));
            }
            if buffer.len() > self.limits.max_header_bytes {
                return Err(Error::Protocol("request headers are too large".into()));
            }

            let mut chunk = [0u8; 4096];
            let read = match tokio::time::timeout(timeout, reader.read(&mut chunk)).await {
                Ok(Ok(0)) if buffer.is_empty() => return Ok(None),
                Ok(Ok(0)) => return Err(Error::Protocol("connection closed mid-request".into())),
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Err(Error::Io(e)),
                Err(_) if buffer.is_empty() => return Ok(None),
                Err(_) => return Err(Error::Protocol("timed out reading request headers".into())),
            };
            buffer.extend_from_slice(&chunk[..read]);
            timeout = self.limits.header_timeout;
        }
    }

    async fn parse(
        &self,
        head: &[u8],
        reader: &mut tokio::net::tcp::OwnedReadHalf,
        buffer: &mut Vec<u8>,
        peer: SocketAddr,
    ) -> Result<(Request, bool)> {
        let text = std::str::from_utf8(head).map_err(|_| Error::Protocol("headers are not UTF-8".into()))?;
        let mut lines = text.split("\r\n");

        let request_line = lines.next().ok_or_else(|| Error::Protocol("empty request".into()))?;
        let mut parts = request_line.split(' ');
        let method = parts
            .next()
            .and_then(Method::parse)
            .ok_or_else(|| Error::Protocol("unsupported method".into()))?;
        let target = parts.next().ok_or_else(|| Error::Protocol("missing request target".into()))?;
        let version = parts.next().unwrap_or("HTTP/1.1");

        let mut headers = Headers::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| Error::Protocol(format!("malformed header line: {line}")))?;
            headers.append(name.trim(), value.trim());
        }

        // An absolute-form target (`GET http://host/path`) is legal for proxies.
        let target = match target.find("://") {
            Some(scheme_end) => match target[scheme_end + 3..].find('/') {
                Some(path_start) => &target[scheme_end + 3 + path_start..],
                None => "/",
            },
            None => target,
        };

        let body = self.read_body(&headers, reader, buffer).await?;

        let keep_alive = match headers.get("connection") {
            Some(value) if value.eq_ignore_ascii_case("close") => false,
            Some(value) if value.eq_ignore_ascii_case("keep-alive") => true,
            _ => version != "HTTP/1.0",
        };

        let (path, query) = url::split_target(target);
        let mut request = Request::new(method, target);
        request.path = url::decode(path);
        request.query = url::parse_query(query);
        request.headers = headers;
        request.peer = Some(peer);
        Ok((request.with_body(body), keep_alive))
    }

    async fn read_body(
        &self,
        headers: &Headers,
        reader: &mut tokio::net::tcp::OwnedReadHalf,
        buffer: &mut Vec<u8>,
    ) -> Result<Vec<u8>> {
        if headers.get("transfer-encoding").is_some_and(|te| te.contains("chunked")) {
            return self.read_chunked_body(reader, buffer).await;
        }

        let Some(length) = headers.content_length() else {
            return Ok(Vec::new());
        };
        if length > self.limits.max_body_bytes {
            return Err(Error::Protocol("request body is too large".into()));
        }

        while buffer.len() < length {
            let mut chunk = vec![0u8; (length - buffer.len()).min(64 * 1024)];
            let read = tokio::time::timeout(self.limits.header_timeout, reader.read(&mut chunk))
                .await
                .map_err(|_| Error::Protocol("timed out reading request body".into()))?
                .map_err(Error::Io)?;
            if read == 0 {
                return Err(Error::Protocol("request body ended early".into()));
            }
            buffer.extend_from_slice(&chunk[..read]);
        }

        Ok(buffer.drain(..length).collect())
    }

    async fn read_chunked_body(
        &self,
        reader: &mut tokio::net::tcp::OwnedReadHalf,
        buffer: &mut Vec<u8>,
    ) -> Result<Vec<u8>> {
        let mut body = Vec::new();

        loop {
            // Each chunk starts with its size in hex on its own line.
            let line_end = loop {
                if let Some(at) = find_crlf(buffer) {
                    break at;
                }
                if !fill(reader, buffer, self.limits.header_timeout).await? {
                    return Err(Error::Protocol("chunked body ended early".into()));
                }
            };

            let header: Vec<u8> = buffer.drain(..line_end + 2).collect();
            let size_text = String::from_utf8_lossy(&header[..line_end]);
            let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| Error::Protocol("invalid chunk size".into()))?;

            if size == 0 {
                // The final chunk may be followed by trailer lines; both end at
                // a blank line.
                loop {
                    let end = loop {
                        if let Some(at) = find_crlf(buffer) {
                            break at;
                        }
                        if !fill(reader, buffer, self.limits.header_timeout).await? {
                            return Ok(body);
                        }
                    };
                    buffer.drain(..end + 2);
                    if end == 0 {
                        return Ok(body);
                    }
                }
            }

            if body.len() + size > self.limits.max_body_bytes {
                return Err(Error::Protocol("request body is too large".into()));
            }

            while buffer.len() < size + 2 {
                if !fill(reader, buffer, self.limits.header_timeout).await? {
                    return Err(Error::Protocol("chunked body ended early".into()));
                }
            }
            body.extend(buffer.drain(..size));
            buffer.drain(..2);
        }
    }
}

async fn fill(
    reader: &mut tokio::net::tcp::OwnedReadHalf,
    buffer: &mut Vec<u8>,
    timeout: Duration,
) -> Result<bool> {
    let mut chunk = [0u8; 4096];
    let read = tokio::time::timeout(timeout, reader.read(&mut chunk))
        .await
        .map_err(|_| Error::Protocol("timed out reading request body".into()))?
        .map_err(Error::Io)?;
    buffer.extend_from_slice(&chunk[..read]);
    Ok(read > 0)
}

/// Byte offset just past the blank line that ends the header block.
fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n").map(|at| at + 4)
}

fn find_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|w| w == b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_end_of_a_header_block() {
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r\nbody"), Some(18));
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n"), None);
    }

    #[tokio::test]
    async fn parses_a_request_with_a_body() {
        let server = Server::new(Router::new(), Context::default());
        let head = b"POST /users?page=2 HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 14\r\n\r\n";

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(br#"{"name":"ada"}"#).await.unwrap();
        });
        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut reader, _writer) = stream.into_split();

        let mut buffer = Vec::new();
        let (mut request, keep_alive) =
            server.parse(head, &mut reader, &mut buffer, addr).await.unwrap();

        assert_eq!(request.method(), Method::Post);
        assert_eq!(request.path(), "/users");
        assert_eq!(request.query("page"), Some("2"));
        assert_eq!(request.header("host"), Some("localhost"));
        assert_eq!(request.input("name").as_deref(), Some("ada"));
        assert!(keep_alive);
    }

    #[tokio::test]
    async fn http_1_0_closes_by_default() {
        let server = Server::new(Router::new(), Context::default());
        let head = b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n";

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let (mut reader, _w) = TcpStream::connect(addr).await.unwrap().into_split();

        let mut buffer = Vec::new();
        let (_request, keep_alive) =
            server.parse(head, &mut reader, &mut buffer, addr).await.unwrap();

        assert!(!keep_alive);
    }

    #[tokio::test]
    async fn rejects_a_body_larger_than_the_limit() {
        let mut server = Server::new(Router::new(), Context::default());
        server.limits.max_body_bytes = 8;
        let head = b"POST / HTTP/1.1\r\nContent-Length: 9999\r\n\r\n";

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let (mut reader, _w) = TcpStream::connect(addr).await.unwrap().into_split();

        let mut buffer = Vec::new();
        let error = server.parse(head, &mut reader, &mut buffer, addr).await.unwrap_err();

        assert!(error.to_string().contains("too large"));
    }
}
