//! RFC 6455 framing: the bytes on the wire.
//!
//! A frame is a two-byte header, an optional extended length, an optional
//! four-byte mask, and a payload:
//!
//! ```text
//!  0               1               2               3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-------+-+-------------+-------------------------------+
//! |F|R|R|R| opcode|M| Payload len |    Extended payload length    |
//! |I|S|S|S|  (4)  |A|     (7)     |             (16/64)           |
//! |N|V|V|V|       |S|             |   (if payload len==126/127)   |
//! | |1|2|3|       |K|             |                               |
//! +-+-+-+-+-------+-+-------------+ - - - - - - - - - - - - - - - +
//! |     Masking-key (if MASK set) |          Payload Data         |
//! +-------------------------------+-------------------------------+
//! ```
//!
//! Encoding and decoding are kept apart, the way the PostgreSQL driver keeps
//! them apart, so both halves can be checked against hand-written bytes.
//!
//! Two rules from the RFC drive most of the checks here, and both are
//! directional: a frame travelling **client → server must be masked**, and a
//! frame travelling **server → client must not be**. That is why [`Frame::decode`]
//! takes a [`Role`] rather than guessing.

use crate::error::{WsError, WsResult};

/// Which end of the connection is *reading*.
///
/// A server reads client frames (masked); a client reads server frames
/// (unmasked). Tests use both directions, which is the point of naming it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Server,
    Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpCode {
    Continuation,
    Text,
    Binary,
    Close,
    Ping,
    Pong,
}

impl OpCode {
    pub fn bits(self) -> u8 {
        match self {
            OpCode::Continuation => 0x0,
            OpCode::Text => 0x1,
            OpCode::Binary => 0x2,
            OpCode::Close => 0x8,
            OpCode::Ping => 0x9,
            OpCode::Pong => 0xa,
        }
    }

    pub fn from_bits(bits: u8) -> WsResult<OpCode> {
        match bits {
            0x0 => Ok(OpCode::Continuation),
            0x1 => Ok(OpCode::Text),
            0x2 => Ok(OpCode::Binary),
            0x8 => Ok(OpCode::Close),
            0x9 => Ok(OpCode::Ping),
            0xa => Ok(OpCode::Pong),
            other => Err(WsError::protocol(format!("opcode {other:#x} is reserved"))),
        }
    }

    /// Control frames may be delivered between the fragments of a data message,
    /// so they are never accumulated into one.
    pub fn is_control(self) -> bool {
        matches!(self, OpCode::Close | OpCode::Ping | OpCode::Pong)
    }
}

/// A close status code.
///
/// A newtype rather than an enum because the range 3000–4999 belongs to the
/// application and must survive a round trip unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseCode(pub u16);

impl CloseCode {
    pub const NORMAL: CloseCode = CloseCode(1000);
    pub const GOING_AWAY: CloseCode = CloseCode(1001);
    pub const PROTOCOL_ERROR: CloseCode = CloseCode(1002);
    pub const UNSUPPORTED: CloseCode = CloseCode(1003);
    /// Never sent on the wire: it means "the peer closed without a code".
    pub const NO_STATUS: CloseCode = CloseCode(1005);
    /// Never sent on the wire: it means "the connection died".
    pub const ABNORMAL: CloseCode = CloseCode(1006);
    pub const INVALID_PAYLOAD: CloseCode = CloseCode(1007);
    pub const POLICY: CloseCode = CloseCode(1008);
    pub const TOO_LARGE: CloseCode = CloseCode(1009);
    pub const INTERNAL_ERROR: CloseCode = CloseCode(1011);

    pub fn code(self) -> u16 {
        self.0
    }

    /// Whether this code is one a peer is allowed to put in a close frame.
    ///
    /// 1005, 1006 and 1015 describe how a connection ended locally; sending one
    /// is a protocol violation, and clients do occasionally try.
    pub fn is_sendable(self) -> bool {
        match self.0 {
            1005 | 1006 | 1015 => false,
            1000..=1014 => true,
            3000..=4999 => true,
            _ => false,
        }
    }
}

