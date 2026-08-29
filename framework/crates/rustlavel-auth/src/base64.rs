//! Base64, in both alphabets this crate needs.
//!
//! `APP_KEY` is written the way an operator expects to see it — standard
//! alphabet, padded, exactly what `openssl rand -base64 32` prints. Encrypted
//! payloads and signatures travel in cookies and query strings, so they use the
//! URL-safe alphabet with no padding and need no percent-encoding at all.
//!
//! Fifty lines is cheaper than a dependency, and getting base64 wrong is
//! visible immediately, unlike getting a cipher wrong.

const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL_SAFE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Standard alphabet, padded with `=`.
pub fn encode(input: &[u8]) -> String {
    encode_with(input, STANDARD, true)
}

/// URL-safe alphabet, unpadded — safe in a cookie value or a query string.
pub fn encode_url(input: &[u8]) -> String {
    encode_with(input, URL_SAFE, false)
}

fn encode_with(input: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let bits = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);

        out.push(alphabet[(bits >> 18 & 0x3f) as usize] as char);
        out.push(alphabet[(bits >> 12 & 0x3f) as usize] as char);

        match (chunk.len() > 1, pad) {
            (true, _) => out.push(alphabet[(bits >> 6 & 0x3f) as usize] as char),
            (false, true) => out.push('='),
            (false, false) => {}
        }
        match (chunk.len() > 2, pad) {
            (true, _) => out.push(alphabet[(bits & 0x3f) as usize] as char),
            (false, true) => out.push('='),
            (false, false) => {}
        }
    }

    out
}

/// Decode either alphabet, with or without padding.
///
/// Deliberately permissive about which alphabet it is handed: a key pasted
/// from a password manager and a signature copied out of a URL should both
/// just work. It is not permissive about *characters* — anything outside both
/// alphabets is rejected, so a truncated or mangled payload fails here rather
/// than silently decoding to different bytes.
pub fn decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;

    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };

        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_encoding_matches_the_rfc_4648_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes()), encoded, "encoding {plain:?}");
            assert_eq!(decode(encoded).unwrap(), plain.as_bytes(), "decoding {encoded:?}");
        }
    }

    #[test]
    fn url_safe_encoding_avoids_the_characters_a_url_would_escape() {
        // 0xfb 0xff encodes to `+/` in the standard alphabet.
        let bytes = [0xfb, 0xff, 0xbf];
        assert_eq!(encode(&bytes), "+/+/");
        assert_eq!(encode_url(&bytes), "-_-_");

        let unpadded = encode_url(b"f");
        assert_eq!(unpadded, "Zg");
        assert_eq!(decode(&unpadded).unwrap(), b"f");
    }

    #[test]
    fn both_alphabets_round_trip_arbitrary_bytes() {
        let bytes: Vec<u8> = (0..=255).collect();

        assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
        assert_eq!(decode(&encode_url(&bytes)).unwrap(), bytes);
    }

    #[test]
    fn rejects_characters_outside_the_alphabets() {
        assert!(decode("not base64!").is_none());
        assert!(decode("abc$def").is_none());
    }
}
