//! Just enough BER to speak LDAP.
//!
//! LDAP v3 is ASN.1 BER over a socket (RFC 4511 §5.1), so this has to exist.
//! It is a serialisation format rather than cryptography, so rule one applies
//! and it is written here — but the subset is deliberately small, because
//! RFC 4511 already made it small: **only the definite form of length encoding
//! is used**, every tag LDAP defines is below 31, and nothing in the protocol
//! needs REAL, BIT STRING or a tag longer than one octet.
//!
//! Everything outside that subset is refused rather than parsed. A decoder
//! that accepts more than the specification allows is a decoder that accepts
//! things the specification was trying to keep out, and the bytes here arrive
//! from a socket that an attacker on the path controls completely.
//!
//! What is refused, and why:
//!
//! * **Indefinite lengths.** RFC 4511 forbids them outright. A parser that
//!   supports them has to scan for an end-of-contents marker, which is how a
//!   message's real boundary and its declared boundary come apart.
//! * **Multi-byte tags.** LDAP's highest tag number is 25. A high-tag-number
//!   form is either a bug or a probe.
//! * **A length longer than the bytes present.** Checked *before* anything is
//!   allocated, so a four-byte header claiming four gigabytes costs nothing.
//! * **Non-minimal lengths and integers.** X.690 requires the shortest form for
//!   both. Two spellings of the same number is how two parsers end up
//!   disagreeing about the same bytes.
//! * **Nesting past [`MAX_DEPTH`].** A filter is a recursive type, so without a
//!   limit a few dozen bytes of `(!(!(!(…))))` recurse until the stack gives
//!   out.

use rustlavel_core::{Error, Result};

/// The class bits of an identifier octet.
pub const UNIVERSAL: u8 = 0x00;
/// Application-class tags: LDAP's protocol operations.
pub const APPLICATION: u8 = 0x40;
/// Context-class tags: the fields inside an operation, and filter choices.
pub const CONTEXT: u8 = 0x80;
/// The constructed bit — set when the contents are themselves elements.
pub const CONSTRUCTED: u8 = 0x20;

pub const BOOLEAN: u8 = 0x01;
pub const INTEGER: u8 = 0x02;
pub const OCTET_STRING: u8 = 0x04;
pub const NULL: u8 = 0x05;
pub const ENUMERATED: u8 = 0x0a;
pub const SEQUENCE: u8 = 0x30;
pub const SET: u8 = 0x31;

/// The identifier octet for `[APPLICATION n]`.
pub const fn application(number: u8, constructed: bool) -> u8 {
    APPLICATION | if constructed { CONSTRUCTED } else { 0 } | number
}

/// The identifier octet for `[n]` — a context-specific tag.
pub const fn context(number: u8, constructed: bool) -> u8 {
    CONTEXT | if constructed { CONSTRUCTED } else { 0 } | number
}