/// The body of a close frame: a status code, and optionally a reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFrame {
    pub code: CloseCode,
    pub reason: String,
}

impl CloseFrame {
    pub fn new(code: CloseCode, reason: impl Into<String>) -> Self {
        CloseFrame { code, reason: reason.into() }
    }

    /// Parse a close payload. An empty payload is legal and means "no status".
    pub fn parse(payload: &[u8]) -> WsResult<Option<CloseFrame>> {
        match payload.len() {
            0 => Ok(None),
            1 => Err(WsError::protocol("close payload must be empty or at least two bytes")),
            _ => {
                let code = CloseCode(u16::from_be_bytes([payload[0], payload[1]]));
                if !code.is_sendable() {
                    return Err(WsError::protocol(format!(
                        "close code {} must not be sent on the wire",
                        code.0
                    )));
                }
                // The reason is required to be UTF-8, and a peer that gets this
                // wrong is exactly the peer whose text frames cannot be trusted.
                let reason =
                    std::str::from_utf8(&payload[2..]).map_err(|_| WsError::InvalidUtf8)?;
                Ok(Some(CloseFrame { code, reason: reason.to_string() }))
            }
        }
    }

    /// The wire payload: the code, then the reason.
    ///
    /// A control frame carries at most 125 bytes and two of them are the code,
    /// so a long reason is truncated on a character boundary rather than being
    /// allowed to make an unsendable frame. Reasons are diagnostics, and a
    /// diagnostic that kills the connection it explains is worse than a short one.
    pub fn to_payload(&self) -> Vec<u8> {
        let mut end = self.reason.len().min(123);
        while end > 0 && !self.reason.is_char_boundary(end) {
            end -= 1;
        }

        let mut out = Vec::with_capacity(2 + end);
        out.extend_from_slice(&self.code.0.to_be_bytes());
        out.extend_from_slice(&self.reason.as_bytes()[..end]);
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub fin: bool,
    pub opcode: OpCode,
    /// Present only on frames a client sends. A server that masks its frames is
    /// as wrong as a client that does not.
    pub mask: Option<[u8; 4]>,
    pub payload: Vec<u8>,
}

impl Frame {
    /// A frame as a server sends it: final, and never masked.
    pub fn server(opcode: OpCode, payload: Vec<u8>) -> Frame {
        Frame { fin: true, opcode, mask: None, payload }
    }

    /// A frame as a client sends it. The mask is supplied rather than generated
    /// so encoding stays a pure function that a test can pin to exact bytes.
    pub fn client(opcode: OpCode, payload: Vec<u8>, mask: [u8; 4]) -> Frame {
        Frame { fin: true, opcode, mask: Some(mask), payload }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.payload.len() + 14);
        out.push(if self.fin { 0x80 } else { 0 } | self.opcode.bits());

        let masked = if self.mask.is_some() { 0x80 } else { 0 };
        let length = self.payload.len();
        // The three length forms. The RFC requires the *shortest* one that
        // fits, so a 125-byte payload never uses the 16-bit form.
        if length < 126 {
            out.push(masked | length as u8);
        } else if length <= u16::MAX as usize {
            out.push(masked | 126);
            out.extend_from_slice(&(length as u16).to_be_bytes());
        } else {
            out.push(masked | 127);
            out.extend_from_slice(&(length as u64).to_be_bytes());
        }

        match self.mask {
            Some(mask) => {
                out.extend_from_slice(&mask);
                let start = out.len();
                out.extend_from_slice(&self.payload);
                apply_mask(&mut out[start..], mask);
            }
            None => out.extend_from_slice(&self.payload),
        }
        out
    }

