//! What can go wrong once a socket is speaking WebSocket.
//!
//! Every variant knows which RFC 6455 close code it should be reported with,
//! because the protocol's contract is that a connection is never just dropped:
//! the peer is told *why* it is going away.

use crate::frame::CloseCode;

pub type WsResult<T> = std::result::Result<T, WsError>;

#[derive(Debug)]
pub enum WsError {
    /// The peer broke the framing rules: a reserved bit set, an unmasked client
    /// frame, a continuation that started nothing.
    Protocol(String),
    /// A frame or a reassembled message exceeded the configured limit.
    TooLarge { size: usize, limit: usize },
    /// A text frame whose payload was not valid UTF-8. RFC 6455 is explicit
    /// that this closes with 1007 rather than being passed to the application.
    InvalidUtf8,
    /// The peer stopped answering pings within the idle timeout.
    Idle,
    /// The peer's send queue is full: it is not reading fast enough to keep up.
    /// The only safe answer is to drop it, so this is a distinct variant rather
    /// than an I/O error.
    Full,
    /// The connection is already closed, so there is nothing to send on.
    Closed,
    Io(std::io::Error),
}

impl WsError {
    pub fn protocol(message: impl Into<String>) -> Self {
        WsError::Protocol(message.into())
    }

    /// The close code this failure should be reported to the peer with.
    pub fn close_code(&self) -> CloseCode {
        match self {
            WsError::Protocol(_) => CloseCode::PROTOCOL_ERROR,
            WsError::TooLarge { .. } => CloseCode::TOO_LARGE,
            WsError::InvalidUtf8 => CloseCode::INVALID_PAYLOAD,
            WsError::Idle => CloseCode::GOING_AWAY,
            WsError::Full => CloseCode::POLICY,
            WsError::Closed | WsError::Io(_) => CloseCode::ABNORMAL,
        }
    }
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsError::Protocol(message) => write!(f, "websocket protocol error: {message}"),
            WsError::TooLarge { size, limit } => {
                write!(f, "message of {size} bytes exceeds the {limit} byte limit")
            }
            WsError::InvalidUtf8 => f.write_str("text frame payload was not valid UTF-8"),
            WsError::Idle => f.write_str("peer stopped answering pings"),
            WsError::Full => f.write_str("peer is not reading fast enough"),
            WsError::Closed => f.write_str("the connection is closed"),
            WsError::Io(e) => write!(f, "websocket i/o failed: {e}"),
        }
    }
}

impl std::error::Error for WsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WsError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for WsError {
    fn from(e: std::io::Error) -> Self {
        WsError::Io(e)
    }
}

/// So a handler that talks to a socket can still use `?` alongside the rest of
/// the framework's error type.
impl From<WsError> for rustlavel_core::Error {
    fn from(e: WsError) -> Self {
        match e {
            WsError::Io(io) => rustlavel_core::Error::Io(io),
            other => rustlavel_core::Error::Protocol(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_maps_to_the_close_code_the_rfc_names() {
        assert_eq!(WsError::protocol("nope").close_code(), CloseCode::PROTOCOL_ERROR);
        assert_eq!(WsError::InvalidUtf8.close_code(), CloseCode::INVALID_PAYLOAD);
        assert_eq!(WsError::TooLarge { size: 9, limit: 4 }.close_code(), CloseCode::TOO_LARGE);
        assert_eq!(WsError::Idle.close_code(), CloseCode::GOING_AWAY);
    }

    #[test]
    fn oversize_says_both_the_size_and_the_limit() {
        let message = WsError::TooLarge { size: 2048, limit: 1024 }.to_string();
        assert!(message.contains("2048"), "{message}");
        assert!(message.contains("1024"), "{message}");
    }
}