/// How deep a nested structure may go.
///
/// A realistic search filter is three or four levels; the deepest thing a
/// directory sends back is a search result entry at five. Anything approaching
/// this is hostile, and `Filter` is recursive on both sides of the wire, so
/// the limit is what stands between a short message and a blown stack.
pub const MAX_DEPTH: usize = 32;

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// A BER element under construction.
///
/// Nested elements are built into a temporary encoder and copied in once their
/// length is known. BER puts the length before the contents, so something has
/// to give: either the bytes move, or the length does. An LDAP message is a few
/// hundred bytes and a handful of levels deep, so the copy is not worth
/// out-thinking, and building inside-out is the version a reader can check.
#[derive(Default)]
pub struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Encoder {
        Encoder::default()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// One element: identifier octet, definite length, contents verbatim.
    pub fn element(&mut self, tag: u8, contents: &[u8]) -> &mut Encoder {
        self.bytes.push(tag);
        encode_length(contents.len(), &mut self.bytes);
        self.bytes.extend_from_slice(contents);
        self
    }

    /// A constructed element whose contents are written by `body`.
    pub fn constructed(&mut self, tag: u8, body: impl FnOnce(&mut Encoder)) -> &mut Encoder {
        debug_assert!(tag & CONSTRUCTED != 0, "a constructed element needs its constructed bit");
        let mut inner = Encoder::new();
        body(&mut inner);
        self.element(tag, &inner.bytes)
    }

    pub fn sequence(&mut self, body: impl FnOnce(&mut Encoder)) -> &mut Encoder {
        self.constructed(SEQUENCE, body)
    }

    pub fn set(&mut self, body: impl FnOnce(&mut Encoder)) -> &mut Encoder {
        self.constructed(SET, body)
    }

    pub fn integer(&mut self, value: i64) -> &mut Encoder {
        self.tagged_integer(INTEGER, value)
    }

    pub fn enumerated(&mut self, value: i64) -> &mut Encoder {
        self.tagged_integer(ENUMERATED, value)
    }

    /// An INTEGER under some other tag — LDAP tags a few of them contextually.
    pub fn tagged_integer(&mut self, tag: u8, value: i64) -> &mut Encoder {
        self.element(tag, &integer_contents(value))
    }

    /// TRUE is encoded as `0xff`.
    ///
    /// BER would accept any non-zero octet, but there is no reason to invent a
    /// second spelling on the way out, and DER-shaped output is what every
    /// directory expects to see.
    pub fn boolean(&mut self, value: bool) -> &mut Encoder {
        self.element(BOOLEAN, &[if value { 0xff } else { 0x00 }])
    }

    pub fn octet_string(&mut self, contents: &[u8]) -> &mut Encoder {
        self.element(OCTET_STRING, contents)
    }

    /// An `LDAPString`, which RFC 4511 defines as UTF-8 in an OCTET STRING.
    pub fn string(&mut self, value: &str) -> &mut Encoder {
        self.element(OCTET_STRING, value.as_bytes())
    }

    /// An OCTET STRING carrying a context-specific tag instead of its own —
    /// implicit tagging, which is what RFC 4511's ASN.1 module asks for.
    pub fn tagged_string(&mut self, tag: u8, value: &str) -> &mut Encoder {
        self.element(tag, value.as_bytes())
    }

    pub fn null(&mut self) -> &mut Encoder {
        self.element(NULL, &[])
    }

    /// Append bytes that are already a complete element.
    pub fn raw(&mut self, encoded: &[u8]) -> &mut Encoder {
        self.bytes.extend_from_slice(encoded);
        self
    }
}

/// The definite length, in the fewest octets that can hold it.
fn encode_length(length: usize, out: &mut Vec<u8>) {
    if length < 0x80 {
        out.push(length as u8);
        return;
    }

    let bytes = length.to_be_bytes();
    let first = bytes.iter().position(|&byte| byte != 0).unwrap_or(bytes.len() - 1);
    let significant = &bytes[first..];

    out.push(0x80 | significant.len() as u8);
    out.extend_from_slice(significant);
}

