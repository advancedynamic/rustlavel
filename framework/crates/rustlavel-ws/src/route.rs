//! Mounting a socket on the router.
//!
//! ```ignore
//! r.get("/ws", websocket(|mut socket, _request| async move {
//!     while let Some(message) = socket.recv().await {
//!         let _ = socket.send(message).await;
//!     }
//! }));
//! ```
//!
//! That is the whole echo server. The handshake, the 101, the framing, the
//! pongs and the close are all behind `websocket`; the application writes an
//! async closure that owns the socket for as long as the client is connected.

use crate::connection::{WebSocket, WebSocketConfig};
use crate::handshake;
use rustlavel_http::handler::BoxFuture;
use rustlavel_http::{Handler, IntoResponse, Request, Response, Upgrade, Upgraded};
use rustlavel_core::{Event, events};
use std::sync::{Arc, Mutex};

/// A route that answers the WebSocket handshake and hands the socket to
/// `handler`.
///
/// The closure receives the [`Request`] as well, because everything a socket
/// needs to know about who is connecting — the session, a query parameter, the
/// authenticated user attached by middleware — lives on it and is gone once the
/// connection stops being HTTP.
pub fn websocket<F, Fut>(handler: F) -> WebSocketRoute<F>
where
    F: Fn(WebSocket, Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    WebSocketRoute {
        handler: Arc::new(handler),
        config: WebSocketConfig::default(),
        protocols: Vec::new(),
    }
}

pub struct WebSocketRoute<F> {
    handler: Arc<F>,
    config: WebSocketConfig,
    protocols: Vec<String>,
}

impl<F> WebSocketRoute<F> {
    /// Limits and timers for sockets opened on this route.
    pub fn with_config(mut self, config: WebSocketConfig) -> Self {
        self.config = config;
        self
    }

    /// The subprotocols this route understands, most preferred first.
    pub fn protocols(mut self, protocols: &[&str]) -> Self {
        self.protocols = protocols.iter().map(|p| (*p).to_string()).collect();
        self
    }
}

impl<F, Fut> Handler for WebSocketRoute<F>
where
    F: Fn(WebSocket, Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    fn call(&self, request: Request) -> BoxFuture<Response> {
        let handshake = match handshake::negotiate(&request, &self.protocols) {
            Ok(handshake) => handshake,
            // A malformed handshake is an ordinary HTTP failure, and stays one:
            // the connection is never upgraded, so the client gets a 400 body
            // it can actually read.
            Err(error) => return Box::pin(async move { error.into_response() }),
        };

        let response = handshake::response(&handshake);
        let upgrade = SocketUpgrade {
            handler: Arc::clone(&self.handler),
            config: self.config.clone(),
            path: request.path().to_string(),
            protocol: handshake.protocol.clone(),
            // The request is moved through to the closure. It cannot be cloned
            // and `Upgrade::run` only has `&self`, so it waits in a cell that
            // the single upgrade takes it out of.
            request: Mutex::new(Some(request)),
        };

        Box::pin(async move { response.upgrading(upgrade) })
    }
}

struct SocketUpgrade<F> {
    handler: Arc<F>,
    config: WebSocketConfig,
    path: String,
    protocol: Option<String>,
    request: Mutex<Option<Request>>,
}

