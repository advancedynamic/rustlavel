//! Just enough CBOR to read what an authenticator sends.
//!
//! WebAuthn wraps its attestation object and its public keys in CBOR, so this
//! has to exist. It is a serialisation format rather than cryptography, so
//! rule one applies and it is written here — but the subset is deliberately
//! small: CTAP2 pins authenticators to *canonical* CBOR, and a decoder that
//! accepts more than the specification allows is a decoder that accepts things
//! the specification was trying to keep out.
//!
//! Indefinite-length items, tags and floats are refused rather than parsed. No
//! authenticator may send them, so anything that does is either broken or
//! probing.

use rustlavel_core::{Error, Result};
use std::collections::BTreeMap;

/// A decoded CBOR value, in the shapes WebAuthn actually uses.
#[derive(Debug, Clone, PartialEq)]
pub enum Cbor {
    Unsigned(u64),
    Negative(i64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    /// Keys stay ordered, and both integer and text keys occur: COSE keys are
    /// integer-keyed, attestation objects text-keyed.
    Map(BTreeMap<CborKey, Cbor>),
    Bool(bool),
    Null,
}

/// A map key. COSE uses small integers; attestation objects use strings.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum CborKey {
    Int(i64),
    Text(String),
}

impl Cbor {
    /// Decode one value, and refuse trailing bytes.
    ///
    /// Trailing data means the input was not what it claimed to be, and
    /// ignoring it is how a parser gets used to smuggle a second message.
    pub fn parse(bytes: &[u8]) -> Result<Cbor> {
        let mut decoder = Decoder { bytes, position: 0, depth: 0 };
        let value = decoder.value()?;
        if decoder.position != bytes.len() {
            return Err(Error::msg(format!(
                "CBOR had {} trailing bytes after a complete value",
                bytes.len() - decoder.position
            )));
        }
        Ok(value)
    }

    /// Decode one value and say where it ended.
    ///
    /// The attestation object holds `authData` as a byte string whose length
    /// the caller needs in order to keep reading past it.
    pub fn parse_prefix(bytes: &[u8]) -> Result<(Cbor, usize)> {
        let mut decoder = Decoder { bytes, position: 0, depth: 0 };
        let value = decoder.value()?;
        Ok((value, decoder.position))
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Cbor::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Cbor::Text(text) => Some(text),
            _ => None,
        }
    }

    /// A signed integer, however CBOR chose to encode it.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Cbor::Unsigned(value) => i64::try_from(*value).ok(),
            Cbor::Negative(value) => Some(*value),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Cbor> {
        match self {
            Cbor::Map(map) => map.get(&CborKey::Text(key.to_string())),
            _ => None,
        }
    }

    pub fn get_int(&self, key: i64) -> Option<&Cbor> {
        match self {
            Cbor::Map(map) => map.get(&CborKey::Int(key)),
            _ => None,
        }
    }
}

/// How deep a nested structure may go.
///
/// WebAuthn's are three or four levels; anything approaching this is hostile,
/// and without a limit a few bytes of nested arrays can exhaust the stack.
const MAX_DEPTH: usize = 16;

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
    depth: usize,
}

