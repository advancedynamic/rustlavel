//! A self-contained JSON implementation: value model, parser, and serializer.
//!
//! The framework needs JSON in three places (responses, config, and later the
//! AI/MCP packages), so it lives in core rather than pulling in serde.

use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// A parsed JSON value.
///
/// Object keys are kept in a `BTreeMap` so serialization is deterministic,
/// which keeps test assertions and HTTP caching stable.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

impl Json {
    /// Build an object from pairs: `Json::object([("id", 1.into())])`.
    pub fn object<K: Into<String>, I: IntoIterator<Item = (K, Json)>>(pairs: I) -> Self {
        Json::Object(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// Look up a nested value with a dotted path: `value.get("user.name")`.
    ///
    /// Numeric segments index into arrays, so `items.0.id` works too.
    pub fn get(&self, path: &str) -> Option<&Json> {
        let mut current = self;
        for segment in path.split('.') {
            current = match current {
                Json::Object(map) => map.get(segment)?,
                Json::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(current)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        self.as_f64().map(|n| n as i64)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&BTreeMap<String, Json>> {
        match self {
            Json::Object(map) => Some(map),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    /// Serialize with two-space indentation, for config files and debug output.
    pub fn to_string_pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, Some(2), 0);
        out
    }

    /// A rough byte count, so the buffer is sized once instead of grown.
    ///
    /// Deliberately an estimate: an exact count means walking the tree twice,
    /// and being a little over or under costs one reallocation at worst, where
    /// starting from nothing costs a dozen.
    fn estimated_len(&self) -> usize {
        match self {
            Json::Null => 4,
            Json::Bool(true) => 4,
            Json::Bool(false) => 5,
            // Enough for any i64, and most numbers are far shorter.
            Json::Number(_) => 20,
            // Two quotes, and room for a few escapes without regrowing.
            Json::String(s) => s.len() + 8,
            Json::Array(items) => {
                2 + items.iter().map(Json::estimated_len).sum::<usize>() + items.len()
            }
            Json::Object(map) => {
                2 + map
                    .iter()
                    .map(|(key, value)| key.len() + 4 + value.estimated_len())
                    .sum::<usize>()
                    + map.len()
            }
        }
    }

    fn write(&self, out: &mut String, indent: Option<usize>, depth: usize) {
        // Borrowed, not built. The compact form is the one every API response
        // takes, and the old code allocated three Strings per node at every
        // depth only to push them as empty.
        let newline = if indent.is_some() { "\n" } else { "" };
        let colon = if indent.is_some() { ": " } else { ":" };
        let (pad, pad_close) = match indent {
            Some(width) => (" ".repeat(width * (depth + 1)), " ".repeat(width * depth)),
            None => (String::new(), String::new()),
        };

        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Number(n) => {
                if n.is_finite() {
                    // Integral values render without a trailing ".0" so ids stay ids.
                    if n.fract() == 0.0 && n.abs() < 1e15 {
                        push_integer(out, *n as i64);
                    } else {
                        let _ = write!(out, "{n}");
                    }
                } else {
                    out.push_str("null");
                }
            }
            Json::String(s) => escape_into(s, out),
            Json::Array(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(newline);
                    out.push_str(&pad);
                    item.write(out, indent, depth + 1);
                }
                out.push_str(newline);
                out.push_str(&pad_close);
                out.push(']');
            }
            Json::Object(map) => {
                if map.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                for (i, (key, value)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(newline);
                    out.push_str(&pad);
                    escape_into(key, out);
                    out.push_str(colon);
                    value.write(out, indent, depth + 1);
                }
                out.push_str(newline);
                out.push_str(&pad_close);
                out.push('}');
            }
        }
    }

    /// Parse JSON text.
    pub fn parse(input: &str) -> Result<Json> {
        let mut parser = Parser { bytes: input.as_bytes(), pos: 0 };
        parser.skip_whitespace();
        let value = parser.value()?;
        parser.skip_whitespace();
        if parser.pos < parser.bytes.len() {
            return Err(parser.error("unexpected trailing characters"));
        }
        Ok(value)
    }
}

/// Compact JSON text. `to_string()` comes from here.
impl std::fmt::Display for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::with_capacity(self.estimated_len());
        self.write(&mut out, None, 0);
        f.write_str(&out)
    }
}

/// `\u0000` through `\u001f`, so a control character costs a lookup rather
/// than a trip through the formatting machinery.
const CONTROL_ESCAPES: [&str; 32] = [
    "\\u0000", "\\u0001", "\\u0002", "\\u0003", "\\u0004", "\\u0005", "\\u0006", "\\u0007",
    "\\u0008", "\\u0009", "\\u000a", "\\u000b", "\\u000c", "\\u000d", "\\u000e", "\\u000f",
    "\\u0010", "\\u0011", "\\u0012", "\\u0013", "\\u0014", "\\u0015", "\\u0016", "\\u0017",
    "\\u0018", "\\u0019", "\\u001a", "\\u001b", "\\u001c", "\\u001d", "\\u001e", "\\u001f",
];

/// Write a string as a JSON string literal.
///
/// Scans bytes and copies whole clean runs, rather than pushing one character
/// at a time. Every byte that needs escaping is ASCII, and no byte of a
/// multi-byte UTF-8 sequence is ASCII, so a byte scan cannot split a character
/// — which is what makes the run-at-a-time copy safe as well as faster.
fn escape_into(s: &str, out: &mut String) {
    out.push('"');

    let bytes = s.as_bytes();
    let mut clean_from = 0;

    for (index, &byte) in bytes.iter().enumerate() {
        let replacement: &str = match byte {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0c => "\\f",
            // Escaping `<` keeps embedded JSON from closing a <script> tag.
            b'<' => "\\u003c",
            0x00..=0x1f => CONTROL_ESCAPES[byte as usize],
            _ => continue,
        };

        out.push_str(&s[clean_from..index]);
        out.push_str(replacement);
        clean_from = index + 1;
    }

    out.push_str(&s[clean_from..]);
    out.push('"');
}

/// Decimal digits without going through `core::fmt`.
///
/// Every id, count and timestamp in a JSON response comes through here, and
/// `write!` costs far more than the arithmetic does.
fn push_integer(out: &mut String, value: i64) {
    if value == 0 {
        out.push('0');
        return;
    }

    let mut digits = [0u8; 20];
    let mut index = digits.len();
    // Unsigned, so i64::MIN does not overflow when its sign is removed.
    let mut magnitude = value.unsigned_abs();

    while magnitude > 0 {
        index -= 1;
        digits[index] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
    }
    if value < 0 {
        index -= 1;
        digits[index] = b'-';
    }

    out.push_str(std::str::from_utf8(&digits[index..]).unwrap_or("0"));
}

impl From<bool> for Json {
    fn from(v: bool) -> Self {
        Json::Bool(v)
    }
}

impl From<String> for Json {
    fn from(v: String) -> Self {
        Json::String(v)
    }
}

impl From<&str> for Json {
    fn from(v: &str) -> Self {
        Json::String(v.to_string())
    }
}

impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(v: Option<T>) -> Self {
        v.map_or(Json::Null, Into::into)
    }
}

