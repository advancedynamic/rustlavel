//! Base64, needed by the opening handshake.
//!
//! `Sec-WebSocket-Key` arrives base64-encoded and `Sec-WebSocket-Accept` goes
//! back base64-encoded, so the crate needs both directions. Fifty lines is
//! cheaper than a dependency, and a WebSocket crate that dragged in a base64
//! crate would be the only thing in the framework that did.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let bits = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);

        out.push(ALPHABET[(bits >> 18 & 0x3f) as usize] as char);
        out.push(ALPHABET[(bits >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(bits >> 6 & 0x3f) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(bits & 0x3f) as usize] as char } else { '=' });
    }

    out
}

pub fn decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;

    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
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
    fn matches_the_rfc_4648_vectors() {
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
    fn round_trips_the_sixteen_random_bytes_a_handshake_key_carries() {
        let key: Vec<u8> = (0..16).map(|i| i * 17).collect();
        assert_eq!(decode(&encode(&key)).unwrap(), key);
    }

    #[test]
    fn rejects_characters_outside_the_alphabet() {
        assert!(decode("not base64!").is_none());
    }
}
