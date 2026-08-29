//! What an application sends and receives.
//!
//! A [`Message`] is a *whole* message: fragmentation is a transport detail that
//! [`crate::connection::WebSocket`] reassembles before anything reaches here.
//!
//! `Ping`, `Pong` and `Close` carry their payloads because the protocol
//! requires it — a pong must echo the ping's application data back byte for
//! byte, and a close frame that drops its status code tells the peer nothing.

use crate::error::{WsError, WsResult};
use crate::frame::{CloseCode, CloseFrame, Frame, OpCode};
use rustlavel_core::Json;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    /// `None` when the peer closed without a status code, which is legal.
    Close(Option<CloseFrame>),
}

impl Message {
    pub fn text(body: impl Into<String>) -> Message {
        Message::Text(body.into())
    }

    /// A JSON message. The broadcasting protocol is built entirely out of these.
    pub fn json(value: &Json) -> Message {
        Message::Text(value.to_string())
    }

    pub fn close(code: CloseCode, reason: impl Into<String>) -> Message {
        Message::Close(Some(CloseFrame::new(code, reason)))
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Message::Text(body) => Some(body),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Message::Text(body) => body.as_bytes(),
            Message::Binary(bytes) | Message::Ping(bytes) | Message::Pong(bytes) => bytes,
            Message::Close(_) => &[],
        }
    }

    /// Text or binary — the messages an application actually cares about.
    pub fn is_data(&self) -> bool {
        matches!(self, Message::Text(_) | Message::Binary(_))
    }

    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A stable label for instrumentation. Never the payload itself: a socket
    /// carrying chat or auth tokens must not leak them into Telescope.
    pub fn kind(&self) -> &'static str {
        match self {
            Message::Text(_) => "text",
            Message::Binary(_) => "binary",
            Message::Ping(_) => "ping",
            Message::Pong(_) => "pong",
            Message::Close(_) => "close",
        }
    }

    /// The frame a server writes for this message: final, and never masked.
    pub fn into_frame(self) -> Frame {
        match self {
            Message::Text(body) => Frame::server(OpCode::Text, body.into_bytes()),
            Message::Binary(bytes) => Frame::server(OpCode::Binary, bytes),
            Message::Ping(bytes) => Frame::server(OpCode::Ping, bytes),
            Message::Pong(bytes) => Frame::server(OpCode::Pong, bytes),
            Message::Close(close) => Frame::server(
                OpCode::Close,
                close.map(|close| close.to_payload()).unwrap_or_default(),
            ),
        }
    }

    /// Turn a reassembled payload into a message.
    ///
    /// Text is validated here rather than at the frame layer, because a message
    /// split across fragments is only valid UTF-8 once every fragment has
    /// arrived — a multi-byte character is allowed to straddle the boundary.
    pub fn from_data(opcode: OpCode, payload: Vec<u8>) -> WsResult<Message> {
        match opcode {
            OpCode::Text => {
                String::from_utf8(payload).map(Message::Text).map_err(|_| WsError::InvalidUtf8)
            }
            OpCode::Binary => Ok(Message::Binary(payload)),
            other => Err(WsError::protocol(format!("{other:?} is not a data frame"))),
        }
    }
}

impl From<String> for Message {
    fn from(body: String) -> Message {
        Message::Text(body)
    }
}

impl From<&str> for Message {
    fn from(body: &str) -> Message {
        Message::Text(body.to_string())
    }
}

impl From<Json> for Message {
    fn from(value: Json) -> Message {
        Message::Text(value.to_string())
    }
}

impl From<Vec<u8>> for Message {
    fn from(bytes: Vec<u8>) -> Message {
        Message::Binary(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_message_becomes_an_unmasked_text_frame() {
        let frame = Message::text("hi").into_frame();

        assert_eq!(frame.opcode, OpCode::Text);
        assert!(frame.fin);
        assert_eq!(frame.mask, None, "a server must never mask");
        assert_eq!(frame.encode(), b"\x81\x02hi");
    }

    #[test]
    fn a_close_message_encodes_its_code_and_reason() {
        let frame = Message::close(CloseCode::POLICY, "no").into_frame();
        assert_eq!(frame.encode(), b"\x88\x04\x03\xf0no");

        // Closing with no status at all is legal and sends an empty payload.
        assert_eq!(Message::Close(None).into_frame().encode(), b"\x88\x00");
    }

    #[test]
    fn reassembled_text_must_be_valid_utf8() {
        assert_eq!(
            Message::from_data(OpCode::Text, "héllo".as_bytes().to_vec()).unwrap(),
            Message::text("héllo")
        );

        // A lone continuation byte: the classic 1007 case.
        let error = Message::from_data(OpCode::Text, vec![0xf0, 0x28, 0x8c, 0x28]).unwrap_err();
        assert_eq!(error.close_code(), CloseCode::INVALID_PAYLOAD);
    }

    #[test]
    fn binary_payloads_are_passed_through_untouched() {
        let bytes = vec![0x00, 0xff, 0x80];
        assert_eq!(
            Message::from_data(OpCode::Binary, bytes.clone()).unwrap(),
            Message::Binary(bytes)
        );
    }

    #[test]
    fn instrumentation_sees_a_kind_and_a_size_but_never_the_payload() {
        let message = Message::text("a secret token");

        assert_eq!(message.kind(), "text");
        assert_eq!(message.len(), 14);
        assert!(message.is_data());
        assert!(!Message::Ping(Vec::new()).is_data());
    }

    #[test]
    fn json_values_convert_straight_into_messages() {
        let value = Json::object([("event", Json::from("ping"))]);
        assert_eq!(Message::json(&value).as_text(), Some(r#"{"event":"ping"}"#));
    }
}
