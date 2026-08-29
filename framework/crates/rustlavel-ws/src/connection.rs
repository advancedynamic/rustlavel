//! A live WebSocket connection.
//!
//! [`WebSocket`] owns the read half and drives everything the protocol requires
//! but an application should never have to think about: pongs are sent for
//! pings, fragments are reassembled, text is validated as UTF-8, oversize
//! messages are refused, and a close is answered with a close.
//!
//! Writing happens in a separate task fed by a bounded queue. That split is
//! what makes broadcasting possible: a [`Sender`] can be cloned and handed to
//! anything that wants to push to this client, while `recv` keeps exclusive
//! ownership of the reader. It is also what makes backpressure expressible —
//! the queue has a fixed depth, and a client that will not drain it is dropped
//! rather than allowed to grow the server's memory.

use crate::error::{WsError, WsResult};
use crate::frame::{CloseCode, CloseFrame, Frame, OpCode, Role};
use crate::message::Message;
use rustlavel_core::{Config, Event, events};
use rustlavel_http::Upgraded;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// Limits and timers for one socket.
#[derive(Debug, Clone)]
pub struct WebSocketConfig {
    /// The largest reassembled message that will be accepted.
    pub max_message_size: usize,
    /// The largest single frame that will be accepted. Checked against the
    /// declared length, so an absurd announcement costs no memory.
    pub max_frame_size: usize,
    /// How long a connection may be silent before it is pinged. A second
    /// silent period with no pong closes it. `None` disables the check.
    pub idle_timeout: Option<Duration>,
    /// How long to wait for the peer's half of the close handshake.
    pub close_timeout: Duration,
    /// Depth of the outgoing queue. This is the backpressure budget: a client
    /// this far behind is dropped.
    pub send_queue: usize,
}

impl Default for WebSocketConfig {
    fn default() -> Self {
        WebSocketConfig {
            max_message_size: 1 << 20,
            max_frame_size: 1 << 20,
            idle_timeout: Some(Duration::from_secs(60)),
            close_timeout: Duration::from_secs(5),
            send_queue: 64,
        }
    }
}

impl WebSocketConfig {
    /// Read the limits from the application's configuration, so an operator can
    /// raise them in `.env` without recompiling.
    ///
    /// `websocket.idle_timeout` is in seconds; zero turns the check off.
    pub fn from_config(config: &Config) -> Self {
        let defaults = WebSocketConfig::default();
        let idle = config.int("websocket.idle_timeout", 60).max(0) as u64;

        WebSocketConfig {
            max_message_size: config
                .int("websocket.max_message_size", defaults.max_message_size as i64)
                .max(1) as usize,
            max_frame_size: config
                .int("websocket.max_frame_size", defaults.max_frame_size as i64)
                .max(1) as usize,
            idle_timeout: (idle > 0).then(|| Duration::from_secs(idle)),
            close_timeout: Duration::from_secs(
                config.int("websocket.close_timeout", 5).max(1) as u64
            ),
            send_queue: config.int("websocket.send_queue", defaults.send_queue as i64).max(1)
                as usize,
        }
    }
}

/// A cloneable handle for pushing messages at one client.
///
/// Handed to the broadcaster, to background tasks, to anything that produces
/// events. It never blocks the reader, and it can outlive the socket: once the
/// connection is gone every clone reports closed.
#[derive(Clone)]
pub struct Sender {
    queue: mpsc::Sender<Message>,
}

impl Sender {
    /// Queue a message, waiting for room. Use this from the connection's own
    /// task, where waiting is what you want.
    pub async fn send(&self, message: impl Into<Message>) -> WsResult<()> {
        self.queue.send(message.into()).await.map_err(|_| WsError::Closed)
    }

    /// Queue a message only if there is room right now.
    ///
    /// This is the shape broadcasting needs: fan-out must never wait on the
    /// slowest subscriber, so a full queue is a failure the caller acts on by
    /// dropping the subscriber.
    pub fn try_send(&self, message: impl Into<Message>) -> WsResult<()> {
        match self.queue.try_send(message.into()) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(WsError::Full),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(WsError::Closed),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.queue.is_closed()
    }

