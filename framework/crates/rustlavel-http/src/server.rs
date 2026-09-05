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

/// How many ports to try before giving up.
///
/// Ten outside production, because a port held by a process that has not
/// finished dying is a daily nuisance there and nothing depends on the number.
/// **One inside it**, because in production something *does* depend on the
/// number: a load balancer, a health check, a firewall rule. A server that
/// moved itself to 8301 while the traffic still went to 8300 would look like an
/// outage with no cause in the logs. Refusing to start says what happened.
///
/// `server.port_attempts` overrides both, in either direction.
fn port_attempts(config: &rustlavel_core::Config) -> u16 {
    let default = if config.is_production() { 1 } else { 10 };
    config.int("server.port_attempts", default).clamp(1, 1000) as u16
}

/// Refuse a message whose end has more than one answer.
///
/// These checks are the difference between a proxy and this server agreeing
/// about where one request stops and the next begins. When they disagree, the
/// bytes left in the connection buffer are read as the start of the next
/// request — so an attacker writes a prefix onto a pooled connection and
/// captures or rewrites whatever victim's request arrives after it. The session
/// and CSRF work above this is no defence: the request that reaches the router
/// was never the one the visitor sent.
///
/// RFC 7230 §3.3.3 says refuse, and refusing is cheap.
fn check_framing(headers: &Headers) -> Result<()> {
        //
    // These checks are the difference between a proxy and this server
    // agreeing about where one request stops and the next begins. When they
    // disagree, the bytes left in the connection buffer are read as the
    // start of the next request — so an attacker writes a prefix onto a
    // pooled connection and captures or rewrites whatever victim's request
    // arrives after it. The session and CSRF work above this is no defence:
    // the request that reaches the router was never the one the visitor
    // sent.
    //
    // RFC 7230 §3.3.3 says refuse, and refusing is cheap.
    let lengths = headers.get_all("content-length");
    if lengths.len() > 1 && lengths.iter().any(|value| value != &lengths[0]) {
        return Err(Error::Protocol(
            "more than one Content-Length, and they disagree".into(),
        ));
    }
    if !lengths.is_empty() && headers.get("transfer-encoding").is_some() {
        return Err(Error::Protocol(
            "both Transfer-Encoding and Content-Length: a message may say where it ends \
             once, not twice"
                .into(),
        ));
    }
    // Only `chunked`, and only as the last encoding, tells us where the
    // body ends. Anything else — a second `Transfer-Encoding` line, an
    // encoding this server does not implement — leaves the length unknown,
    // and guessing is what a smuggled request relies on.
    let encodings = headers.get_all("transfer-encoding");
    if !encodings.is_empty() {
        let listed: Vec<&str> = encodings
            .iter()
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect();
        if listed.last() != Some(&"chunked")
            || listed.iter().filter(|value| **value == "chunked").count() != 1
        {
            return Err(Error::Protocol(format!(
                "unsupported Transfer-Encoding: {}",
                listed.join(", ")
            )));
        }
    }
    Ok(())
}

