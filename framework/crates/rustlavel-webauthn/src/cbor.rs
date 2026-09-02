//! Just enough CBOR to read what an authenticator sends, and to write it back.
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
//!
//! The encoder is the same subset in the other direction, and it exists so a
//! credential can be *stored*. A parsed key with no way back to bytes is a key
//! that cannot go in a database column, which left the in-memory
//! `CredentialStore` as the only one anybody could write. Because it emits the
//! one canonical form, re-encoding what an authenticator sent reproduces the
//! authenticator's own bytes.

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

    /// Write this value out as canonical CTAP2 CBOR.
    ///
    /// "Canonical" is not decoration. RFC 8949 §4.2.1 calls it deterministic
    /// encoding and CTAP2 §6 calls it the canonical CBOR encoding form; both
    /// mean definite lengths, every integer and every length written in the
    /// shortest form that holds it, and one fixed order for map keys. Two
    /// consequences follow, and the tests hold this to both: encoding a value
    /// twice gives the same bytes, and encoding something an authenticator
    /// sent gives back exactly the bytes the authenticator sent.
    ///
    /// Infallible on purpose — every `Cbor` variant has an encoding, so there
    /// is no failure for a caller to handle and no `Result` to unwrap in the
    /// middle of storing a credential.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut Vec<u8>) {
        match self {
            Cbor::Unsigned(value) => head(0, *value, out),
            Cbor::Negative(value) => head(1, negative_argument(*value), out),
            Cbor::Bytes(bytes) => {
                head(2, bytes.len() as u64, out);
                out.extend_from_slice(bytes);
            }
            Cbor::Text(text) => {
                head(3, text.len() as u64, out);
                out.extend_from_slice(text.as_bytes());
            }
            Cbor::Array(items) => {
                head(4, items.len() as u64, out);
                for item in items {
                    item.write(out);
                }
            }
            Cbor::Map(map) => {
                head(5, map.len() as u64, out);

                // The keys are sorted on their *encoded* bytes, which is not
                // the order the `BTreeMap` holds them in: `CborKey` orders
                // every integer before every text and orders integers
                // numerically, while CBOR compares the bytes those keys turn
                // into. Encoding each key first and sorting afterwards is the
                // only way to get the order the specification asks for.
                let mut entries: Vec<(Vec<u8>, &Cbor)> = map
                    .iter()
                    .map(|(key, value)| {
                        let mut encoded = Vec::new();
                        key.write(&mut encoded);
                        (encoded, value)
                    })
                    .collect();
                entries.sort_by(|(left, _), (right, _)| canonical_key_order(left, right));

                for (key, value) in entries {
                    out.extend_from_slice(&key);
                    value.write(out);
                }
            }
            // Major type 7: 20 is false and 21 is true (RFC 8949 §3.3).
            Cbor::Bool(value) => out.push(0xf4 | u8::from(*value)),
            // 22 is null. Note that the decoder also folds 23 (undefined) into
            // `Null`, so an `undefined` an authenticator somehow sent comes
            // back out as `null` — the only place a round trip changes bytes,
            // and a distinction WebAuthn has no use for.
            Cbor::Null => out.push(0xf6),
        }
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

impl CborKey {
    fn write(&self, out: &mut Vec<u8>) {
        match self {
            CborKey::Int(value) if *value >= 0 => head(0, *value as u64, out),
            CborKey::Int(value) => head(1, negative_argument(*value), out),
            CborKey::Text(text) => {
                head(3, text.len() as u64, out);
                out.extend_from_slice(text.as_bytes());
            }
        }
    }
}