    /// How many more messages fit before this client counts as too slow.
    pub fn room(&self) -> usize {
        self.queue.capacity()
    }
}

/// A sender with no socket behind it, for tests and for anything that wants to
/// collect what would have been written.
pub fn channel(capacity: usize) -> (Sender, mpsc::Receiver<Message>) {
    let (queue, receiver) = mpsc::channel(capacity.max(1));
    (Sender { queue }, receiver)
}

pub struct WebSocket {
    reader: Box<dyn AsyncRead + Send + Unpin>,
    /// Bytes read but not yet framed. Seeded with whatever the HTTP server had
    /// already buffered past the request — dropping those loses the first frame.
    buffer: Vec<u8>,
    outgoing: Sender,
    config: WebSocketConfig,
    /// The opcode and bytes of a message still arriving in fragments.
    partial: Option<(OpCode, Vec<u8>)>,
    awaiting_pong: bool,
    closed: bool,
    close_frame: Option<CloseFrame>,
    path: String,
    protocol: Option<String>,
    opened_at: Instant,
    received: u64,
}

impl WebSocket {
    /// Take over a connection the HTTP server has finished with.
    pub fn new(upgraded: Upgraded, config: WebSocketConfig) -> WebSocket {
        let (queue, receiver) = mpsc::channel(config.send_queue.max(1));
        tokio::spawn(write_loop(upgraded.writer, receiver));

        WebSocket {
            reader: upgraded.reader,
            buffer: upgraded.buffered,
            outgoing: Sender { queue },
            config,
            partial: None,
            awaiting_pong: false,
            closed: false,
            close_frame: None,
            path: String::new(),
            protocol: None,
            opened_at: Instant::now(),
            received: 0,
        }
    }

    /// Label the socket for instrumentation with the route it was opened on.
    pub fn on_path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    pub fn with_protocol(mut self, protocol: Option<String>) -> Self {
        self.protocol = protocol;
        self
    }