/// Two's complement, in the fewest octets — X.690 §8.3.2.
fn integer_contents(value: i64) -> Vec<u8> {
    let mut bytes = value.to_be_bytes().to_vec();

    while bytes.len() > 1 {
        // A leading 0x00 in front of a positive byte, or a leading 0xff in
        // front of a negative one, carries no information.
        let redundant = (bytes[0] == 0x00 && bytes[1] & 0x80 == 0)
            || (bytes[0] == 0xff && bytes[1] & 0x80 != 0);
        if !redundant {
            break;
        }
        bytes.remove(0);
    }

    bytes
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Read an identifier octet and a definite length.
///
/// Returns `Ok(None)` when the header itself is not all here yet — the framing
/// case, where the caller should read more from the socket. A header that is
/// present but *wrong* is an error, not a request for more bytes: waiting for
/// more of a message that has already told you it is malformed is how a client
/// hangs on a hostile server.
fn read_header(bytes: &[u8]) -> Result<Option<(u8, usize, usize)>> {
    let Some(&tag) = bytes.first() else {
        return Ok(None);
    };

    if tag & 0x1f == 0x1f {
        return Err(Error::msg(
            "BER high-tag-number form is refused: LDAP's highest tag number is 25, so a \
             multi-byte tag is not something a directory has any way to mean",
        ));
    }

    let Some(&first) = bytes.get(1) else {
        return Ok(None);
    };

    if first == 0x80 {
        return Err(Error::msg(
            "BER indefinite length is refused: RFC 4511 §5.1 uses the definite form only, and a \
             message whose end has to be searched for is a message whose end can be moved",
        ));
    }

    if first < 0x80 {
        return Ok(Some((tag, 2, first as usize)));
    }

    let count = (first & 0x7f) as usize;
    if count > 4 {
        return Err(Error::msg(format!(
            "BER length is spread over {count} octets. Nothing in an LDAP message is four \
             gigabytes long, so this is a header worth refusing rather than believing"
        )));
    }

    if bytes.len() < 2 + count {
        return Ok(None);
    }

    let mut length = 0usize;
    for &byte in &bytes[2..2 + count] {
        length = (length << 8) | byte as usize;
    }

    if length < 0x80 || bytes[2] == 0 {
        return Err(Error::msg(format!(
            "BER length {length} is not in its shortest form. X.690 requires one encoding per \
             number; a padded one is a second spelling, and two spellings is how two parsers \
             come to disagree about the same bytes"
        )));
    }

    Ok(Some((tag, 2 + count, length)))
}

/// The total size of the element starting at `bytes[0]`, header included.
///
/// `Ok(None)` means "not yet" — the header is incomplete, or the contents have
/// not all arrived. This is the framing primitive the connection reads with, so
/// it is also where the size limit is enforced: the length is checked against
/// `limit` before the caller is ever told to keep reading, which is what stops
/// a server from asking a client to buffer a gigabyte.
pub fn element_size(bytes: &[u8], limit: usize) -> Result<Option<usize>> {
    let Some((_, header, length)) = read_header(bytes)? else {
        return Ok(None);
    };

    let total = header
        .checked_add(length)
        .ok_or_else(|| Error::msg("BER element length overflows a usize"))?;

    if total > limit {
        return Err(Error::msg(format!(
            "the directory announced a {total} byte message; the limit is {limit}. Raise it \
             deliberately with `LdapConfig::max_message_size` if a legitimate search really \
             does return that much"
        )));
    }

    if bytes.len() < total {
        return Ok(None);
    }

    Ok(Some(total))
}

/// A cursor over a sequence of BER elements.
///
/// LDAP's shapes are all known ahead of time, so this reads them positionally
/// rather than building a value tree: `decoder.integer()?` then
/// `decoder.nested(tag)?`, mirroring the ASN.1 in RFC 4511 line for line.
pub struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
    depth: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(bytes: &'a [u8]) -> Decoder<'a> {
        Decoder { bytes, position: 0, depth: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.position >= self.bytes.len()
    }

    pub fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.position..]
    }

    /// The identifier octet of the next element, without consuming it.
    ///
    /// Optional fields are how LDAP says "and maybe a referral", so every
    /// optional read starts here.
    pub fn peek_tag(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    pub fn has_tag(&self, tag: u8) -> bool {
        self.peek_tag() == Some(tag)
    }

    /// The next element: its tag and its contents.
    pub fn element(&mut self) -> Result<(u8, &'a [u8])> {
        let rest = &self.bytes[self.position..];

        let (tag, header, length) = read_header(rest)?
            .ok_or_else(|| Error::msg("BER ended in the middle of an element header"))?;

        let end = header
            .checked_add(length)
            .ok_or_else(|| Error::msg("BER element length overflows a usize"))?;

        if end > rest.len() {
            return Err(Error::msg(format!(
                "a BER element claims {length} content bytes but only {} remain in the message",
                rest.len() - header
            )));
        }

        self.position += end;
        Ok((tag, &rest[header..end]))
    }

    /// The contents of the next element, which must carry exactly `tag`.
    pub fn expect(&mut self, tag: u8) -> Result<&'a [u8]> {
        let found = self.peek_tag();
        let (actual, contents) = self.element()?;
        if actual != tag {
            return Err(Error::msg(format!(
                "expected a BER element tagged {tag:#04x} but found {:#04x}",
                found.unwrap_or(actual)
            )));
        }
        Ok(contents)
    }

    /// Step into a constructed element, one level deeper.
    pub fn nested(&mut self, tag: u8) -> Result<Decoder<'a>> {
        if self.depth + 1 > MAX_DEPTH {
            return Err(Error::msg(format!(
                "BER nested deeper than {MAX_DEPTH} levels; a filter that deep is not one a \
                 directory built"
            )));
        }
        let contents = self.expect(tag)?;
        Ok(Decoder { bytes: contents, position: 0, depth: self.depth + 1 })
    }

    pub fn sequence(&mut self) -> Result<Decoder<'a>> {
        self.nested(SEQUENCE)
    }

    pub fn set(&mut self) -> Result<Decoder<'a>> {
        self.nested(SET)
    }

    pub fn integer(&mut self) -> Result<i64> {
        integer_from(self.expect(INTEGER)?)
    }

    pub fn enumerated(&mut self) -> Result<i64> {
        integer_from(self.expect(ENUMERATED)?)
    }

    pub fn boolean(&mut self) -> Result<bool> {
        match self.expect(BOOLEAN)? {
            [byte] => Ok(*byte != 0),
            other => Err(Error::msg(format!(
                "a BER BOOLEAN has {} content octets; it must have exactly one",
                other.len()
            ))),
        }
    }

    pub fn octet_string(&mut self) -> Result<&'a [u8]> {
        self.expect(OCTET_STRING)
    }

    /// An `LDAPString` — an OCTET STRING that RFC 4511 promises is UTF-8.
    ///
    /// Attribute *values* are not read through here: they are arbitrary octets
    /// (a `jpegPhoto` is not text), and forcing them through UTF-8 would turn a
    /// perfectly good photograph into an error. Names, DNs and diagnostics are.
    pub fn string(&mut self) -> Result<String> {
        let bytes = self.octet_string()?;
        utf8(bytes)
    }

    /// An implicitly tagged `LDAPString`.
    pub fn tagged_string(&mut self, tag: u8) -> Result<String> {
        let bytes = self.expect(tag)?;
        utf8(bytes)
    }

    /// Consume the next element and throw it away.
    ///
    /// Used for fields a newer directory may add that this client does not
    /// read. Skipping a known-shaped element is safe; skipping to a byte offset
    /// is not, which is why this goes through the same header parser.
    pub fn skip(&mut self) -> Result<()> {
        self.element().map(|_| ())
    }

    /// Refuse anything left over.
    ///
    /// Trailing bytes mean the message was not what it claimed to be, and
    /// ignoring them is how one message smuggles a second past a parser.
    pub fn finish(&self, what: &str) -> Result<()> {
        if !self.is_empty() {
            return Err(Error::msg(format!(
                "{what} had {} trailing bytes after a complete value",
                self.bytes.len() - self.position
            )));
        }
        Ok(())
    }
}

