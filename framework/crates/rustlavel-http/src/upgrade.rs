//! Handing a connection over to another protocol.
//!
//! HTTP/1.1 lets a client ask to stop speaking HTTP and start speaking
//! something else — that is how WebSocket begins. A handler answers `101` and
//! attaches an [`Upgrade`]; the server then stops managing the socket and hands
//! it over.

use crate::handler::BoxFuture;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

/// The socket, after HTTP is finished with it.
///
/// Boxed so the upgraded protocol does not have to name the concrete stream
/// type, which differs between a plain and (later) a TLS connection.
pub struct Upgraded {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    /// Bytes the client sent after the request but before the upgrade
    /// completed. Dropping these loses the first frame.
    pub buffered: Vec<u8>,
}

/// What takes over the connection.
pub trait Upgrade: Send + Sync + 'static {
    fn run(&self, connection: Upgraded) -> BoxFuture<()>;
}

impl<F, Fut> Upgrade for F
where
    F: Fn(Upgraded) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    fn run(&self, connection: Upgraded) -> BoxFuture<()> {
        Box::pin(self(connection))
    }
}

/// An upgrade attached to a response, shared so the response stays cloneable.
pub type Upgrader = Arc<dyn Upgrade>;