    /// A handle other tasks can push messages through.
    pub fn sender(&self) -> Sender {
        self.outgoing.clone()
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// The subprotocol agreed during the handshake, if any.
    pub fn protocol(&self) -> Option<&str> {
        self.protocol.as_deref()
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// How the connection ended, once it has. Available after `recv` returns
    /// `None`, which is where an application usually wants to log it.
    pub fn close_frame(&self) -> Option<&CloseFrame> {
        self.close_frame.as_ref()
    }

    /// How many data messages have arrived.
    pub fn received(&self) -> u64 {
        self.received
    }

    pub async fn send(&mut self, message: impl Into<Message>) -> WsResult<()> {
        if self.closed {
            return Err(WsError::Closed);
        }
        self.outgoing.send(message).await
    }

    /// The next message from the peer, or `None` once the connection is over.
    ///
    /// Only `Text` and `Binary` come out of here. Pings, pongs and closes are
    /// answered internally, because every application would have to write the
    /// same three replies and one of them would get it wrong.
    pub async fn recv(&mut self) -> Option<Message> {
        loop {
            if self.closed {
                return None;
            }

            let frame = match self.next_frame().await {
                Ok(Some(frame)) => frame,
                // The socket ended without a close frame: nothing to reply to.
                Ok(None) => {
                    self.mark_closed(Some(CloseFrame::new(CloseCode::ABNORMAL, "")));
                    return None;
                }
                Err(error) => {
                    self.fail(error).await;
                    return None;
                }
            };

            // Any frame at all proves the peer is alive, not just a pong.
            self.awaiting_pong = false;

            match frame.opcode {
                OpCode::Ping => {
                    // The pong must carry the ping's application data back
                    // unchanged; that is how the peer matches them up.
                    if self.outgoing.send(Message::Pong(frame.payload)).await.is_err() {
                        self.mark_closed(Some(CloseFrame::new(CloseCode::ABNORMAL, "")));
                        return None;
                    }
                }
                OpCode::Pong => {}
                OpCode::Close => {
                    let peer = match CloseFrame::parse(&frame.payload) {
                        Ok(close) => close,
                        Err(error) => {
                            self.fail(error).await;
                            return None;
                        }
                    };
                    // The other half of the close handshake: echo the status
                    // back so the peer knows the close was seen, not lost.
                    let echo =
                        peer.clone().unwrap_or_else(|| CloseFrame::new(CloseCode::NORMAL, ""));
                    let _ = self.outgoing.send(Message::Close(Some(echo))).await;
                    self.mark_closed(
                        peer.or_else(|| Some(CloseFrame::new(CloseCode::NO_STATUS, ""))),
                    );
                    return None;
                }
                OpCode::Text | OpCode::Binary => {
                    if self.partial.is_some() {
                        self.fail(WsError::protocol(
                            "a new data frame arrived while a fragmented message was still open",
                        ))
                        .await;
                        return None;
                    }
                    if frame.fin {
                        return self.deliver(frame.opcode, frame.payload).await;
                    }
                    self.partial = Some((frame.opcode, frame.payload));
                }
                OpCode::Continuation => {
                    let Some((opcode, mut assembled)) = self.partial.take() else {
                        self.fail(WsError::protocol(
                            "a continuation frame arrived with no message to continue",
                        ))
                        .await;
                        return None;
                    };

                    let total = assembled.len() + frame.payload.len();
                    if total > self.config.max_message_size {
                        // Checked as fragments arrive, not at the end: otherwise
                        // a peer could spend the whole limit one frame at a time.
                        self.fail(WsError::TooLarge {
                            size: total,
                            limit: self.config.max_message_size,
                        })
                        .await;
                        return None;
                    }

                    assembled.extend_from_slice(&frame.payload);
                    if frame.fin {
                        return self.deliver(opcode, assembled).await;
                    }
                    self.partial = Some((opcode, assembled));
                }
            }
        }
    }

    /// Close the connection politely: send a close frame, then wait — briefly —
    /// for the peer's.
    pub async fn close(&mut self, code: CloseCode, reason: impl Into<String>) {
        if self.closed {
            return;
        }
        let close = CloseFrame::new(code, reason);

        if self.outgoing.send(Message::Close(Some(close.clone()))).await.is_err() {
            self.mark_closed(Some(close));
            return;
        }

        // Frames the peer already sent are drained rather than reset, which is
        // what stops a clean close from looking like a crash on the other end.
        // Bounded, because a peer that never answers must not pin this task.
        let peer = tokio::time::timeout(self.config.close_timeout, self.drain_until_close())
            .await
            .ok()
            .flatten();
        self.mark_closed(peer.or(Some(close)));
    }

    /// Hand a reassembled payload to the application.
    async fn deliver(&mut self, opcode: OpCode, payload: Vec<u8>) -> Option<Message> {
        match Message::from_data(opcode, payload) {
            Ok(message) => {
                self.received += 1;
                if events::has_subscribers() {
                    // Size and kind only. A socket carrying chat, tokens or
                    // order data must not spill its payloads into Telescope.
                    Event::new("ws.message")
                        .with("path", self.path.as_str())
                        .with("kind", message.kind())
                        .with("bytes", message.len())
                        .dispatch();
                }
                Some(message)
            }
            Err(error) => {
                self.fail(error).await;
                None
            }
        }
    }

    /// Report a protocol failure to the peer and stop.
    async fn fail(&mut self, error: WsError) {
        let close = CloseFrame::new(error.close_code(), error.to_string());
        let _ = self.outgoing.send(Message::Close(Some(close.clone()))).await;
        self.mark_closed(Some(close));
    }

    fn mark_closed(&mut self, close: Option<CloseFrame>) {
        self.closed = true;
        if self.close_frame.is_none() {
            self.close_frame = close;
        }
    }

    async fn drain_until_close(&mut self) -> Option<CloseFrame> {
        loop {
            match self.read_frame().await {
                Ok(Some(frame)) if frame.opcode == OpCode::Close => {
                    return CloseFrame::parse(&frame.payload).ok().flatten();
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return None,
            }
        }
    }

    /// One frame, with the idle timer wrapped around it.
    async fn next_frame(&mut self) -> WsResult<Option<Frame>> {
        let Some(idle) = self.config.idle_timeout else {
            return self.read_frame().await;
        };

        loop {
            // Cancelling the read is safe: bytes already read live in
            // `self.buffer`, and a read that had not completed consumed nothing.
            match tokio::time::timeout(idle, self.read_frame()).await {
                Ok(result) => return result,
                Err(_) if self.awaiting_pong => return Err(WsError::Idle),
                Err(_) => {
                    // Silent for a whole period: prod the peer once. A TCP
                    // connection to a machine that has vanished stays "open"
                    // for a long time, so silence has to be tested, not assumed.
                    self.awaiting_pong = true;
                    if self.outgoing.send(Message::Ping(Vec::new())).await.is_err() {
                        return Ok(None);
                    }
                }
            }
        }
    }

    /// One frame, reading more bytes until the buffer holds a whole one.
    /// `Ok(None)` means the peer closed the socket underneath us.
    async fn read_frame(&mut self) -> WsResult<Option<Frame>> {
        loop {
            if let Some((frame, used)) =
                Frame::decode(&self.buffer, Role::Server, self.config.max_frame_size)?
            {
                self.buffer.drain(..used);
                return Ok(Some(frame));
            }

            let mut chunk = [0u8; 8192];
            let read = self.reader.read(&mut chunk).await?;
            if read == 0 {
                return Ok(None);
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

/// Dispatched here so every socket reports exactly once, however it ended —
/// including the ones an application drops without draining.
impl Drop for WebSocket {
    fn drop(&mut self) {
        if !events::has_subscribers() {
            return;
        }
        let code = self.close_frame.as_ref().map_or(CloseCode::ABNORMAL, |close| close.code);

        Event::new("ws.disconnected")
            .with("path", self.path.as_str())
            .with("messages", self.received)
            .with("code", code.code())
            .took(self.opened_at.elapsed())
            .dispatch();
    }
}

/// Serialize everything queued for this client onto the socket.
///
/// One task per connection means writes are ordered and never interleave two
/// half-written frames, which is exactly the bug an unsynchronised broadcaster
/// would produce.
async fn write_loop(
    mut writer: Box<dyn AsyncWrite + Send + Unpin>,
    mut queue: mpsc::Receiver<Message>,
) {
    while let Some(message) = queue.recv().await {
        let closing = matches!(message, Message::Close(_));
        let bytes = message.into_frame().encode();

        if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
            break;
        }
        if closing {
            // Nothing may follow a close frame.
            break;
        }
    }

    // Closing the queue makes every surviving `Sender` clone — the broadcaster
    // holds some — report the client as gone instead of quietly filling up.
    queue.close();
    let _ = writer.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::DuplexStream;

    /// A fixed mask, so the bytes a test writes are reproducible.
    const MASK: [u8; 4] = [0xa1, 0xb2, 0xc3, 0xd4];

    /// The other end of an in-memory socket: no ports, no sockets, no sharing
    /// between tests.
    struct Peer {
        stream: DuplexStream,
        buffer: Vec<u8>,
    }

    impl Peer {
        async fn send(&mut self, frame: Frame) {
            self.stream.write_all(&frame.encode()).await.unwrap();
        }

        /// A client frame: always masked.
        async fn send_masked(&mut self, opcode: OpCode, payload: &[u8]) {
            self.send(Frame::client(opcode, payload.to_vec(), MASK)).await;
        }

        async fn send_fragment(&mut self, opcode: OpCode, payload: &[u8], fin: bool) {
            self.send(Frame { fin, opcode, mask: Some(MASK), payload: payload.to_vec() }).await;
        }

        /// The next frame the server wrote. Panics if the socket ends first,
        /// which in a test is the failure we want to see.
        async fn recv(&mut self) -> Frame {
            loop {
                if let Some((frame, used)) =
                    Frame::decode(&self.buffer, Role::Client, 1 << 20).unwrap()
                {
                    self.buffer.drain(..used);
                    return frame;
                }
                let mut chunk = [0u8; 4096];
                let read = self.stream.read(&mut chunk).await.unwrap();
                assert!(read > 0, "the server closed without sending an expected frame");
                self.buffer.extend_from_slice(&chunk[..read]);
            }
        }

        async fn hang_up(&mut self) {
            self.stream.shutdown().await.unwrap();
        }
    }

    fn pair(config: WebSocketConfig) -> (WebSocket, Peer) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (reader, writer) = tokio::io::split(server);
        let upgraded = Upgraded {
            reader: Box::new(reader),
            writer: Box::new(writer),
            buffered: Vec::new(),
        };
        (WebSocket::new(upgraded, config), Peer { stream: client, buffer: Vec::new() })
    }

    /// The close frame the server sent, whatever else it sent first.
    async fn close_from_server(peer: &mut Peer) -> CloseFrame {
        loop {
            let frame = peer.recv().await;
            if frame.opcode == OpCode::Close {
                return CloseFrame::parse(&frame.payload).unwrap().unwrap();
            }
        }
    }

    #[tokio::test]
    async fn echoes_a_message_the_client_sent() {
        let (mut socket, mut peer) = pair(WebSocketConfig::default());

        peer.send_masked(OpCode::Text, b"hello").await;
        let message = socket.recv().await.unwrap();
        assert_eq!(message, Message::text("hello"));

        socket.send(message).await.unwrap();
        let echoed = peer.recv().await;
        assert_eq!(echoed.opcode, OpCode::Text);
        assert_eq!(echoed.mask, None, "a server must never mask");
        assert_eq!(echoed.payload, b"hello");
    }

    #[tokio::test]
    async fn bytes_already_buffered_by_the_http_server_are_not_lost() {
        let (client, server) = tokio::io::duplex(1024);
        let (reader, writer) = tokio::io::split(server);
        // The first frame arrived glued to the handshake request.
        let early = Frame::client(OpCode::Text, b"first".to_vec(), MASK).encode();

        let mut socket = WebSocket::new(
            Upgraded { reader: Box::new(reader), writer: Box::new(writer), buffered: early },
            WebSocketConfig::default(),
        );

        assert_eq!(socket.recv().await.unwrap(), Message::text("first"));
        drop(client);
    }

    #[tokio::test]
    async fn answers_a_ping_with_a_pong_carrying_the_same_payload() {
        let (mut socket, mut peer) = pair(WebSocketConfig::default());

        peer.send_masked(OpCode::Ping, b"keepalive").await;
        peer.send_masked(OpCode::Text, b"after").await;

        // The ping never surfaces; the message behind it does.
        assert_eq!(socket.recv().await.unwrap(), Message::text("after"));

        let pong = peer.recv().await;
        assert_eq!(pong.opcode, OpCode::Pong);
        assert_eq!(pong.payload, b"keepalive");
    }

    #[tokio::test]
    async fn reassembles_a_fragmented_message_around_an_interleaved_ping() {
        let (mut socket, mut peer) = pair(WebSocketConfig::default());

        // A control frame between two fragments is legal, and is the classic
        // place a hand-written implementation corrupts the message.
        peer.send_fragment(OpCode::Text, b"Hel", false).await;
        peer.send_masked(OpCode::Ping, b"mid").await;
        peer.send_fragment(OpCode::Continuation, b"lo ", false).await;
        peer.send_fragment(OpCode::Continuation, b"there", true).await;

        assert_eq!(socket.recv().await.unwrap(), Message::text("Hello there"));

        let pong = peer.recv().await;
        assert_eq!(pong.opcode, OpCode::Pong);
        assert_eq!(pong.payload, b"mid");
    }

    #[tokio::test]
    async fn a_continuation_with_nothing_to_continue_closes_with_1002() {
        let (mut socket, mut peer) = pair(WebSocketConfig::default());

        peer.send_fragment(OpCode::Continuation, b"orphan", true).await;

        assert!(socket.recv().await.is_none());
        assert_eq!(close_from_server(&mut peer).await.code, CloseCode::PROTOCOL_ERROR);
    }

    #[tokio::test]
    async fn a_text_frame_that_is_not_utf8_closes_with_1007() {
        let (mut socket, mut peer) = pair(WebSocketConfig::default());

        peer.send_masked(OpCode::Text, &[0xf0, 0x28, 0x8c, 0x28]).await;

        assert!(socket.recv().await.is_none());
        assert_eq!(close_from_server(&mut peer).await.code, CloseCode::INVALID_PAYLOAD);
        assert_eq!(socket.close_frame().unwrap().code, CloseCode::INVALID_PAYLOAD);
    }

    #[tokio::test]
    async fn an_unmasked_client_frame_closes_with_1002() {
        let (mut socket, mut peer) = pair(WebSocketConfig::default());

        // A server frame sent by a client: unmasked, and therefore illegal.
        peer.send(Frame::server(OpCode::Text, b"naked".to_vec())).await;

        assert!(socket.recv().await.is_none());
        let close = close_from_server(&mut peer).await;
        assert_eq!(close.code, CloseCode::PROTOCOL_ERROR);
        assert!(close.reason.contains("must be masked"), "{}", close.reason);
    }

    #[tokio::test]
    async fn an_oversize_frame_closes_with_1009() {
        let config = WebSocketConfig { max_frame_size: 64, ..WebSocketConfig::default() };
        let (mut socket, mut peer) = pair(config);

        peer.send_masked(OpCode::Binary, &[7u8; 200]).await;

        assert!(socket.recv().await.is_none());
        assert_eq!(close_from_server(&mut peer).await.code, CloseCode::TOO_LARGE);
    }

    #[tokio::test]
    async fn fragments_that_add_up_to_more_than_the_limit_close_with_1009() {
        let config = WebSocketConfig {
            max_message_size: 8,
            max_frame_size: 1024,
            ..WebSocketConfig::default()
        };
        let (mut socket, mut peer) = pair(config);

        // Each fragment is small; together they are over the limit.
        peer.send_fragment(OpCode::Binary, &[0u8; 5], false).await;
        peer.send_fragment(OpCode::Continuation, &[0u8; 5], true).await;

        assert!(socket.recv().await.is_none());
        assert_eq!(close_from_server(&mut peer).await.code, CloseCode::TOO_LARGE);
    }

    #[tokio::test]
    async fn a_close_from_the_peer_is_echoed_back() {
        let (mut socket, mut peer) = pair(WebSocketConfig::default());

        peer.send(Frame::client(
            OpCode::Close,
            CloseFrame::new(CloseCode::NORMAL, "bye").to_payload(),
            MASK,
        ))
        .await;

        assert!(socket.recv().await.is_none());

        let echo = close_from_server(&mut peer).await;
        assert_eq!(echo.code, CloseCode::NORMAL);
        assert_eq!(echo.reason, "bye");
        // The socket remembers how it ended, for the application's log line.
        assert_eq!(socket.close_frame().unwrap().code, CloseCode::NORMAL);
    }

    #[tokio::test]
    async fn closing_waits_for_the_peers_half_of_the_handshake() {
        let (mut socket, mut peer) = pair(WebSocketConfig::default());

        let reply = async {
            let frame = peer.recv().await;
            assert_eq!(frame.opcode, OpCode::Close);
            assert_eq!(
                CloseFrame::parse(&frame.payload).unwrap().unwrap().code,
                CloseCode::GOING_AWAY
            );
            peer.send(Frame::client(
                OpCode::Close,
                CloseFrame::new(CloseCode(4001), "acknowledged").to_payload(),
                MASK,
            ))
            .await;
        };

        tokio::join!(socket.close(CloseCode::GOING_AWAY, "restarting"), reply);

        assert!(socket.is_closed());
        // The peer's status wins: it is the more informative of the two.
        assert_eq!(socket.close_frame().unwrap().code, CloseCode(4001));
        assert!(socket.send(Message::text("late")).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_socket_is_pinged_and_then_closed() {
        let config = WebSocketConfig {
            idle_timeout: Some(Duration::from_secs(30)),
            ..WebSocketConfig::default()
        };
        let (mut socket, mut peer) = pair(config);

        let reading = tokio::spawn(async move {
            let message = socket.recv().await;
            (message, socket.close_frame().cloned())
        });

        // Nothing is sent, so the first quiet period produces a ping…
        let ping = peer.recv().await;
        assert_eq!(ping.opcode, OpCode::Ping);

        // …and the second, with no pong in between, ends the connection.
        assert_eq!(close_from_server(&mut peer).await.code, CloseCode::GOING_AWAY);

        let (message, close) = reading.await.unwrap();
        assert!(message.is_none());
        assert_eq!(close.unwrap().code, CloseCode::GOING_AWAY);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pong_keeps_an_otherwise_idle_socket_open() {
        let config = WebSocketConfig {
            idle_timeout: Some(Duration::from_secs(30)),
            ..WebSocketConfig::default()
        };
        let (mut socket, mut peer) = pair(config);

        let reading = tokio::spawn(async move { socket.recv().await });

        let ping = peer.recv().await;
        assert_eq!(ping.opcode, OpCode::Ping);
        peer.send_masked(OpCode::Pong, &ping.payload).await;
        peer.send_masked(OpCode::Text, b"still here").await;

        assert_eq!(reading.await.unwrap(), Some(Message::text("still here")));
    }

    #[tokio::test]
    async fn a_socket_that_just_ends_reports_an_abnormal_close() {
        let (mut socket, mut peer) = pair(WebSocketConfig::default());

        peer.hang_up().await;

        assert!(socket.recv().await.is_none());
        assert_eq!(socket.close_frame().unwrap().code, CloseCode::ABNORMAL);
    }

    #[tokio::test]
    async fn a_full_send_queue_is_reported_rather_than_waited_on() {
        let (sender, mut receiver) = channel(2);

        assert!(sender.try_send(Message::text("one")).is_ok());
        assert!(sender.try_send(Message::text("two")).is_ok());
        // The third has nowhere to go, and waiting for room is exactly what
        // broadcasting must not do.
        assert!(matches!(sender.try_send(Message::text("three")), Err(WsError::Full)));

        assert_eq!(receiver.recv().await, Some(Message::text("one")));
        assert!(sender.try_send(Message::text("three")).is_ok());
    }

    #[tokio::test]
    async fn senders_report_closed_once_the_socket_is_gone() {
        let (sender, receiver) = channel(2);
        drop(receiver);

        assert!(sender.is_closed());
        assert!(matches!(sender.try_send(Message::text("x")), Err(WsError::Closed)));
    }

    #[test]
    fn limits_come_from_the_application_configuration() {
        let config = Config::new();
        config.set("websocket.max_message_size", 4096);
        config.set("websocket.idle_timeout", 0);
        config.set("websocket.send_queue", 8);

        let settings = WebSocketConfig::from_config(&config);

        assert_eq!(settings.max_message_size, 4096);
        assert_eq!(settings.send_queue, 8);
        // Zero seconds means "never ping", which is what a test harness wants.
        assert_eq!(settings.idle_timeout, None);
        // Anything unset keeps the built-in default.
        assert_eq!(settings.max_frame_size, WebSocketConfig::default().max_frame_size);
    }
}
