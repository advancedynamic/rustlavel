//! Percent-encoding and form bodies.
//!
//! OAuth passes secrets through query strings and `application/x-www-form-
//! urlencoded` bodies, so the encoding rules are not cosmetic: a `+` in a token
//! that survives encoding becomes a space on the other side, and the grant
//! fails with an error that points nowhere near the cause.

/// Percent-encode a string for a query value, per RFC 3986's unreserved set.
///
/// Deliberately stricter than encoding "just the dangerous characters": `+`,
/// `/` and `=` all appear in base64 and all change meaning in a query string.
pub fn encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Decode a percent-encoded value, treating `+` as a space.
///
/// The `+` rule belongs to form encoding rather than to URLs generally, but
/// every OAuth provider sends form bodies, so this is the decoder that matches
/// what actually arrives.
pub fn decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match u8::from_str_radix(&input[index + 1..index + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    // A stray `%` is kept rather than dropped: losing it would
                    // silently change the value.
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

/// Build an `application/x-www-form-urlencoded` body from pairs.
pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Parse a query string or form body into pairs.
///
/// Duplicate keys are kept in order rather than collapsed — the caller decides
/// whether a repeated parameter is an attack or a list.
pub fn form_decode(input: &str) -> Vec<(String, String)> {
    input
        .trim_start_matches('?')
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (decode(key), decode(value)),
            None => (decode(pair), String::new()),
        })
        .collect()
}

/// Append a query string to a URL that may already have one.
pub fn append_query(url: &str, query: &str) -> String {
    if query.is_empty() {
        return url.to_string();
    }
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}{query}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_everything_outside_the_unreserved_set() {
        assert_eq!(encode("abcXYZ019-._~"), "abcXYZ019-._~");
        assert_eq!(encode("a b"), "a%20b");
        assert_eq!(encode("a+b/c=d"), "a%2Bb%2Fc%3Dd");
        assert_eq!(encode("https://x.test/cb?a=1"), "https%3A%2F%2Fx.test%2Fcb%3Fa%3D1");
    }

    #[test]
    fn a_plus_in_a_secret_survives_the_round_trip() {
        // The failure this guards against: base64 output containing `+`, sent
        // unencoded, arriving as a space, and the grant being rejected.
        let secret = "s3cr+t/val=ue";
        assert_eq!(decode(&encode(secret)), secret);
    }

    #[test]
    fn decoding_treats_plus_as_a_space_the_way_form_bodies_do() {
        assert_eq!(decode("a+b"), "a b");
        assert_eq!(decode("a%20b"), "a b");
    }

    #[test]
    fn a_malformed_escape_is_kept_rather_than_dropped() {
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("100%zz"), "100%zz");
    }

    #[test]
    fn handles_non_ascii() {
        assert_eq!(encode("ä"), "%C3%A4");
        assert_eq!(decode("%C3%A4"), "ä");
    }

    #[test]
    fn form_bodies_round_trip() {
        let body = form_encode(&[("grant_type", "authorization_code"), ("code", "a b&c")]);
        assert_eq!(body, "grant_type=authorization_code&code=a%20b%26c");

        let parsed = form_decode(&body);
        assert_eq!(parsed[0], ("grant_type".to_string(), "authorization_code".to_string()));
        assert_eq!(parsed[1], ("code".to_string(), "a b&c".to_string()));
    }

    #[test]
    fn a_repeated_parameter_is_visible_rather_than_collapsed() {
        // Two `code` parameters is a request worth rejecting, so the parser
        // must not quietly pick one.
        let parsed = form_decode("code=a&code=b");
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn a_valueless_parameter_parses() {
        assert_eq!(form_decode("prompt"), vec![("prompt".to_string(), String::new())]);
    }

    #[test]
    fn query_is_appended_with_the_right_separator() {
        assert_eq!(append_query("https://x.test/a", "b=1"), "https://x.test/a?b=1");
        assert_eq!(append_query("https://x.test/a?z=0", "b=1"), "https://x.test/a?z=0&b=1");
        assert_eq!(append_query("https://x.test/a", ""), "https://x.test/a");
    }
}