/// Bind `addr`, walking up the port number while the one asked for is taken.
///
/// A port left occupied by a process that has not finished dying is the most
/// common way a run fails, and the operating system's answer to it — `Os {
/// code: 48, kind: AddrInUse }` — tells somebody nothing they can act on. So
/// the next few ports are tried, and the one actually used is said out loud.
///
/// **Saying it out loud is the whole safety argument.** A server that quietly
/// moves is worse than one that refuses to start: the load balancer still
/// points at the old port, the health check fails, and nothing in the logs
/// explains why. That is why production defaults to a single attempt — see
/// [`Server::listen`] — and why moving is a warning rather than a debug line.
///
/// Only `AddrInUse` moves on. A refused bind for any other reason — a
/// privileged port, an address that is not on this machine — is that reason,
/// and walking away from it would only make it harder to see.
async fn bind_walking(addr: &str, attempts: u16) -> Result<TcpListener> {
    // Port zero means "any free port"; the operating system is already doing
    // this job, and incrementing from zero would undo it.
    let Some((host, first)) = addr
        .rsplit_once(':')
        .and_then(|(host, port)| port.parse::<u16>().ok().map(|port| (host, port)))
        .filter(|(_, port)| *port != 0)
    else {
        return TcpListener::bind(addr).await.map_err(Error::Io);
    };

    let mut tried = first;
    for offset in 0..attempts {
        let Some(port) = first.checked_add(offset) else { break };
        tried = port;
        match TcpListener::bind(format!("{host}:{port}")).await {
            Ok(listener) => {
                if offset > 0 {
                    rustlavel_core::warn!(
                        "port {first} is in use, so this is serving on {port} instead"
                    );
                }
                return Ok(listener);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(e) => return Err(Error::Io(e)),
        }
    }

    Err(Error::msg(if first == tried {
        format!(
            "port {first} is already in use. Something else is listening on it — `lsof -i :{first}` \
             says what — so stop that, or set SERVER_PORT to a free port."
        )
    } else {
        format!(
            "every port from {first} to {tried} is already in use. Stop whatever is holding them \
             — `lsof -i :{first}` names the first — or set SERVER_PORT to a free one."
        )
    }))
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
    ///
    /// When the port is taken, the next ones are tried — see [`bind_walking`].
    /// How many is `server.port_attempts`, which defaults to ten outside
    /// production and to one inside it.
    pub async fn listen(self, addr: impl Into<String>) -> Result<()> {
        let addr = addr.into();
        let listener = bind_walking(&addr, port_attempts(self.context.config())).await?;
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
            let mut response = self.dispatch(request).await;

            // A handler that answered 101 wants the socket. Write the
            // handshake, then stop speaking HTTP on this connection.
            if let Some(upgrade) = response.take_upgrade() {
                writer.write_all(&response.to_bytes(false)).await.map_err(Error::Io)?;
                writer.flush().await.map_err(Error::Io)?;

                let upgraded = crate::upgrade::Upgraded {
                    reader: Box::new(reader),
                    writer: Box::new(writer),
                    // Anything already read past the request belongs to the new
                    // protocol; dropping it would lose its first frame.
                    buffered: std::mem::take(&mut buffer),
                };
                upgrade.run(upgraded).await;
                return Ok(());
            }

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

            // `Content-Length : 5` is not a header with the name
            // `Content-Length`. RFC 7230 §3.2.4 requires it be rejected, and
            // the reason is this file's problem specifically: a front-end that
            // trims where we trim, or does not, disagrees with us about the
            // body's length — which is the whole of request smuggling.
            if name.ends_with(' ') || name.ends_with('\t') {
                return Err(Error::Protocol(
                    "a header name may not be followed by whitespace before the colon".into(),
                ));
            }
            headers.append(name.trim(), value.trim());
        }

        check_framing(&headers)?;

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
        // Exact, not `contains`: `xchunked` is not chunked, and the parse
        // above has already refused anything whose last encoding is not
        // `chunked`.
        if headers.get("transfer-encoding").is_some() {
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


    /// Parse just the headers of a raw message and run the framing checks over
    /// them, which is what `parse` does before it reads a body.
    fn framing_of(raw: &str) -> Result<()> {
        let mut headers = Headers::new();
        for line in raw.split("\r\n").skip(1) {
            if line.is_empty() {
                break;
            }
            let (name, value) = line.split_once(':').expect("a header line");
            if name.ends_with(' ') || name.ends_with('\t') {
                return Err(Error::Protocol("whitespace before the colon".into()));
            }
            headers.append(name.trim(), value.trim());
        }
        check_framing(&headers)
    }

    /// Request smuggling, in the four shapes this server used to accept.
    ///
    /// Each one is a message whose end has two answers. A front-end that picks
    /// the other answer leaves bytes in this connection's buffer, and the
    /// keep-alive loop reads them as the next request — so an attacker prefixes
    /// a request onto a pooled connection and captures or rewrites whatever
    /// arrives next. None of the session or CSRF work above this helps: the
    /// request the router sees was never the one the visitor sent.
    #[test]
    fn a_message_that_says_where_it_ends_twice_is_refused() {
        let ambiguous = [
            (
                "two lengths that disagree",
                "POST / HTTP/1.1\r\nhost: x\r\ncontent-length: 6\r\ncontent-length: 0\r\n\r\nsmuggl",
            ),
            (
                "a length and a chunked encoding",
                "POST / HTTP/1.1\r\nhost: x\r\ncontent-length: 6\r\ntransfer-encoding: chunked\r\n\r\n0\r\n\r\n",
            ),
            (
                "an encoding that is not chunked last",
                "POST / HTTP/1.1\r\nhost: x\r\ntransfer-encoding: chunked, identity\r\n\r\n0\r\n\r\n",
            ),
            (
                "whitespace before the colon",
                "POST / HTTP/1.1\r\nhost: x\r\ncontent-length : 6\r\n\r\nsmuggl",
            ),
        ];

        for (what, raw) in ambiguous {
            assert!(
                framing_of(raw).is_err(),
                "{what}: accepted a message with two answers for where its body ends"
            );
        }
    }

    /// And the ordinary shapes still parse, or the fix would be a denial of
    /// service dressed as a security patch.
    #[test]
    fn an_unambiguous_message_still_parses() {
        for raw in [
            "GET / HTTP/1.1\r\nhost: x\r\n\r\n",
            "POST / HTTP/1.1\r\nhost: x\r\ncontent-length: 3\r\n\r\nabc",
            "POST / HTTP/1.1\r\nhost: x\r\ntransfer-encoding: chunked\r\n\r\n0\r\n\r\n",
            // Repeated but agreeing is legal, and some proxies do it.
            "POST / HTTP/1.1\r\nhost: x\r\ncontent-length: 3\r\ncontent-length: 3\r\n\r\nabc",
        ] {
            assert!(framing_of(raw).is_ok(), "refused an ordinary message: {raw:?}");
        }
    }

    /// Production must not wander. This is the check that the default is not
    /// quietly the same everywhere — the dangerous outcome is silent.
    #[test]
    fn production_gets_one_attempt_and_development_gets_more() {
        use rustlavel_core::Config;

        let production = Config::with_defaults();
        production.set("app.env", "production");
        assert_eq!(port_attempts(&production), 1);

        let local = Config::with_defaults();
        local.set("app.env", "local");
        assert!(port_attempts(&local) > 1);
    }

    /// And somebody who wants the other behaviour can say so.
    #[test]
    fn the_setting_overrides_the_environment_both_ways() {
        use rustlavel_core::Config;

        let production = Config::with_defaults();
        production.set("app.env", "production");
        production.set("server.port_attempts", "5");
        assert_eq!(port_attempts(&production), 5);

        let local = Config::with_defaults();
        local.set("app.env", "local");
        local.set("server.port_attempts", "1");
        assert_eq!(port_attempts(&local), 1);
    }

    /// The reason this exists: a port left occupied should cost a warning, not
    /// a failed run.
    #[tokio::test]
    async fn a_taken_port_moves_to_the_next_one() {
        let held = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken = held.local_addr().unwrap().port();

        let listener = bind_walking(&format!("127.0.0.1:{taken}"), 10).await.unwrap();
        assert_ne!(listener.local_addr().unwrap().port(), taken);
        assert!(listener.local_addr().unwrap().port() > taken);
    }

    /// One attempt is what production asks for, and it must actually mean one:
    /// a server that moves without being allowed to is the failure this whole
    /// feature has to avoid.
    #[tokio::test]
    async fn a_single_attempt_does_not_move() {
        let held = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken = held.local_addr().unwrap().port();

        let error = bind_walking(&format!("127.0.0.1:{taken}"), 1).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains(&taken.to_string()), "{message}");
        assert!(message.contains("lsof"), "the message has to say what to do: {message}");
    }

    /// Port zero already means "any free port". Walking up from it would turn
    /// a working request into a scan of the low ports.
    #[tokio::test]
    async fn port_zero_is_left_to_the_operating_system() {
        let listener = bind_walking("127.0.0.1:0", 10).await.unwrap();
        assert_ne!(listener.local_addr().unwrap().port(), 0);
    }

    /// Running out of ports has to name the range actually tried, or the
    /// message sends somebody looking at the wrong port.
    #[tokio::test]
    async fn exhausting_the_range_says_what_it_tried() {
        // Hold three consecutive ports, then allow exactly those three.
        let first = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let start = first.local_addr().unwrap().port();
        let mut held = vec![first];
        for offset in 1..3u16 {
            // Another test may hold it; the assertion below still holds.
            if let Ok(listener) = TcpListener::bind(format!("127.0.0.1:{}", start + offset)).await {
                held.push(listener);
            }
        }

        let error = bind_walking(&format!("127.0.0.1:{start}"), 3).await;
        if let Err(error) = error {
            let message = error.to_string();
            assert!(message.contains(&start.to_string()), "{message}");
            assert!(message.contains(&(start + 2).to_string()), "{message}");
        }
    }
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
