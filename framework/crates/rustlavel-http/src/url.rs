//! Percent-encoding and query-string handling.

/// Decode `%XX` escapes and `+` (which means a space in query strings).
pub fn decode(input: &str) -> String {
    if !input.contains('%') && !input.contains('+') {
        return input.to_string();
    }

    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match hex_pair(bytes[i + 1], bytes[i + 2]) {
                Some(byte) => {
                    out.push(byte);
                    i += 3;
                }
                // A stray `%` is kept verbatim rather than dropped.
                None => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(high: u8, low: u8) -> Option<u8> {
    Some(hex_digit(high)? << 4 | hex_digit(low)?)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Percent-encode everything outside the unreserved set.
pub fn encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Parse `a=1&b=2` into pairs, decoding both sides.
///
/// Pairs are kept in order and duplicates preserved, so `tags[]=a&tags[]=b`
/// can be read as a list.
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((key, value)) => (decode(key), decode(value)),
            None => (decode(part), String::new()),
        })
        .collect()
}

/// Split a request target into its path and raw query string.
pub fn split_target(target: &str) -> (&str, &str) {
    match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    }
}

/// Collapse `.` and `..` segments and reject anything that escapes the root.
///
/// Used before serving files from disk so a request cannot walk out of the
/// public directory.
pub fn normalize_path(path: &str) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                segments.pop()?;
            }
            // A decoded segment must never reintroduce a separator.
            s if s.contains('\\') || s.contains('\0') => return None,
            s => segments.push(s),
        }
    }
    Some(format!("/{}", segments.join("/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_escapes_and_plus() {
        assert_eq!(decode("hello+world"), "hello world");
        assert_eq!(decode("caf%C3%A9"), "café");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("plain"), "plain");
    }

    #[test]
    fn encoding_round_trips() {
        let original = "a b/c?d=é";
        assert_eq!(decode(&encode(original)), original);
    }

    #[test]
    fn parses_query_pairs_in_order() {
        let pairs = parse_query("name=Rust+lavel&tags=a&tags=b&empty");

        assert_eq!(pairs[0], ("name".to_string(), "Rust lavel".to_string()));
        assert_eq!(pairs[2], ("tags".to_string(), "b".to_string()));
        assert_eq!(pairs[3], ("empty".to_string(), String::new()));
    }

    #[test]
    fn splits_a_request_target() {
        assert_eq!(split_target("/users?page=2"), ("/users", "page=2"));
        assert_eq!(split_target("/users"), ("/users", ""));
    }

    #[test]
    fn normalization_blocks_directory_traversal() {
        assert_eq!(normalize_path("/css//app.css").as_deref(), Some("/css/app.css"));
        assert_eq!(normalize_path("/a/./b").as_deref(), Some("/a/b"));
        assert_eq!(normalize_path("/a/../b").as_deref(), Some("/b"));
        assert_eq!(normalize_path("/../etc/passwd"), None);
    }
}