    /// Decode one frame from the front of `bytes`.
    ///
    /// Returns `Ok(None)` when the buffer does not hold a whole frame yet — the
    /// caller reads more and asks again — and the number of bytes consumed
    /// alongside the frame so the caller can drain exactly that much.
    ///
    /// `max_payload` is checked against the *declared* length, before anything
    /// is allocated, so a peer announcing a 4 GiB frame costs nothing.
    pub fn decode(
        bytes: &[u8],
        role: Role,
        max_payload: usize,
    ) -> WsResult<Option<(Frame, usize)>> {
        if bytes.len() < 2 {
            return Ok(None);
        }

        let (first, second) = (bytes[0], bytes[1]);
        let fin = first & 0x80 != 0;
        if first & 0x70 != 0 {
            return Err(WsError::protocol("a reserved bit is set but no extension was negotiated"));
        }
        let opcode = OpCode::from_bits(first & 0x0f)?;
        let masked = second & 0x80 != 0;

        let mut cursor = 2;
        let length = match second & 0x7f {
            126 => {
                if bytes.len() < cursor + 2 {
                    return Ok(None);
                }
                let value = u16::from_be_bytes([bytes[cursor], bytes[cursor + 1]]) as usize;
                cursor += 2;
                if value < 126 {
                    return Err(WsError::protocol(
                        "16-bit length used for a payload that fits in 7 bits",
                    ));
                }
                value
            }
            127 => {
                if bytes.len() < cursor + 8 {
                    return Ok(None);
                }
                let mut wide = [0u8; 8];
                wide.copy_from_slice(&bytes[cursor..cursor + 8]);
                let value = u64::from_be_bytes(wide);
                cursor += 8;
                if value & (1 << 63) != 0 {
                    return Err(WsError::protocol("64-bit length has its high bit set"));
                }
                if value <= u16::MAX as u64 {
                    return Err(WsError::protocol(
                        "64-bit length used for a payload that fits in 16 bits",
                    ));
                }
                // On a 32-bit target a legal 64-bit length can still be more
                // than this process could ever hold; the size limit catches it.
                match usize::try_from(value) {
                    Ok(value) => value,
                    Err(_) => {
                        return Err(WsError::TooLarge { size: usize::MAX, limit: max_payload });
                    }
                }
            }
            short => short as usize,
        };

        if length > max_payload {
            return Err(WsError::TooLarge { size: length, limit: max_payload });
        }

        if opcode.is_control() {
            if length > 125 {
                return Err(WsError::protocol("a control frame payload must be 125 bytes or fewer"));
            }
            if !fin {
                return Err(WsError::protocol("a control frame must not be fragmented"));
            }
        }

        match (role, masked) {
            (Role::Server, false) => {
                return Err(WsError::protocol("a frame from a client must be masked"));
            }
            (Role::Client, true) => {
                return Err(WsError::protocol("a frame from a server must not be masked"));
            }
            _ => {}
        }

        let mask = if masked {
            if bytes.len() < cursor + 4 {
                return Ok(None);
            }
            let mask = [bytes[cursor], bytes[cursor + 1], bytes[cursor + 2], bytes[cursor + 3]];
            cursor += 4;
            Some(mask)
        } else {
            None
        };

        if bytes.len() < cursor + length {
            return Ok(None);
        }

        let mut payload = bytes[cursor..cursor + length].to_vec();
        if let Some(mask) = mask {
            apply_mask(&mut payload, mask);
        }

        Ok(Some((Frame { fin, opcode, mask, payload }, cursor + length)))
    }
}