/// A signed integer from its content octets.
pub fn integer_from(contents: &[u8]) -> Result<i64> {
    let Some((&first, rest)) = contents.split_first() else {
        return Err(Error::msg("a BER INTEGER has no content octets; it must have at least one"));
    };

    if contents.len() > 8 {
        return Err(Error::msg(format!(
            "a BER INTEGER of {} octets does not fit in an i64; nothing LDAP defines is that \
             wide",
            contents.len()
        )));
    }

    // X.690 §8.3.2: the first nine bits may not be all zero or all one. That is
    // the rule that gives every integer exactly one encoding, and it is a BER
    // rule, not just a DER one — so a padded integer is malformed, not lenient.
    if let Some(&second) = rest.first()
        && ((first == 0x00 && second & 0x80 == 0) || (first == 0xff && second & 0x80 != 0))
    {
        return Err(Error::msg(
            "a BER INTEGER has a redundant leading octet; X.690 requires the shortest form",
        ));
    }

    let mut value = first as i8 as i64;
    for &byte in rest {
        value = (value << 8) | byte as i64;
    }
    Ok(value)
}

fn utf8(bytes: &[u8]) -> Result<String> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| Error::msg("an LDAPString from the directory is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(body: impl FnOnce(&mut Encoder)) -> Vec<u8> {
        let mut encoder = Encoder::new();
        body(&mut encoder);
        encoder.into_bytes()
    }

    /// The error's text, for results whose `Ok` type has no `Debug` — which is
    /// most of them here, on purpose.
    fn failure<T>(result: Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected this to fail"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn integers_use_the_shortest_two_s_complement_form() {
        // The boundaries are the whole story: 127 fits in one octet, 128 needs
        // a zero in front of it so it does not read as -128.
        assert_eq!(encoded(|e| { e.integer(0); }), vec![0x02, 0x01, 0x00]);
        assert_eq!(encoded(|e| { e.integer(127); }), vec![0x02, 0x01, 0x7f]);
        assert_eq!(encoded(|e| { e.integer(128); }), vec![0x02, 0x02, 0x00, 0x80]);
        assert_eq!(encoded(|e| { e.integer(256); }), vec![0x02, 0x02, 0x01, 0x00]);
        assert_eq!(encoded(|e| { e.integer(-1); }), vec![0x02, 0x01, 0xff]);
        assert_eq!(encoded(|e| { e.integer(-128); }), vec![0x02, 0x01, 0x80]);
        assert_eq!(encoded(|e| { e.integer(-129); }), vec![0x02, 0x02, 0xff, 0x7f]);
        assert_eq!(
            encoded(|e| { e.integer(i64::MAX); }),
            vec![0x02, 0x08, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
    }

    #[test]
    fn integers_round_trip_through_their_encoding() {
        for value in [0, 1, -1, 127, 128, -128, -129, 255, 256, 65_535, 65_536, -65_536] {
            let bytes = encoded(|e| {
                e.integer(value);
            });
            assert_eq!(Decoder::new(&bytes).integer().unwrap(), value, "{value}");
        }
        for value in [i64::MIN, i64::MAX, i32::MIN as i64, i32::MAX as i64] {
            let bytes = encoded(|e| {
                e.integer(value);
            });
            assert_eq!(Decoder::new(&bytes).integer().unwrap(), value, "{value}");
        }
    }

    #[test]
    fn a_padded_integer_is_refused_rather_than_read_leniently() {
        // 0x02 0x02 0x00 0x7f is 127 written the long way. Reading it happily
        // means this parser and the directory's disagree about which byte
        // strings are legal, which is exactly the gap worth closing.
        let error = Decoder::new(&[0x02, 0x02, 0x00, 0x7f]).integer().unwrap_err().to_string();
        assert!(error.contains("redundant leading octet"), "got {error}");

        let error = Decoder::new(&[0x02, 0x02, 0xff, 0x80]).integer().unwrap_err().to_string();
        assert!(error.contains("redundant leading octet"), "got {error}");

        // Empty contents are not zero; they are nothing.
        let error = Decoder::new(&[0x02, 0x00]).integer().unwrap_err().to_string();
        assert!(error.contains("no content octets"), "got {error}");
    }

    #[test]
    fn booleans_strings_and_sequences_encode_the_way_x690_says() {
        assert_eq!(encoded(|e| { e.boolean(true); }), vec![0x01, 0x01, 0xff]);
        assert_eq!(encoded(|e| { e.boolean(false); }), vec![0x01, 0x01, 0x00]);
        assert_eq!(encoded(|e| { e.string("abc"); }), vec![0x04, 0x03, b'a', b'b', b'c']);
        assert_eq!(encoded(|e| { e.string(""); }), vec![0x04, 0x00]);
        assert_eq!(encoded(|e| { e.sequence(|_| {}); }), vec![0x30, 0x00]);
        assert_eq!(encoded(|e| { e.enumerated(49); }), vec![0x0a, 0x01, 0x31]);

        let nested = encoded(|e| {
            e.sequence(|body| {
                body.integer(1);
                body.string("x");
            });
        });
        assert_eq!(nested, vec![0x30, 0x06, 0x02, 0x01, 0x01, 0x04, 0x01, b'x']);
    }

    #[test]
    fn long_lengths_use_as_few_octets_as_the_number_needs() {
        // 127 content bytes fits the short form; 128 does not, and the boundary
        // is the only interesting place in the whole length encoding.
        let short = encoded(|e| {
            e.octet_string(&[0u8; 127]);
        });
        assert_eq!(&short[..2], &[0x04, 0x7f]);

        let long = encoded(|e| {
            e.octet_string(&[0u8; 128]);
        });
        assert_eq!(&long[..3], &[0x04, 0x81, 0x80]);

        let longer = encoded(|e| {
            e.octet_string(&[0u8; 300]);
        });
        assert_eq!(&longer[..4], &[0x04, 0x82, 0x01, 0x2c]);

        assert_eq!(Decoder::new(&longer).octet_string().unwrap().len(), 300);
    }

    #[test]
    fn a_padded_length_is_refused() {
        // 0x04 0x82 0x00 0x03 is "three" written in two octets.
        let error =
            Decoder::new(&[0x04, 0x82, 0x00, 0x03, 1, 2, 3]).octet_string().unwrap_err().to_string();
        assert!(error.contains("shortest form"), "got {error}");

        // And so is the long form used for a number the short form could hold.
        let error =
            Decoder::new(&[0x04, 0x81, 0x03, 1, 2, 3]).octet_string().unwrap_err().to_string();
        assert!(error.contains("shortest form"), "got {error}");
    }

    #[test]
    fn indefinite_lengths_are_refused_rather_than_parsed() {
        // RFC 4511 §5.1 forbids them, so a directory cannot legitimately send
        // one — which makes anything that does worth rejecting outright.
        let error = failure(Decoder::new(&[0x30, 0x80, 0x02, 0x01, 0x01, 0x00, 0x00]).sequence());
        assert!(error.contains("indefinite"), "got {error}");
    }

    #[test]
    fn a_multi_byte_tag_is_refused() {
        let error = Decoder::new(&[0x1f, 0x81, 0x00, 0x00]).element().unwrap_err().to_string();
        assert!(error.contains("high-tag-number"), "got {error}");
    }

    #[test]
    fn a_length_longer_than_the_message_is_refused_before_allocating() {
        // The header claims sixteen megabytes; the message is seven bytes.
        // Believing it would allocate first and find out afterwards.
        let error = Decoder::new(&[0x04, 0x84, 0x01, 0x00, 0x00, 0x00, 0x00])
            .octet_string()
            .unwrap_err()
            .to_string();
        assert!(error.contains("only"), "got {error}");
    }

    #[test]
    fn deep_nesting_is_bounded() {
        // MAX_DEPTH + 2 nested SEQUENCEs — a few dozen bytes that would
        // otherwise recurse until the stack gives out.
        let mut bytes = vec![0x30, 0x00];
        for _ in 0..MAX_DEPTH + 2 {
            let mut outer = vec![0x30, bytes.len() as u8];
            outer.extend_from_slice(&bytes);
            bytes = outer;
        }

        fn descend(decoder: &mut Decoder<'_>) -> Result<()> {
            let mut inner = decoder.sequence()?;
            if !inner.is_empty() {
                descend(&mut inner)?;
            }
            Ok(())
        }

        let error = descend(&mut Decoder::new(&bytes)).unwrap_err().to_string();
        assert!(error.contains("nested deeper"), "got {error}");
    }

    #[test]
    fn truncation_anywhere_is_an_error_and_never_a_panic() {
        // Every prefix of a valid message must be refused cleanly — this is the
        // shape of input a hostile server controls completely.
        let complete = encoded(|e| {
            e.sequence(|body| {
                body.integer(7);
                body.string("hello");
                body.boolean(true);
            });
        });

        for length in 0..complete.len() {
            let mut decoder = Decoder::new(&complete[..length]);
            let read = (|| -> Result<()> {
                let mut inner = decoder.sequence()?;
                inner.integer()?;
                inner.string()?;
                inner.boolean()?;
                Ok(())
            })();
            assert!(read.is_err(), "a {length} byte prefix should not parse");
        }
    }

    #[test]
    fn framing_says_not_yet_rather_than_guessing() {
        let message = encoded(|e| {
            e.sequence(|body| {
                body.integer(1);
                body.string("abcdefghij");
            });
        });

        // One byte at a time: nothing is claimed until the whole element is in.
        for length in 0..message.len() {
            assert_eq!(
                element_size(&message[..length], 1 << 20).unwrap(),
                None,
                "{length} bytes should not frame a complete message"
            );
        }
        assert_eq!(element_size(&message, 1 << 20).unwrap(), Some(message.len()));

        // Trailing bytes belong to the next message, not this one.
        let mut two = message.clone();
        two.extend_from_slice(&message);
        assert_eq!(element_size(&two, 1 << 20).unwrap(), Some(message.len()));
    }

    #[test]
    fn framing_refuses_an_oversized_message_before_buffering_it() {
        // A four-byte header announcing sixty-four megabytes. The point of the
        // limit is that this costs nothing: no read loop, no allocation.
        let header = [0x30, 0x84, 0x04, 0x00, 0x00, 0x00];
        let error = element_size(&header, 1 << 20).unwrap_err().to_string();
        assert!(error.contains("the limit is"), "got {error}");
    }

    #[test]
    fn a_wrong_tag_names_both_tags() {
        let bytes = encoded(|e| {
            e.string("not an integer");
        });
        let error = Decoder::new(&bytes).integer().unwrap_err().to_string();
        assert!(error.contains("0x02") && error.contains("0x04"), "got {error}");
    }

    #[test]
    fn trailing_bytes_after_a_value_are_refused() {
        let bytes = encoded(|e| {
            e.sequence(|body| {
                body.integer(1);
                body.integer(2);
            });
        });
        let mut sequence = Decoder::new(&bytes).sequence().unwrap();
        sequence.integer().unwrap();
        assert!(sequence.finish("the sequence").is_err());
        sequence.integer().unwrap();
        assert!(sequence.finish("the sequence").is_ok());
    }

    #[test]
    fn context_and_application_tags_are_built_from_their_parts() {
        // BindRequest is [APPLICATION 0] SEQUENCE, BindResponse [APPLICATION 1],
        // and the simple authentication choice is [0] primitive.
        assert_eq!(application(0, true), 0x60);
        assert_eq!(application(1, true), 0x61);
        assert_eq!(application(2, false), 0x42);
        assert_eq!(application(23, true), 0x77);
        assert_eq!(context(0, false), 0x80);
        assert_eq!(context(0, true), 0xa0);
        assert_eq!(context(7, false), 0x87);
    }

    #[test]
    fn a_non_utf8_ldapstring_is_refused_but_a_raw_value_is_not() {
        let bytes = encoded(|e| {
            e.octet_string(&[0xff, 0xfe]);
        });
        assert!(Decoder::new(&bytes).string().is_err());
        // The same octets read as an attribute value, which they are allowed
        // to be: a photograph is not text.
        assert_eq!(Decoder::new(&bytes).octet_string().unwrap(), &[0xff, 0xfe]);
    }
}
