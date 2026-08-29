//! RESP2: the Redis serialization protocol, encoder and decoder.
//!
//! RESP is five type-tagged, CRLF-terminated forms:
//!
//! ```text
//! +OK\r\n                     simple string
//! -ERR unknown command\r\n    error
//! :42\r\n                     integer
//! $5\r\nhello\r\n             bulk string (a length, then exactly that many bytes)
//! $-1\r\n                     the null bulk string — "no such key"
//! *2\r\n:1\r\n:2\r\n          array, which may nest
//! ```
//!
//! Commands go the other way as an array of bulk strings, which is the only
//! form a Redis server accepts from a client and the only one that is safe:
//! because every argument carries its own byte length, a value containing a
//! newline, a space, or a quote cannot be read as a second argument. That is
//! this module's answer to command injection, and it is why nothing here ever
//! builds a command by formatting a string.
//!
//! The decoder is incremental: [`decode`] returns `Ok(None)` when the buffer
//! holds only part of a reply, so the connection can read more bytes and try
//! again without any framing state of its own.

use rustlavel_core::{Error, Result};

/// The largest reply this client will assemble, as a guard against a malformed
/// or hostile length header asking for a terabyte-sized allocation.
const MAX_BULK: i64 = 512 * 1024 * 1024;

/// How deeply arrays may nest, so a crafted reply cannot exhaust the stack.
const MAX_DEPTH: usize = 32;

/// One decoded RESP value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// `+OK` — a status reply.
    Simple(String),
    /// `-ERR ...` — an error reply. Carried as a value rather than a Rust error
    /// so the decoder stays total and the caller decides what is fatal.
    Error(String),
    /// `:42`
    Integer(i64),
    /// `$5\r\nhello` — arbitrary bytes, not necessarily UTF-8.
    Bulk(Vec<u8>),
    /// `$-1` or `*-1` — the absence of a value, which is how Redis says
    /// "no such key".
    Nil,
    /// `*2\r\n...`
    Array(Vec<Value>),
}

