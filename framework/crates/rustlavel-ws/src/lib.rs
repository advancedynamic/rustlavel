//! rustlavel-ws: WebSockets and broadcasting, written from scratch on Tokio.
//!
//! Laravel answers real-time with Echo on the client and Reverb on the server.
//! This is both halves in one crate: an RFC 6455 implementation — handshake,
//! framing, control frames, close handshake — and a [`Broadcaster`] with
//! public, private and presence channels on top of it.
//!
//! # An echo server
//!
//! ```ignore
//! use rustlavel_ws::{websocket, WebSocket};
//!
//! r.get("/ws", websocket(|mut socket, _request| async move {
//!     while let Some(message) = socket.recv().await {
//!         let _ = socket.send(message).await;
//!     }
//! }));
//! ```
//!
//! `recv` yields whole [`Message`]s. Fragments are reassembled, pings are
//! answered, text is checked for UTF-8, and a close is replied to with a close —
//! none of which an application should have to remember.
//!
//! # Broadcasting
//!
//! ```ignore
//! use rustlavel_ws::Broadcaster;
//!
//! let broadcaster = Broadcaster::new()
//!     .authorize(|request: &Request, channel: &str| {
//!         request.extension::<User>().is_some_and(|user| user.may_watch(channel))
//!     });
//!
//! r.get("/broadcasting", broadcaster.route());
//!
//! // From anywhere: a controller, a queued job, a background task.
//! broadcaster.broadcast("orders", "order.created", Json::object([("id", 7.into())]));
//! ```
//!
//! The wire protocol clients speak is documented on [`broadcast`].
//!
//! # What is reported
//!
//! `ws.connected`, `ws.message`, `ws.disconnected` and `broadcast.sent` go to
//! `rustlavel_core::events`, carrying the channel, counts and durations — and
//! never a payload. A socket's contents are the application's business.

pub mod base64;
pub mod broadcast;
pub mod connection;
pub mod error;
pub mod frame;
pub mod handshake;
pub mod message;
pub mod route;

pub use broadcast::{Authorizer, Broadcaster, ChannelKind, Member, SubscriberId, authorize_async};
pub use connection::{Sender, WebSocket, WebSocketConfig, channel};
pub use error::{WsError, WsResult};
pub use frame::{CloseCode, CloseFrame, Frame, OpCode, Role};
pub use handshake::{Handshake, HandshakeError, accept_key};
pub use message::Message;
pub use route::{WebSocketRoute, websocket};

#[cfg(test)]
mod testing;