impl<F, Fut> Upgrade for SocketUpgrade<F>
where
    F: Fn(WebSocket, Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    fn run(&self, connection: Upgraded) -> BoxFuture<()> {
        let taken = self.request.lock().ok().and_then(|mut slot| slot.take());
        let Some(request) = taken else {
            // Only reachable if the server ran one upgrade twice.
            return Box::pin(async {});
        };

        let socket = WebSocket::new(connection, self.config.clone())
            .on_path(self.path.clone())
            .with_protocol(self.protocol.clone());

        if events::has_subscribers() {
            Event::new("ws.connected")
                .with("path", self.path.as_str())
                .with("protocol", self.protocol.clone())
                .dispatch();
        }

        let handler = Arc::clone(&self.handler);
        Box::pin(async move { handler(socket, request).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::WebSocketConfig;
    use crate::frame::{Frame, OpCode, Role};
    use crate::message::Message;
    use rustlavel_http::{Method, Router, Status};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn handshake_request(target: &str) -> Request {
        Request::new(Method::Get, target)
            .with_header("upgrade", "websocket")
            .with_header("connection", "Upgrade")
            .with_header("sec-websocket-version", "13")
            .with_header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
    }

    fn echo_router() -> Router {
        let mut router = Router::new();
        router.get(
            "/ws",
            websocket(|mut socket: WebSocket, _request: Request| async move {
                while let Some(message) = socket.recv().await {
                    if socket.send(message).await.is_err() {
                        break;
                    }
                }
            }),
        );
        router.finalize();
        router
    }

    #[tokio::test]
    async fn a_valid_handshake_answers_101_and_takes_the_connection() {
        let response = echo_router().dispatch(handshake_request("/ws")).await;

        assert_eq!(response.status, Status(101));
        assert_eq!(
            response.headers.get("sec-websocket-accept"),
            Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        );
        assert!(response.upgrades(), "the response must claim the socket");
    }

    #[tokio::test]
    async fn a_handshake_missing_a_header_answers_400_and_keeps_the_connection() {
        let mut request = handshake_request("/ws");
        request.headers_mut().remove("sec-websocket-version");

        let response = echo_router().dispatch(request).await;

        assert_eq!(response.status, Status::BAD_REQUEST);
        assert!(!response.upgrades());
        assert!(response.body_string().contains("Sec-WebSocket-Version"));
    }

    #[tokio::test]
    async fn the_route_negotiates_a_subprotocol() {
        let mut router = Router::new();
        router.get(
            "/ws",
            websocket(|_socket: WebSocket, _request: Request| async {}).protocols(&["rustlavel"]),
        );
        router.finalize();

        let request = handshake_request("/ws").with_header("sec-websocket-protocol", "rustlavel");
        let response = router.dispatch(request).await;

        assert_eq!(response.headers.get("sec-websocket-protocol"), Some("rustlavel"));
    }

    /// The upgrade a route attaches, built directly.
    ///
    /// `Response::take_upgrade` is crate-private to `rustlavel-http` — the
    /// server is its only caller — so a test outside that crate cannot pull the
    /// upgrade back out of a dispatched response. Constructing the same value
    /// exercises the same code the server runs.
    fn upgrade_for<F, Fut>(handler: F, request: Request) -> SocketUpgrade<F>
    where
        F: Fn(WebSocket, Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        SocketUpgrade {
            handler: Arc::new(handler),
            config: WebSocketConfig { idle_timeout: None, ..WebSocketConfig::default() },
            path: request.path().to_string(),
            protocol: None,
            request: Mutex::new(Some(request)),
        }
    }

    async fn run_over_a_pipe(
        upgrade: impl Upgrade,
        buffered: Vec<u8>,
    ) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<()>) {
        let (client, server) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(server);
        let running = tokio::spawn(async move {
            upgrade
                .run(Upgraded { reader: Box::new(reader), writer: Box::new(writer), buffered })
                .await;
        });
        (client, running)
    }

    async fn read_one(client: &mut tokio::io::DuplexStream) -> Frame {
        let mut received = vec![0u8; 256];
        let read = client.read(&mut received).await.unwrap();
        Frame::decode(&received[..read], Role::Client, 1024).unwrap().unwrap().0
    }

    #[tokio::test]
    async fn an_echo_handler_answers_over_the_upgraded_connection() {
        // The route claims the socket…
        assert!(echo_router().dispatch(handshake_request("/ws")).await.upgrades());

        // …and this is what it does with it.
        let upgrade = upgrade_for(
            |mut socket: WebSocket, _request: Request| async move {
                while let Some(message) = socket.recv().await {
                    if socket.send(message).await.is_err() {
                        break;
                    }
                }
            },
            handshake_request("/ws"),
        );

        let (mut client, running) = run_over_a_pipe(upgrade, Vec::new()).await;

        let masked = Frame::client(OpCode::Text, b"ping me".to_vec(), [1, 2, 3, 4]);
        client.write_all(&masked.encode()).await.unwrap();

        let frame = read_one(&mut client).await;
        assert_eq!(
            Message::from_data(frame.opcode, frame.payload).unwrap(),
            Message::text("ping me")
        );

        drop(client);
        running.await.unwrap();
    }

    #[tokio::test]
    async fn the_handler_receives_the_request_that_opened_the_socket() {
        let upgrade = upgrade_for(
            |mut socket: WebSocket, request: Request| async move {
                // Everything about who is connecting lives on the request, and
                // is gone once the connection stops being HTTP.
                let room = request.query("room").unwrap_or("lobby").to_string();
                let _ = socket.send(Message::text(room)).await;
            },
            handshake_request("/ws?room=kitchen"),
        );

        let (mut client, _running) = run_over_a_pipe(upgrade, Vec::new()).await;

        assert_eq!(read_one(&mut client).await.payload, b"kitchen");
    }

    #[tokio::test]
    async fn opening_a_socket_is_reported_to_instrumentation() {
        // `events` is process-wide, so this test owns the subscriber list for
        // its duration and clears it again on the way out.
        let _guard = crate::testing::events_lock();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        events::subscribe(move |event: &Event| {
            // Other tests open sockets on this same process-wide bus, so the
            // path this test alone uses is what tells its events apart.
            if event.field("path").and_then(|value| value.as_str()) == Some("/ws-instrumented") {
                sink.lock().unwrap_or_else(|p| p.into_inner()).push((event.kind, event.fields.clone()));
            }
        });

        let upgrade = upgrade_for(
            |mut socket: WebSocket, _request: Request| async move {
                while socket.recv().await.is_some() {}
            },
            handshake_request("/ws-instrumented"),
        );
        let early = Frame::client(OpCode::Text, b"hi".to_vec(), [9, 9, 9, 9]).encode();
        let (client, running) = run_over_a_pipe(upgrade, early).await;

        drop(client);
        running.await.unwrap();

        let kinds: Vec<&str> = seen.lock().unwrap_or_else(|p| p.into_inner()).iter().map(|(kind, _)| *kind).collect();
        assert!(kinds.contains(&"ws.connected"), "{kinds:?}");
        assert!(kinds.contains(&"ws.message"), "{kinds:?}");
        assert!(kinds.contains(&"ws.disconnected"), "{kinds:?}");

        let recorded = seen.lock().unwrap_or_else(|p| p.into_inner());
        let message = recorded.iter().find(|(kind, _)| *kind == "ws.message").unwrap();
        assert_eq!(message.1.get("kind").and_then(|v| v.as_str()), Some("text"));
        assert_eq!(message.1.get("bytes").and_then(|v| v.as_i64()), Some(2));
        // The payload itself is never recorded.
        assert!(!message.1.values().any(|value| value.as_str() == Some("hi")));
    }
}