impl Value {
    /// Interpret a reply as text, whatever shape it arrived in.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Simple(s) => Some(s),
            Value::Bulk(bytes) => std::str::from_utf8(bytes).ok(),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Simple(s) => Some(s.as_bytes()),
            Value::Bulk(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// An integer reply, or a bulk string that happens to hold digits — Redis
    /// answers `TTL` with the first and `GET` on a counter with the second.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Integer(n) => Some(*n),
            Value::Simple(s) => s.trim().parse().ok(),
            Value::Bulk(bytes) => std::str::from_utf8(bytes).ok()?.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn is_nil(&self) -> bool {
        matches!(self, Value::Nil)
    }

    /// Convert an error reply into a Rust error, leaving anything else alone.
    pub fn into_result(self) -> Result<Value> {
        match self {
            Value::Error(message) => Err(Error::msg(format!("redis: {message}"))),
            other => Ok(other),
        }
    }

    /// Serialize back to the wire. Used by the round-trip tests and by the
    /// in-process fake server they run against.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Value::Simple(s) => {
                out.push(b'+');
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Value::Error(s) => {
                out.push(b'-');
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Value::Integer(n) => {
                out.push(b':');
                out.extend_from_slice(n.to_string().as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Value::Bulk(bytes) => {
                out.push(b'$');
                out.extend_from_slice(bytes.len().to_string().as_bytes());
                out.extend_from_slice(b"\r\n");
                out.extend_from_slice(bytes);
                out.extend_from_slice(b"\r\n");
            }
            Value::Nil => out.extend_from_slice(b"$-1\r\n"),
            Value::Array(items) => {
                out.push(b'*');
                out.extend_from_slice(items.len().to_string().as_bytes());
                out.extend_from_slice(b"\r\n");
                for item in items {
                    item.encode_into(out);
                }
            }
        }
    }
}

/// Encode a command as an array of bulk strings.
///
/// Every argument is length-prefixed, so no argument can ever be mistaken for
/// another one however strange its bytes are.
pub fn encode_command(args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + args.iter().map(|a| a.len() + 16).sum::<usize>());
    out.push(b'*');
    out.extend_from_slice(args.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");

    for arg in args {
        out.push(b'$');
        out.extend_from_slice(arg.len().to_string().as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Decode one value from the front of `input`.
///
/// Returns the value and how many bytes it consumed, or `Ok(None)` when the
/// buffer does not yet hold a complete reply.
pub fn decode(input: &[u8]) -> Result<Option<(Value, usize)>> {
    decode_at(input, 0, 0)
}

fn decode_at(input: &[u8], from: usize, depth: usize) -> Result<Option<(Value, usize)>> {
    if depth > MAX_DEPTH {
        return Err(protocol("reply nests deeper than this client will follow"));
    }
    let Some(&tag) = input.get(from) else {
        return Ok(None);
    };

    let Some((line, after_line)) = read_line(input, from + 1)? else {
        return Ok(None);
    };

    match tag {
        b'+' => Ok(Some((Value::Simple(to_text(line)?), after_line))),
        b'-' => Ok(Some((Value::Error(to_text(line)?), after_line))),
        b':' => Ok(Some((Value::Integer(parse_int(line)?), after_line))),
        b'$' => {
            let length = parse_int(line)?;
            if length < 0 {
                // Any negative length is the null bulk string; only -1 is
                // canonical, but older servers were not always careful.
                return Ok(Some((Value::Nil, after_line)));
            }
            if length > MAX_BULK {
                return Err(protocol(&format!("bulk reply of {length} bytes is implausible")));
            }

            let length = length as usize;
            // The payload plus its trailing CRLF must all have arrived.
            if input.len() < after_line + length + 2 {
                return Ok(None);
            }
            let payload = input[after_line..after_line + length].to_vec();
            Ok(Some((Value::Bulk(payload), after_line + length + 2)))
        }
        b'*' => {
            let count = parse_int(line)?;
            if count < 0 {
                return Ok(Some((Value::Nil, after_line)));
            }
            if count > MAX_BULK {
                return Err(protocol(&format!("array reply of {count} items is implausible")));
            }

            let mut items = Vec::with_capacity((count as usize).min(1024));
            let mut cursor = after_line;
            for _ in 0..count {
                match decode_at(input, cursor, depth + 1)? {
                    Some((item, next)) => {
                        items.push(item);
                        cursor = next;
                    }
                    // One element short: the whole array is still incomplete.
                    None => return Ok(None),
                }
            }
            Ok(Some((Value::Array(items), cursor)))
        }
        other => Err(protocol(&format!(
            "unknown RESP type byte {:?} — is this actually a Redis server?",
            other as char
        ))),
    }
}

/// Find the CRLF-terminated line starting at `from`, returning it and the index
/// just past the CRLF.
fn read_line(input: &[u8], from: usize) -> Result<Option<(&[u8], usize)>> {
    let mut index = from;
    while index + 1 < input.len() {
        if input[index] == b'\r' && input[index + 1] == b'\n' {
            return Ok(Some((&input[from..index], index + 2)));
        }
        index += 1;
    }
    Ok(None)
}

fn to_text(bytes: &[u8]) -> Result<String> {
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|_| protocol("a status or error reply was not valid UTF-8"))
}

fn parse_int(bytes: &[u8]) -> Result<i64> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .ok_or_else(|| protocol(&format!("`{}` is not a RESP integer", String::from_utf8_lossy(bytes))))
}

fn protocol(message: &str) -> Error {
    Error::Protocol(format!("RESP: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(bytes: &[u8]) -> Value {
        let (value, consumed) = decode(bytes).unwrap().expect("a complete reply");
        assert_eq!(consumed, bytes.len(), "the decoder must consume exactly the reply");
        value
    }

    #[test]
    fn decodes_a_simple_string() {
        assert_eq!(decoded(b"+OK\r\n"), Value::Simple("OK".into()));
        assert_eq!(decoded(b"+PONG\r\n"), Value::Simple("PONG".into()));
        assert_eq!(decoded(b"+\r\n"), Value::Simple(String::new()));
    }

    #[test]
    fn decodes_an_error_and_keeps_it_out_of_the_result_type_until_asked() {
        let value = decoded(b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n");
        assert_eq!(
            value,
            Value::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        );

        let error = value.into_result().unwrap_err();
        assert!(error.to_string().contains("WRONGTYPE"));
    }

    #[test]
    fn decodes_integers_including_negative_and_zero() {
        assert_eq!(decoded(b":0\r\n"), Value::Integer(0));
        assert_eq!(decoded(b":1000\r\n"), Value::Integer(1000));
        assert_eq!(decoded(b":-42\r\n"), Value::Integer(-42));
    }

    #[test]
    fn decodes_a_bulk_string_including_empty_and_binary_payloads() {
        assert_eq!(decoded(b"$5\r\nhello\r\n"), Value::Bulk(b"hello".to_vec()));
        assert_eq!(decoded(b"$0\r\n\r\n"), Value::Bulk(Vec::new()));

        // A payload with an embedded CRLF is exactly why bulk strings carry a
        // length: a line-oriented parser would stop halfway.
        assert_eq!(decoded(b"$7\r\na\r\nb\r\nc\r\n"), Value::Bulk(b"a\r\nb\r\nc".to_vec()));
    }

    #[test]
    fn decodes_the_null_bulk_string_as_nil() {
        assert_eq!(decoded(b"$-1\r\n"), Value::Nil);
        assert!(decoded(b"$-1\r\n").is_nil());
        assert_eq!(decoded(b"*-1\r\n"), Value::Nil, "a null array is also an absence");
    }

    #[test]
    fn decodes_an_array_of_mixed_types() {
        assert_eq!(
            decoded(b"*3\r\n:1\r\n$3\r\ntwo\r\n+three\r\n"),
            Value::Array(vec![
                Value::Integer(1),
                Value::Bulk(b"two".to_vec()),
                Value::Simple("three".into()),
            ])
        );
        assert_eq!(decoded(b"*0\r\n"), Value::Array(Vec::new()));
    }

    #[test]
    fn decodes_nested_arrays() {
        let value = decoded(b"*2\r\n*2\r\n:1\r\n:2\r\n*2\r\n$3\r\nfoo\r\n$-1\r\n");
        assert_eq!(
            value,
            Value::Array(vec![
                Value::Array(vec![Value::Integer(1), Value::Integer(2)]),
                Value::Array(vec![Value::Bulk(b"foo".to_vec()), Value::Nil]),
            ])
        );
    }

    #[test]
    fn a_partial_reply_asks_for_more_bytes_instead_of_failing() {
        let complete: &[u8] = b"*2\r\n$5\r\nhello\r\n$5\r\nworld\r\n";
        for cut in 1..complete.len() {
            assert_eq!(
                decode(&complete[..cut]).unwrap(),
                None,
                "a {cut}-byte prefix must not decode"
            );
        }
        assert!(decode(complete).unwrap().is_some());
    }

    #[test]
    fn decoding_stops_at_the_end_of_one_reply_when_two_are_pipelined() {
        let buffer: &[u8] = b"+OK\r\n:7\r\n";
        let (first, consumed) = decode(buffer).unwrap().unwrap();

        assert_eq!(first, Value::Simple("OK".into()));
        assert_eq!(consumed, 5);

        let (second, _) = decode(&buffer[consumed..]).unwrap().unwrap();
        assert_eq!(second, Value::Integer(7));
    }

    #[test]
    fn every_value_survives_an_encode_decode_round_trip() {
        let values = [
            Value::Simple("OK".into()),
            Value::Error("ERR no such key".into()),
            Value::Integer(-9_000_000_000),
            Value::Bulk(b"payload with \r\n inside".to_vec()),
            Value::Bulk(vec![0x00, 0xff, 0x7f]),
            Value::Nil,
            Value::Array(vec![]),
            Value::Array(vec![
                Value::Integer(1),
                Value::Nil,
                Value::Array(vec![Value::Bulk(b"deep".to_vec())]),
            ]),
        ];

        for value in values {
            let bytes = value.encode();
            let (back, consumed) = decode(&bytes).unwrap().expect("a complete reply");
            assert_eq!(back, value);
            assert_eq!(consumed, bytes.len());
        }
    }

    #[test]
    fn a_command_is_encoded_as_an_array_of_bulk_strings() {
        assert_eq!(
            encode_command(&[b"SET", b"name", b"ada"]),
            b"*3\r\n$3\r\nSET\r\n$4\r\nname\r\n$3\r\nada\r\n".to_vec()
        );
        assert_eq!(encode_command(&[b"PING"]), b"*1\r\n$4\r\nPING\r\n".to_vec());
    }

    #[test]
    fn an_argument_full_of_protocol_syntax_stays_one_argument() {
        // The whole point of length-prefixing: this cannot become `FLUSHALL`.
        let hostile = b"value\r\nFLUSHALL\r\n";
        let encoded = encode_command(&[b"SET", b"key", hostile]);

        let (decoded, _) = decode(&encoded).unwrap().unwrap();
        let Value::Array(parts) = decoded else { panic!("a command is an array") };

        assert_eq!(parts.len(), 3, "the injected newline must not create a fourth argument");
        assert_eq!(parts[2], Value::Bulk(hostile.to_vec()));
    }

    #[test]
    fn a_reply_that_is_not_resp_at_all_is_reported_as_a_protocol_error() {
        let error = decode(b"HTTP/1.1 200 OK\r\n").unwrap_err();
        assert!(error.to_string().contains("Redis server"), "got: {error}");
    }

    #[test]
    fn an_implausible_length_is_refused_before_anything_is_allocated() {
        assert!(decode(b"$999999999999\r\n").is_err());
        assert!(decode(b"*999999999999\r\n").is_err());
    }

    #[test]
    fn a_length_header_that_is_not_a_number_is_a_protocol_error() {
        assert!(decode(b"$abc\r\n").is_err());
        assert!(decode(b":not-an-integer\r\n").is_err());
    }

    #[test]
    fn values_read_as_text_and_numbers_whatever_form_they_arrived_in() {
        assert_eq!(Value::Bulk(b"42".to_vec()).as_i64(), Some(42));
        assert_eq!(Value::Integer(42).as_i64(), Some(42));
        assert_eq!(Value::Simple("42".into()).as_i64(), Some(42));
        assert_eq!(Value::Nil.as_i64(), None);
        assert_eq!(Value::Bulk(b"hello".to_vec()).as_str(), Some("hello"));
        assert_eq!(Value::Bulk(vec![0xff]).as_str(), None, "invalid UTF-8 is not text");
    }
}