/// Write a major type and its argument in the shortest form that holds it.
///
/// RFC 8949 §4.2.1 requires the "preferred serialization" of every argument:
/// values below 24 go in the low five bits of the initial byte, and everything
/// else takes the smallest of the one-, two-, four- and eight-byte forms.
/// CTAP2 says the same thing in its own words — integers encoded as small as
/// possible, lengths expressed as short as possible — so an authenticator that
/// disagreed with this function would not be sending canonical CBOR.
fn head(major: u8, argument: u64, out: &mut Vec<u8>) {
    let major = major << 5;
    match argument {
        0..=23 => out.push(major | argument as u8),
        24..=0xff => {
            out.push(major | 24);
            out.push(argument as u8);
        }
        0x100..=0xffff => {
            out.push(major | 25);
            out.extend_from_slice(&(argument as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(major | 26);
            out.extend_from_slice(&(argument as u32).to_be_bytes());
        }
        _ => {
            out.push(major | 27);
            out.extend_from_slice(&argument.to_be_bytes());
        }
    }
}

/// The argument a negative integer carries: major type 1 encodes -1 - n, so a
/// value of -1 is written as 0 and -256 as 255 (RFC 8949 §3.1).
///
/// The subtraction happens in `i128` because `-1 - i64::MIN` overflows an
/// `i64`, and that value — the largest negative CBOR integer this type can
/// hold — is exactly the one an attacker would reach for. `Negative` never
/// holds a value above -1: the decoder builds it as `-1 - n` from a
/// non-negative `n`, and nothing else constructs one. A hand-built value that
/// broke that invariant would be a programming error rather than untrusted
/// input, so it is clamped to -1 rather than being allowed to wrap.
fn negative_argument(value: i64) -> u64 {
    (-1i128 - i128::from(value)).max(0) as u64
}

/// The order canonical CBOR puts map keys in: shorter encodings first, then
/// byte-wise lexicographic among keys of the same length.
///
/// This is RFC 8949 §4.2.3's length-first ordering, which is what CTAP2's
/// canonical CBOR requires — not the plain byte-wise ordering of §4.2.1. The
/// difference is visible in every attestation object: "fmt", "attStmt" and
/// "authData" are sent in that order because "fmt" is the shortest, where
/// byte-wise ordering would have put it last.
///
/// CTAP2 words its own rule as major type first, then length, then bytes.
/// Comparing encoded bytes gives the same answer whenever two keys encode to
/// the same length, because the major type lives in the high bits of the first
/// byte; the two rules can only disagree when a lower major type needs a
/// longer encoding, which takes a map key above 23 — larger than any COSE
/// label or any key WebAuthn defines.
fn canonical_key_order(left: &[u8], right: &[u8]) -> std::cmp::Ordering {
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
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

    /// Hex in, so a row of RFC 8949 Appendix A can be written here exactly as
    /// the RFC prints it and read back against the table by eye.
    fn from_hex(text: &str) -> Vec<u8> {
        assert!(text.len().is_multiple_of(2), "hex must come in whole bytes");
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex digits"))
            .collect()
    }

    /// Encode a value, compare it with the RFC's own hex, then decode what was
    /// written and check it is the value we started from.
    ///
    /// Both halves matter. The first says the encoder agrees with the
    /// specification rather than merely with itself; the second says the two
    /// directions in this file agree with each other.
    fn appendix_a(value: Cbor, expected: &str) {
        let encoded = value.to_bytes();
        assert_eq!(crate::ceremony::hex(&encoded), expected, "encoding {value:?}");
        assert_eq!(Cbor::parse(&encoded).unwrap(), value, "decoding {expected}");
    }

    fn map(entries: impl IntoIterator<Item = (CborKey, Cbor)>) -> Cbor {
        Cbor::Map(entries.into_iter().collect())
    }

    #[test]
    fn appendix_a_unsigned_integers() {
        appendix_a(Cbor::Unsigned(0), "00");
        appendix_a(Cbor::Unsigned(1), "01");
        appendix_a(Cbor::Unsigned(10), "0a");
        appendix_a(Cbor::Unsigned(23), "17");
        appendix_a(Cbor::Unsigned(24), "1818");
        appendix_a(Cbor::Unsigned(25), "1819");
        appendix_a(Cbor::Unsigned(100), "1864");
        appendix_a(Cbor::Unsigned(1000), "1903e8");
        appendix_a(Cbor::Unsigned(1_000_000), "1a000f4240");
        appendix_a(Cbor::Unsigned(1_000_000_000_000), "1b000000e8d4a51000");
        appendix_a(Cbor::Unsigned(18_446_744_073_709_551_615), "1bffffffffffffffff");
    }

    #[test]
    fn appendix_a_negative_integers() {
        appendix_a(Cbor::Negative(-1), "20");
        appendix_a(Cbor::Negative(-10), "29");
        appendix_a(Cbor::Negative(-100), "3863");
        appendix_a(Cbor::Negative(-1000), "3903e7");
    }

    #[test]
    fn appendix_a_simple_values() {
        appendix_a(Cbor::Bool(false), "f4");
        appendix_a(Cbor::Bool(true), "f5");
        appendix_a(Cbor::Null, "f6");
    }

    #[test]
    fn appendix_a_byte_and_text_strings() {
        appendix_a(Cbor::Bytes(Vec::new()), "40");
        appendix_a(Cbor::Bytes(vec![1, 2, 3, 4]), "4401020304");

        appendix_a(Cbor::Text(String::new()), "60");
        appendix_a(Cbor::Text("a".into()), "6161");
        appendix_a(Cbor::Text("IETF".into()), "6449455446");
        appendix_a(Cbor::Text("\"\\".into()), "62225c");
        // Text lengths are in bytes, not characters — a two-byte "ü", a
        // three-byte "水" and a four-byte "𐅑" all carry a length of one
        // character less than nothing, which is the classic way to get this
        // wrong.
        appendix_a(Cbor::Text("\u{00fc}".into()), "62c3bc");
        appendix_a(Cbor::Text("\u{6c34}".into()), "63e6b0b4");
        appendix_a(Cbor::Text("\u{10151}".into()), "64f0908591");
    }

    #[test]
    fn appendix_a_arrays() {
        appendix_a(Cbor::Array(Vec::new()), "80");
        appendix_a(
            Cbor::Array(vec![Cbor::Unsigned(1), Cbor::Unsigned(2), Cbor::Unsigned(3)]),
            "83010203",
        );
        appendix_a(
            Cbor::Array(vec![
                Cbor::Unsigned(1),
                Cbor::Array(vec![Cbor::Unsigned(2), Cbor::Unsigned(3)]),
                Cbor::Array(vec![Cbor::Unsigned(4), Cbor::Unsigned(5)]),
            ]),
            "8301820203820405",
        );
        // Twenty-five items: the element count crosses the same threshold an
        // integer does, and takes the two-byte form.
        appendix_a(
            Cbor::Array((1..=25).map(Cbor::Unsigned).collect()),
            "98190102030405060708090a0b0c0d0e0f101112131415161718181819",
        );
    }

    #[test]
    fn appendix_a_maps_and_nesting() {
        appendix_a(map([]), "a0");
        appendix_a(
            map([
                (CborKey::Int(1), Cbor::Unsigned(2)),
                (CborKey::Int(3), Cbor::Unsigned(4)),
            ]),
            "a201020304",
        );
        appendix_a(
            map([
                (CborKey::Text("a".into()), Cbor::Unsigned(1)),
                (
                    CborKey::Text("b".into()),
                    Cbor::Array(vec![Cbor::Unsigned(2), Cbor::Unsigned(3)]),
                ),
            ]),
            "a26161016162820203",
        );
        appendix_a(
            Cbor::Array(vec![
                Cbor::Text("a".into()),
                map([(CborKey::Text("b".into()), Cbor::Text("c".into()))]),
            ]),
            "826161a161626163",
        );
        appendix_a(
            map([
                (CborKey::Text("a".into()), Cbor::Text("A".into())),
                (CborKey::Text("b".into()), Cbor::Text("B".into())),
                (CborKey::Text("c".into()), Cbor::Text("C".into())),
                (CborKey::Text("d".into()), Cbor::Text("D".into())),
                (CborKey::Text("e".into()), Cbor::Text("E".into())),
            ]),
            "a56161614161626142616361436164614461656145",
        );
    }

    #[test]
    fn the_argument_boundaries_take_the_shortest_form_on_both_sides() {
        // Preferred serialisation (RFC 8949 §4.2.1) is the rule a canonical
        // encoder is likeliest to get wrong, and these are the four thresholds
        // where it would show. One byte too many at any of them and the bytes
        // stop matching what the device sent.
        appendix_a(Cbor::Unsigned(255), "18ff");
        appendix_a(Cbor::Unsigned(256), "190100");
        appendix_a(Cbor::Unsigned(65_535), "19ffff");
        appendix_a(Cbor::Unsigned(65_536), "1a00010000");
        appendix_a(Cbor::Unsigned(4_294_967_295), "1affffffff");
        appendix_a(Cbor::Unsigned(4_294_967_296), "1b0000000100000000");

        // The negative mirrors. Major type 1 stores -1 - n, so each threshold
        // sits one further from zero than its unsigned twin.
        appendix_a(Cbor::Negative(-24), "37");
        appendix_a(Cbor::Negative(-25), "3818");
        appendix_a(Cbor::Negative(-256), "38ff");
        appendix_a(Cbor::Negative(-257), "390100");
        appendix_a(Cbor::Negative(-65_536), "39ffff");
        appendix_a(Cbor::Negative(-65_537), "3a00010000");
        appendix_a(Cbor::Negative(-4_294_967_296), "3affffffff");
        appendix_a(Cbor::Negative(-4_294_967_297), "3b0000000100000000");
    }

    #[test]
    fn the_largest_negative_integer_does_not_overflow_on_the_way_out() {
        // -1 - i64::MIN overflows an i64, and this is the one value that finds
        // it. Nothing more negative can reach the encoder: the decoder refuses
        // a magnitude that will not fit in an i64.
        appendix_a(Cbor::Negative(i64::MIN), "3b7fffffffffffffff");
    }

    #[test]
    fn string_and_container_lengths_take_the_shortest_form_too() {
        assert_eq!(Cbor::Bytes(vec![0; 23]).to_bytes()[..1], [0x57]);
        assert_eq!(Cbor::Bytes(vec![0; 24]).to_bytes()[..2], [0x58, 24]);
        assert_eq!(Cbor::Bytes(vec![0; 255]).to_bytes()[..2], [0x58, 0xff]);
        assert_eq!(Cbor::Bytes(vec![0; 256]).to_bytes()[..3], [0x59, 0x01, 0x00]);
        // 0x58 0x20 is how every COSE key on the wire introduces a coordinate.
        assert_eq!(Cbor::Bytes(vec![0; 32]).to_bytes()[..2], [0x58, 0x20]);
    }

    #[test]
    fn map_keys_are_sorted_on_their_encoded_bytes_and_not_on_the_rust_value() {
        // `CborKey` puts every integer before every text and orders integers
        // numerically. Canonical CBOR orders the bytes those keys encode to,
        // shortest first — so 1_000_000, which needs five bytes, comes last,
        // behind a text key the `BTreeMap` holds after it.
        let value = map([
            (CborKey::Text("a".into()), Cbor::Unsigned(1)),
            (CborKey::Int(1_000_000), Cbor::Unsigned(2)),
            (CborKey::Int(1), Cbor::Unsigned(3)),
        ]);

        let Cbor::Map(entries) = &value else { unreachable!("built as a map") };
        assert_eq!(
            entries.keys().collect::<Vec<_>>(),
            vec![&CborKey::Int(1), &CborKey::Int(1_000_000), &CborKey::Text("a".into())],
            "the map really is held in an order the encoder has to undo"
        );

        // 01, then 6161 ("a"), then 1a000f4240 — one byte, two, five.
        appendix_a(value, "a301036161011a000f424002");
    }

    #[test]
    fn text_keys_sort_by_length_first_which_is_what_an_attestation_object_needs() {
        // Byte-wise ordering (RFC 8949 §4.2.1) would give attStmt, authData,
        // fmt. CTAP2 uses the length-first ordering of §4.2.3, which puts the
        // short one in front — and length-first is what every attestation
        // object on the wire is written in.
        appendix_a(
            map([
                (CborKey::Text("fmt".into()), Cbor::Text("none".into())),
                (CborKey::Text("attStmt".into()), map([])),
                (CborKey::Text("authData".into()), Cbor::Bytes(vec![7])),
            ]),
            "a363666d74646e6f6e656761747453746d74a06861757468446174614107",
        );
    }

    #[test]
    fn empty_containers_encode_to_a_single_byte() {
        // Not hypothetical: an attestation of format "none" carries an empty
        // map for attStmt, and a zero-length byte string is what a credential
        // with no extension data has.
        let empties =
            [map([]), Cbor::Array(Vec::new()), Cbor::Bytes(Vec::new()), Cbor::Text(String::new())];
        for value in empties {
            let encoded = value.to_bytes();
            assert_eq!(encoded.len(), 1, "{value:?} encoded to {}", crate::ceremony::hex(&encoded));
            assert_eq!(Cbor::parse(&encoded).unwrap(), value);
        }
    }

    #[test]
    fn undefined_is_the_one_value_that_does_not_come_back_as_it_went_in() {
        // The decoder folds 23 (undefined) into `Null`, so it leaves again as
        // 22 (null). Nothing in WebAuthn distinguishes the two, and writing it
        // down here is cheaper than someone rediscovering it against a
        // fixture.
        assert_eq!(Cbor::parse(&[0xf7]).unwrap(), Cbor::Null);
        assert_eq!(Cbor::Null.to_bytes(), from_hex("f6"));
    }

    #[test]
    fn a_real_attestation_object_re_encodes_byte_for_byte() {
        // The fake authenticator writes its own CBOR by hand, the way a real
        // one does: text keys in canonical order, a COSE key nested inside the
        // authenticator data inside a byte string. Nothing below is this
        // encoder's own opinion, so a disagreement about key order or about
        // the shortest length for a hundred-odd bytes shows up here as
        // different bytes.
        let device = crate::ceremony::fake::Authenticator::new(3);
        let auth_data =
            device.authenticator_data("example.test", crate::ceremony::fake::REGISTRATION_FLAGS, 7);

        for (format, entries) in [("none", 0), ("packed", 2)] {
            let object = device.attestation_object(format, entries, &auth_data);
            assert_eq!(
                Cbor::parse(&object).unwrap().to_bytes(),
                object,
                "{format} attestation did not re-encode to itself"
            );
            // And the order really is the one the `BTreeMap` would not have
            // produced: "attStmt" sorts before "fmt" in Rust, and after it
            // here.
            assert!(object.starts_with(b"\xa3\x63fmt"), "fmt must lead");
        }
    }
}