impl Decoder<'_> {
    fn value(&mut self) -> Result<Cbor> {
        if self.depth > MAX_DEPTH {
            return Err(Error::msg(format!("CBOR nested deeper than {MAX_DEPTH} levels")));
        }

        let initial = self.byte()?;
        let major = initial >> 5;
        let extra = initial & 0x1f;

        match major {
            0 => Ok(Cbor::Unsigned(self.argument(extra)?)),
            1 => {
                let magnitude = self.argument(extra)?;
                // -1 - n, and the largest legal value does not fit in i64.
                i64::try_from(magnitude)
                    .map(|n| Cbor::Negative(-1 - n))
                    .map_err(|_| Error::msg("CBOR negative integer is too large for i64"))
            }
            2 => Ok(Cbor::Bytes(self.take(extra)?.to_vec())),
            3 => {
                let raw = self.take(extra)?;
                String::from_utf8(raw.to_vec())
                    .map(Cbor::Text)
                    .map_err(|_| Error::msg("CBOR text string is not valid UTF-8"))
            }
            4 => {
                let count = self.count(extra)?;
                let mut items = Vec::with_capacity(count.min(1024));
                for _ in 0..count {
                    self.depth += 1;
                    items.push(self.value()?);
                    self.depth -= 1;
                }
                Ok(Cbor::Array(items))
            }
            5 => {
                let count = self.count(extra)?;
                let mut map = BTreeMap::new();
                for _ in 0..count {
                    self.depth += 1;
                    let key = self.key()?;
                    let value = self.value()?;
                    self.depth -= 1;
                    // A repeated key is not canonical CBOR, and taking either
                    // one silently is how two parsers end up disagreeing about
                    // the same bytes.
                    if map.insert(key, value).is_some() {
                        return Err(Error::msg("CBOR map has a duplicate key"));
                    }
                }
                Ok(Cbor::Map(map))
            }
            7 => match extra {
                20 => Ok(Cbor::Bool(false)),
                21 => Ok(Cbor::Bool(true)),
                22 => Ok(Cbor::Null),
                23 => Ok(Cbor::Null),
                other => Err(Error::msg(format!(
                    "CBOR simple value {other} is not one an authenticator may send"
                ))),
            },
            other => Err(Error::msg(format!(
                "CBOR major type {other} is not accepted here — tags and floats have no place \
                 in an authenticator message"
            ))),
        }
    }

    fn key(&mut self) -> Result<CborKey> {
        match self.value()? {
            Cbor::Unsigned(value) => i64::try_from(value)
                .map(CborKey::Int)
                .map_err(|_| Error::msg("CBOR map key is too large")),
            Cbor::Negative(value) => Ok(CborKey::Int(value)),
            Cbor::Text(text) => Ok(CborKey::Text(text)),
            other => Err(Error::msg(format!("CBOR map key must be an integer or text, got {other:?}"))),
        }
    }

    /// The argument encoded in the low five bits, or in the bytes after it.
    fn argument(&mut self, extra: u8) -> Result<u64> {
        match extra {
            0..=23 => Ok(extra as u64),
            24 => Ok(self.byte()? as u64),
            25 => Ok(u16::from_be_bytes(self.fixed::<2>()?) as u64),
            26 => Ok(u32::from_be_bytes(self.fixed::<4>()?) as u64),
            27 => Ok(u64::from_be_bytes(self.fixed::<8>()?)),
            // 31 is indefinite length, which canonical CBOR forbids.
            31 => Err(Error::msg(
                "CBOR indefinite-length items are not canonical and are refused here",
            )),
            other => Err(Error::msg(format!("CBOR additional information {other} is reserved"))),
        }
    }

    /// A length, checked against what is actually left.
    ///
    /// Without this a header claiming four billion elements makes the decoder
    /// allocate before it discovers the message is eight bytes long.
    fn count(&mut self, extra: u8) -> Result<usize> {
        let claimed = self.argument(extra)?;
        let remaining = self.bytes.len() - self.position;
        if claimed > remaining as u64 {
            return Err(Error::msg(format!(
                "CBOR header claims {claimed} items but only {remaining} bytes remain"
            )));
        }
        Ok(claimed as usize)
    }

    fn take(&mut self, extra: u8) -> Result<&[u8]> {
        let length = self.argument(extra)? as usize;
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| Error::msg("CBOR length overflows"))?;
        if end > self.bytes.len() {
            return Err(Error::msg(format!(
                "CBOR string of {length} bytes runs past the end of the message"
            )));
        }
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8> {
        let byte = *self
            .bytes
            .get(self.position)
            .ok_or_else(|| Error::msg("CBOR ended in the middle of a value"))?;
        self.position += 1;
        Ok(byte)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        let end = self.position + N;
        if end > self.bytes.len() {
            return Err(Error::msg("CBOR ended in the middle of a number"));
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.bytes[self.position..end]);
        self.position = end;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_integer_encodings() {
        assert_eq!(Cbor::parse(&[0x00]).unwrap(), Cbor::Unsigned(0));
        assert_eq!(Cbor::parse(&[0x17]).unwrap(), Cbor::Unsigned(23));
        assert_eq!(Cbor::parse(&[0x18, 0x18]).unwrap(), Cbor::Unsigned(24));
        assert_eq!(Cbor::parse(&[0x19, 0x01, 0x00]).unwrap(), Cbor::Unsigned(256));
        assert_eq!(Cbor::parse(&[0x1a, 0, 1, 0, 0]).unwrap(), Cbor::Unsigned(65536));
        // -7 is how COSE names ES256, so this exact value matters.
        assert_eq!(Cbor::parse(&[0x26]).unwrap(), Cbor::Negative(-7));
        assert_eq!(Cbor::parse(&[0x20]).unwrap(), Cbor::Negative(-1));
    }

    #[test]
    fn reads_strings_arrays_and_maps() {
        assert_eq!(Cbor::parse(&[0x43, 1, 2, 3]).unwrap(), Cbor::Bytes(vec![1, 2, 3]));
        assert_eq!(Cbor::parse(b"\x63abc").unwrap(), Cbor::Text("abc".into()));
        assert_eq!(
            Cbor::parse(&[0x82, 0x01, 0x02]).unwrap(),
            Cbor::Array(vec![Cbor::Unsigned(1), Cbor::Unsigned(2)])
        );

        let map = Cbor::parse(b"\xa1\x63fmt\x64none").unwrap();
        assert_eq!(map.get("fmt").and_then(Cbor::as_text), Some("none"));
    }

    #[test]
    fn a_cose_key_reads_by_integer_label() {
        // {1: 2, 3: -7} — kty EC2, alg ES256, the shape every passkey sends.
        let key = Cbor::parse(&[0xa2, 0x01, 0x02, 0x03, 0x26]).unwrap();

        assert_eq!(key.get_int(1).and_then(Cbor::as_i64), Some(2));
        assert_eq!(key.get_int(3).and_then(Cbor::as_i64), Some(-7));
    }

    #[test]
    fn trailing_bytes_are_refused() {
        // Accepting them is how one message smuggles a second past a parser.
        let error = Cbor::parse(&[0x00, 0x00]).unwrap_err().to_string();
        assert!(error.contains("trailing"), "got {error}");

        // The prefix form exists precisely for the case where more follows.
        let (value, consumed) = Cbor::parse_prefix(&[0x00, 0xff]).unwrap();
        assert_eq!(value, Cbor::Unsigned(0));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn a_length_longer_than_the_message_is_refused_before_allocating() {
        // The header claims four gigabytes; the message is six bytes. Trusting
        // it would allocate first and discover the truth afterwards.
        let error = Cbor::parse(&[0x5a, 0xff, 0xff, 0xff, 0xff, 0x00]).unwrap_err().to_string();
        assert!(error.contains("runs past the end"), "got {error}");

        let error = Cbor::parse(&[0x9a, 0xff, 0xff, 0xff, 0xff]).unwrap_err().to_string();
        assert!(error.contains("only"), "got {error}");
    }

    #[test]
    fn indefinite_lengths_are_refused_rather_than_parsed() {
        // Canonical CBOR forbids them, so an authenticator cannot legitimately
        // send one — which makes anything that does worth rejecting outright.
        let error = Cbor::parse(&[0x5f, 0x41, 0x01, 0xff]).unwrap_err().to_string();
        assert!(error.contains("indefinite"), "got {error}");
    }

    #[test]
    fn tags_and_floats_are_refused() {
        assert!(Cbor::parse(&[0xc0, 0x00]).is_err(), "tag");
        assert!(Cbor::parse(&[0xfa, 0, 0, 0, 0]).is_err(), "float");
    }

    #[test]
    fn a_duplicate_map_key_is_an_error_not_a_silent_choice() {
        // Two parsers picking different winners for the same bytes is exactly
        // how a signature covers one thing and a check reads another.
        let error = Cbor::parse(&[0xa2, 0x01, 0x01, 0x01, 0x02]).unwrap_err().to_string();
        assert!(error.contains("duplicate"), "got {error}");
    }

    #[test]
    fn deep_nesting_is_bounded() {
        // Seventeen nested one-element arrays: a few bytes that would otherwise
        // recurse until the stack gives out.
        let deep: Vec<u8> = std::iter::repeat_n(0x81, MAX_DEPTH + 2).chain([0x00]).collect();
        let error = Cbor::parse(&deep).unwrap_err().to_string();
        assert!(error.contains("nested deeper"), "got {error}");
    }

    #[test]
    fn truncation_anywhere_is_an_error_and_never_a_panic() {
        // Every prefix of a valid message must be refused cleanly — this is the
        // shape of input an attacker controls completely.
        let complete: &[u8] = &[0xa2, 0x01, 0x02, 0x03, 0x26];
        for length in 0..complete.len() {
            assert!(Cbor::parse(&complete[..length]).is_err(), "prefix of {length} bytes");
        }
        assert!(Cbor::parse(complete).is_ok());
    }

    #[test]
    fn text_that_is_not_utf8_is_refused() {
        assert!(Cbor::parse(&[0x62, 0xff, 0xfe]).is_err());
    }
}