/// Masking is its own inverse: the same four bytes cycle over the payload.
pub fn apply_mask(payload: &mut [u8], mask: [u8; 4]) {
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_short_unmasked_text_frame() {
        let frame = Frame::server(OpCode::Text, b"Hello".to_vec());

        // FIN + text, then an unmasked 7-bit length of 5.
        assert_eq!(frame.encode(), b"\x81\x05Hello");
    }

    #[test]
    fn decodes_the_masked_frame_from_the_rfc() {
        // RFC 6455 section 5.7: a masked "Hello" sent by a client.
        let wire = b"\x81\x85\x37\xfa\x21\x3d\x7f\x9f\x4d\x51\x58";

        let (frame, used) = Frame::decode(wire, Role::Server, 1024).unwrap().unwrap();

        assert_eq!(used, wire.len());
        assert!(frame.fin);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.mask, Some([0x37, 0xfa, 0x21, 0x3d]));
        assert_eq!(frame.payload, b"Hello");
        // Re-encoding with the same mask reproduces the original bytes exactly.
        assert_eq!(frame.encode(), wire);
    }

    #[test]
    fn each_payload_length_form_round_trips() {
        for (length, header) in [
            // 7-bit: the length lives in the second byte.
            (5usize, vec![0x82, 5]),
            (125, vec![0x82, 125]),
            // 16-bit: 126 signals two more bytes.
            (126, vec![0x82, 126, 0x00, 0x7e]),
            (65_535, vec![0x82, 126, 0xff, 0xff]),
            // 64-bit: 127 signals eight more bytes.
            (65_536, vec![0x82, 127, 0, 0, 0, 0, 0, 1, 0, 0]),
        ] {
            let frame = Frame::server(OpCode::Binary, vec![0xab; length]);
            let encoded = frame.encode();

            assert_eq!(&encoded[..header.len()], &header[..], "header for {length} bytes");
            assert_eq!(encoded.len(), header.len() + length);

            let (decoded, used) = Frame::decode(&encoded, Role::Client, 1 << 20).unwrap().unwrap();
            assert_eq!(used, encoded.len());
            assert_eq!(decoded, frame, "round trip of {length} bytes");
        }
    }

    #[test]
    fn masking_is_its_own_inverse() {
        let mask = [0x11, 0x22, 0x33, 0x44];
        let mut bytes = b"the quick brown fox".to_vec();
        let original = bytes.clone();

        apply_mask(&mut bytes, mask);
        assert_ne!(bytes, original);
        apply_mask(&mut bytes, mask);
        assert_eq!(bytes, original);
    }

    #[test]
    fn a_partial_frame_asks_for_more_bytes_instead_of_failing() {
        let encoded = Frame::server(OpCode::Text, vec![b'x'; 300]).encode();

        for cut in [0, 1, 2, 3, 4, encoded.len() - 1] {
            assert!(
                Frame::decode(&encoded[..cut], Role::Client, 1024).unwrap().is_none(),
                "{cut} bytes should not decode yet"
            );
        }
        assert!(Frame::decode(&encoded, Role::Client, 1024).unwrap().is_some());
    }

    #[test]
    fn a_server_rejects_an_unmasked_client_frame() {
        let unmasked = Frame::server(OpCode::Text, b"hi".to_vec()).encode();

        let error = Frame::decode(&unmasked, Role::Server, 1024).unwrap_err();
        assert!(error.to_string().contains("must be masked"), "{error}");
    }

    #[test]
    fn a_client_rejects_a_masked_server_frame() {
        let masked = Frame::client(OpCode::Text, b"hi".to_vec(), [1, 2, 3, 4]).encode();

        let error = Frame::decode(&masked, Role::Client, 1024).unwrap_err();
        assert!(error.to_string().contains("must not be masked"), "{error}");
    }

    #[test]
    fn reserved_bits_and_reserved_opcodes_are_refused() {
        let reserved_bit = Frame::decode(&[0xc1, 0x00], Role::Client, 1024).unwrap_err();
        assert!(reserved_bit.to_string().contains("reserved bit"), "{reserved_bit}");

        let reserved_opcode = Frame::decode(&[0x83, 0x00], Role::Client, 1024).unwrap_err();
        assert!(reserved_opcode.to_string().contains("reserved"), "{reserved_opcode}");
    }

    #[test]
    fn control_frames_must_be_short_and_unfragmented() {
        // Opcode 0x9 (ping) with a declared length of 126 needs the 16-bit form,
        // which is already more than a control frame is allowed to carry.
        let long_ping = [0x89, 126, 0x00, 0x7e];
        let error = Frame::decode(&long_ping, Role::Client, 1 << 20).unwrap_err();
        assert!(error.to_string().contains("125 bytes or fewer"), "{error}");

        // FIN clear on a close frame.
        let fragmented_close = [0x08, 0x00];
        let error = Frame::decode(&fragmented_close, Role::Client, 1024).unwrap_err();
        assert!(error.to_string().contains("must not be fragmented"), "{error}");
    }

    #[test]
    fn a_declared_length_over_the_limit_fails_before_anything_is_allocated() {
        // A two-byte header claiming a 4 GiB payload, and nothing else.
        let header = [0x82, 127, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00];

        let error = Frame::decode(&header, Role::Client, 1024).unwrap_err();
        match error {
            WsError::TooLarge { size, limit } => {
                assert_eq!(size, 1 << 32);
                assert_eq!(limit, 1024);
            }
            other => panic!("expected an oversize error, got {other}"),
        }
    }

    #[test]
    fn non_minimal_length_encodings_are_refused() {
        // 16-bit form declaring 5 bytes, which the 7-bit form could have carried.
        let padded_16 = [0x82, 126, 0x00, 0x05];
        assert!(Frame::decode(&padded_16, Role::Client, 1024).is_err());

        // 64-bit form declaring 5 bytes.
        let padded_64 = [0x82, 127, 0, 0, 0, 0, 0, 0, 0, 5];
        assert!(Frame::decode(&padded_64, Role::Client, 1024).is_err());
    }

    #[test]
    fn a_fragmented_message_decodes_as_three_frames() {
        // "Hel" + "lo " + "there", the way a client streams a long message.
        let mut wire = Vec::new();
        wire.extend(Frame { fin: false, opcode: OpCode::Text, mask: None, payload: b"Hel".to_vec() }.encode());
        wire.extend(
            Frame { fin: false, opcode: OpCode::Continuation, mask: None, payload: b"lo ".to_vec() }
                .encode(),
        );
        wire.extend(
            Frame { fin: true, opcode: OpCode::Continuation, mask: None, payload: b"there".to_vec() }
                .encode(),
        );

        let mut rest = &wire[..];
        let mut assembled = Vec::new();
        let mut opcodes = Vec::new();

        while let Some((frame, used)) = Frame::decode(rest, Role::Client, 1024).unwrap() {
            opcodes.push((frame.opcode, frame.fin));
            assembled.extend_from_slice(&frame.payload);
            rest = &rest[used..];
        }

        assert_eq!(assembled, b"Hello there");
        assert_eq!(
            opcodes,
            [
                (OpCode::Text, false),
                (OpCode::Continuation, false),
                (OpCode::Continuation, true),
            ]
        );
    }

    #[test]
    fn a_close_payload_carries_a_code_and_a_reason() {
        let close = CloseFrame::new(CloseCode::NORMAL, "bye");
        let payload = close.to_payload();

        assert_eq!(payload, b"\x03\xe8bye");
        assert_eq!(CloseFrame::parse(&payload).unwrap(), Some(close));
        // An empty payload is legal: the peer closed without saying why.
        assert_eq!(CloseFrame::parse(&[]).unwrap(), None);
    }

    #[test]
    fn close_codes_that_only_describe_a_local_end_may_not_be_sent() {
        for code in [1005u16, 1006, 1015] {
            let payload = code.to_be_bytes();
            assert!(CloseFrame::parse(&payload).is_err(), "{code} should be refused");
        }
        // The application range survives untouched.
        let custom = CloseFrame::new(CloseCode(4001), "seat taken");
        assert_eq!(CloseFrame::parse(&custom.to_payload()).unwrap(), Some(custom));
    }

    #[test]
    fn a_one_byte_close_payload_is_a_protocol_error() {
        assert!(CloseFrame::parse(&[0x03]).is_err());
    }

    #[test]
    fn a_long_close_reason_is_truncated_on_a_character_boundary() {
        // Two-byte characters, so a naive cut at 123 bytes would split one.
        let close = CloseFrame::new(CloseCode::INTERNAL_ERROR, "é".repeat(100));
        let payload = close.to_payload();

        assert!(payload.len() <= 125, "a control frame may not exceed 125 bytes");
        let parsed = CloseFrame::parse(&payload).unwrap().unwrap();
        assert_eq!(parsed.code, CloseCode::INTERNAL_ERROR);
        assert_eq!(parsed.reason, "é".repeat(61));
    }
}