impl<T: Into<Json>> From<Vec<T>> for Json {
    fn from(v: Vec<T>) -> Self {
        Json::Array(v.into_iter().map(Into::into).collect())
    }
}

macro_rules! impl_from_number {
    ($($t:ty),*) => {
        $(impl From<$t> for Json {
            fn from(v: $t) -> Self {
                Json::Number(v as f64)
            }
        })*
    };
}
impl_from_number!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, f32, f64);

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn error(&self, message: &str) -> Error {
        // Recomputing line/column on the error path keeps the hot path free of bookkeeping.
        let consumed = &self.bytes[..self.pos.min(self.bytes.len())];
        let line = consumed.iter().filter(|b| **b == b'\n').count() + 1;
        let column = consumed.iter().rposition(|b| *b == b'\n').map_or(self.pos, |i| self.pos - i - 1) + 1;
        Error::Json { line, column, message: message.to_string() }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<()> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected `{}`", byte as char)))
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.error("invalid literal"))
        }
    }

    fn value(&mut self) -> Result<Json> {
        match self.peek() {
            Some(b'n') => self.literal("null", Json::Null),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'"') => self.string().map(Json::String),
            Some(b'[') => self.array(),
            Some(b'{') => self.object(),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("unexpected end of input")),
        }
    }

    fn array(&mut self) -> Result<Json> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.value()?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(self.error("expected `,` or `]`")),
            }
        }
    }

    fn object(&mut self) -> Result<Json> {
        self.expect(b'{')?;
        let mut map = BTreeMap::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(map));
        }
        loop {
            self.skip_whitespace();
            let key = self.string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            self.skip_whitespace();
            map.insert(key, self.value()?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(map));
                }
                _ => return Err(self.error("expected `,` or `}`")),
            }
        }
    }

    fn number(&mut self) -> Result<Json> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')) {
            self.pos += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.pos])
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .map(Json::Number)
            .ok_or_else(|| self.error("invalid number"))
    }

    fn string(&mut self) -> Result<String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.error("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let escape = self.peek().ok_or_else(|| self.error("unterminated escape"))?;
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return Err(self.error("invalid escape sequence")),
                    }
                }
                Some(_) => {
                    // Copy the whole UTF-8 sequence in one go.
                    let start = self.pos;
                    while let Some(b) = self.peek() {
                        if b == b'"' || b == b'\\' {
                            break;
                        }
                        self.pos += 1;
                    }
                    match std::str::from_utf8(&self.bytes[start..self.pos]) {
                        Ok(chunk) => out.push_str(chunk),
                        Err(_) => return Err(self.error("invalid UTF-8 in string")),
                    }
                }
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char> {
        let high = self.hex4()?;
        // Surrogate pair: the low half arrives as a second \u escape.
        if (0xD800..0xDC00).contains(&high) {
            if self.peek() == Some(b'\\') && self.bytes.get(self.pos + 1) == Some(&b'u') {
                self.pos += 2;
                let low = self.hex4()?;
                if (0xDC00..0xE000).contains(&low) {
                    let combined = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
                    return char::from_u32(combined).ok_or_else(|| self.error("invalid code point"));
                }
            }
            return Err(self.error("unpaired surrogate"));
        }
        char::from_u32(high).ok_or_else(|| self.error("invalid code point"))
    }

    fn hex4(&mut self) -> Result<u32> {
        if self.pos + 4 > self.bytes.len() {
            return Err(self.error("truncated \\u escape"));
        }
        let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .ok()
            .and_then(|s| u32::from_str_radix(s, 16).ok())
            .ok_or_else(|| self.error("invalid \\u escape"))?;
        self.pos += 4;
        Ok(hex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_reserializes_a_document() {
        let source = r#"{"name":"Rustlavel","stars":42,"tags":["web","rust"],"nested":{"ok":true,"nothing":null}}"#;
        let value = Json::parse(source).unwrap();

        assert_eq!(value.get("name").unwrap().as_str(), Some("Rustlavel"));
        assert_eq!(value.get("stars").unwrap().as_i64(), Some(42));
        assert_eq!(value.get("tags.1").unwrap().as_str(), Some("rust"));
        assert_eq!(value.get("nested.ok").unwrap().as_bool(), Some(true));
        assert!(value.get("nested.nothing").unwrap().is_null());
        assert!(value.get("nested.missing").is_none());

        // Keys come back sorted, not in source order — serialization is
        // deterministic so tests and HTTP caching stay stable.
        assert_eq!(
            value.to_string(),
            r#"{"name":"Rustlavel","nested":{"nothing":null,"ok":true},"stars":42,"tags":["web","rust"]}"#
        );
        assert_eq!(Json::parse(&value.to_string()).unwrap(), value);
    }

    #[test]
    fn handles_escapes_and_surrogate_pairs() {
        let value = Json::parse(r#""line\nbreak é 🚀""#).unwrap();
        assert_eq!(value.as_str(), Some("line\nbreak é 🚀"));

        // `<` is escaped so JSON can be embedded in HTML without closing a script tag.
        let embedded = Json::from("</script>").to_string();
        assert_eq!(embedded, "\"\\u003c/script>\"");
        assert_eq!(Json::parse(&embedded).unwrap().as_str(), Some("</script>"));
    }

    #[test]
    fn integral_numbers_do_not_grow_a_decimal_point() {
        assert_eq!(Json::from(42).to_string(), "42");
        assert_eq!(Json::from(1.5).to_string(), "1.5");
    }

    #[test]
    fn reports_position_of_a_syntax_error() {
        let err = Json::parse("{\n  \"a\": tru\n}").unwrap_err();
        match err {
            Error::Json { line, .. } => assert_eq!(line, 2),
            other => panic!("expected a JSON error, got {other:?}"),
        }
    }

    #[test]
    fn pretty_printing_round_trips() {
        let value = Json::parse(r#"{"a":[1,2],"b":{}}"#).unwrap();
        let pretty = value.to_string_pretty();
        assert!(pretty.contains("\n  \"a\": ["));
        assert_eq!(Json::parse(&pretty).unwrap(), value);
    }
}
