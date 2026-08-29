//! The encodings a message has to speak.
//!
//! Base64, quoted-printable, RFC 2047 encoded-words, header folding, RFC 5322
//! dates, and SMTP dot-stuffing. None of it is interesting, and all of it is
//! where mail actually breaks: a header that is one byte too long, a body line
//! that starts with a dot, an accented name that arrives as mojibake.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Where a header line is folded. RFC 5322 recommends 78 characters; the hard
/// limit is 998, and some servers are unhappy well before it.
const HEADER_WIDTH: usize = 76;

/// The longest line quoted-printable may emit, counting the soft break.
const QP_WIDTH: usize = 76;

/// 57 input bytes encode to exactly 76 base64 characters.
const BASE64_LINE_INPUT: usize = 57;

pub fn base64_encode(input: &[u8]) -> String {
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

pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
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

/// Base64 wrapped at 76 characters with CRLF, which is what a MIME body part
/// must look like — an unwrapped 2 MB single line is not a legal message.
pub fn base64_body(input: &[u8]) -> String {
    let mut out = String::new();
    for (index, chunk) in input.chunks(BASE64_LINE_INPUT).enumerate() {
        if index > 0 {
            out.push_str("\r\n");
        }
        out.push_str(&base64_encode(chunk));
    }
    out
}

/// Every line ending becomes CRLF, which is the only line ending a message on
/// the wire is allowed to have.
pub fn normalize_crlf(input: &str) -> String {
    input.replace("\r\n", "\n").replace('\r', "\n").replace('\n', "\r\n")
}

/// Encode a body part as quoted-printable (RFC 2045).
///
/// Preferred over base64 for text because the result stays readable: a mostly
/// ASCII message is still legible in a raw `.eml`, which matters the day
/// somebody has to debug one.
pub fn quoted_printable(input: &str) -> String {
    let normalized = normalize_crlf(input);
    let bytes = normalized.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 4);
    let mut column = 0usize;
    let mut index = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];

        if byte == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            out.push_str("\r\n");
            column = 0;
            index += 2;
            continue;
        }

        // Whitespace at the end of a line has to be encoded: a relay is
        // allowed to strip trailing spaces, and would change the message.
        let ends_the_line = matches!(bytes.get(index + 1), None | Some(b'\r'));
        let piece = match byte {
            b' ' | b'\t' if !ends_the_line => (byte as char).to_string(),
            33..=60 | 62..=126 => (byte as char).to_string(),
            other => format!("={other:02X}"),
        };

        // A soft break the decoder removes. Checked against the whole piece so
        // an `=XX` triple is never split in half.
        if column + piece.len() > QP_WIDTH - 1 {
            out.push_str("=\r\n");
            column = 0;
        }
        out.push_str(&piece);
        column += piece.len();
        index += 1;
    }

    out
}

/// Whether a header value can be written literally.
fn is_plain_header_text(text: &str) -> bool {
    text.is_ascii() && !text.chars().any(|c| c.is_ascii_control())
}

/// Encode a header value as RFC 2047 encoded-words when it is not plain ASCII.
///
/// Headers are ASCII by definition, so `Grüße` has to travel as
/// `=?UTF-8?B?R3LDvMOfZQ==?=`. Long values become several encoded-words
/// separated by a fold, because one word may not exceed 75 characters.
pub fn encode_word(text: &str) -> String {
    if is_plain_header_text(text) {
        return text.to_string();
    }

    // `=?UTF-8?B?` + `?=` is 12 characters; 45 input bytes encode to 60, which
    // keeps every word inside the 75-character limit with room to spare.
    const CHUNK: usize = 45;

    let mut words: Vec<String> = Vec::new();
    let mut chunk = String::new();

    for ch in text.chars() {
        // Control characters would survive base64 and reappear in a decoded
        // header, so they are dropped rather than encoded.
        if ch.is_control() {
            continue;
        }
        if chunk.len() + ch.len_utf8() > CHUNK {
            words.push(format!("=?UTF-8?B?{}?=", base64_encode(chunk.as_bytes())));
            chunk.clear();
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        words.push(format!("=?UTF-8?B?{}?=", base64_encode(chunk.as_bytes())));
    }

    words.join("\r\n ")
}

/// Strip anything that would let a value break out of its header.
///
/// A subject arriving from a form is attacker-controlled; a bare CRLF in it
/// would start a new header, which is how `Bcc:` gets added to somebody else's
/// mail. Folding whitespace is collapsed to a single space.
pub fn sanitize_header_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = false;

    for ch in text.chars() {
        let space = ch == '\r' || ch == '\n' || ch == '\t' || ch == ' ';
        if space {
            if !last_was_space && !out.is_empty() {
                out.push(' ');
            }
            last_was_space = true;
            continue;
        }
        if ch.is_control() {
            continue;
        }
        out.push(ch);
        last_was_space = false;
    }

    out.trim_end().to_string()
}

/// Write `Name: value`, folded so no line runs past [`HEADER_WIDTH`].
///
/// Folds happen at spaces the value already contains — a header may be split
/// only at folding whitespace, never mid-token. A value that is one long token
/// stays long, which is legal and the only correct answer.
pub fn header_line(name: &str, value: &str) -> String {
    let mut out = String::with_capacity(name.len() + value.len() + 4);
    out.push_str(name);
    out.push_str(": ");
    let mut column = name.len() + 2;

    // The value may already contain folds, from `encode_word`.
    for (segment_index, segment) in value.split("\r\n ").enumerate() {
        if segment_index > 0 {
            out.push_str("\r\n ");
            column = 1;
        }
        for (token_index, token) in segment.split(' ').enumerate() {
            if token_index > 0 {
                if column + 1 + token.len() > HEADER_WIDTH {
                    out.push_str("\r\n ");
                    column = 1;
                } else {
                    out.push(' ');
                    column += 1;
                }
            }
            out.push_str(token);
            column += token.len();
        }
    }

    out.push_str("\r\n");
    out
}

/// Prefix any line that starts with `.` with a second one.
///
/// A lone `.` on its own line ends the SMTP DATA phase, so a message body
/// containing one would be truncated there — and the rest of it interpreted as
/// SMTP commands.
pub fn dot_stuff(data: &str) -> String {
    let normalized = normalize_crlf(data);
    let mut out = String::with_capacity(normalized.len() + 8);

    for (index, line) in normalized.split("\r\n").enumerate() {
        if index > 0 {
            out.push_str("\r\n");
        }
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
    }

    out
}

/// Percent-encode a filename for an RFC 2231 `filename*` parameter.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Format a `Date:` header, RFC 5322 style, in UTC.
///
/// Always `+0000`: the framework has no timezone database, and a wrong offset
/// is worse than an honest one — mail clients sort by this field.
pub fn rfc5322_date(time: SystemTime) -> String {
    let seconds = time.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (hour, minute, second) = (rest / 3600, (rest % 3600) / 60, rest % 60);

    // 1970-01-01 was a Thursday, which is why WEEKDAYS starts there.
    let weekday = WEEKDAYS[days.rem_euclid(7) as usize];
    let (year, month, day) = civil_from_days(days);

    format!(
        "{weekday}, {day:02} {} {year} {hour:02}:{minute:02}:{second:02} +0000",
        MONTHS[(month - 1) as usize]
    )
}

/// Days since the epoch to a calendar date, by Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 { shifted } else { shifted - 146_096 } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 { month_prime + 3 } else { month_prime - 9 };

    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// A value unique within this process, for boundaries and message ids.
///
/// A counter rather than randomness: a boundary only has to be absent from the
/// body it delimits, and a message id only has to be unique per sender.
pub fn unique_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}{count:x}{:x}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        for (plain, encoded) in
            [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")]
        {
            assert_eq!(base64_encode(plain.as_bytes()), encoded);
            assert_eq!(base64_decode(encoded).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn a_base64_body_is_wrapped_at_seventy_six_characters() {
        let body = base64_body(&[b'x'; 200]);

        for line in body.split("\r\n") {
            assert!(line.len() <= 76, "line of {} characters: {line}", line.len());
        }
        assert_eq!(base64_decode(&body).unwrap(), [b'x'; 200]);
    }

    #[test]
    fn quoted_printable_escapes_what_it_must_and_leaves_the_rest_readable() {
        assert_eq!(quoted_printable("hello world"), "hello world");
        assert_eq!(quoted_printable("caf\u{e9}"), "caf=C3=A9");
        assert_eq!(quoted_printable("1 = 1"), "1 =3D 1");
        // A trailing space would be stripped by a relay, so it is encoded.
        assert_eq!(quoted_printable("trailing "), "trailing=20");
        assert_eq!(quoted_printable("two\nlines"), "two\r\nlines");
    }

    /// Undo quoted-printable, so a round trip can be asserted rather than a
    /// shape guessed at.
    fn undo_quoted_printable(encoded: &str) -> Vec<u8> {
        let joined = encoded.replace("=\r\n", "");
        let bytes = joined.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut index = 0;

        while index < bytes.len() {
            if bytes[index] == b'=' && index + 2 < bytes.len() {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap();
                out.push(u8::from_str_radix(hex, 16).expect("two hex digits"));
                index += 3;
            } else {
                out.push(bytes[index]);
                index += 1;
            }
        }

        out
    }

    #[test]
    fn quoted_printable_soft_wraps_without_splitting_an_escape() {
        let source = "\u{e9}".repeat(60);
        let encoded = quoted_printable(&source);

        assert!(encoded.contains("=\r\n"), "a long line should have been folded");
        for line in encoded.split("\r\n") {
            assert!(line.len() <= 76, "line of {} characters: {line}", line.len());
        }
        // The only proof that matters: it decodes back to what went in.
        assert_eq!(undo_quoted_printable(&encoded), source.as_bytes());
    }

    #[test]
    fn plain_ascii_headers_are_left_alone() {
        assert_eq!(encode_word("Your receipt"), "Your receipt");
    }

    #[test]
    fn a_non_ascii_header_becomes_an_encoded_word() {
        assert_eq!(encode_word("Gr\u{fc}\u{df}e"), "=?UTF-8?B?R3LDvMOfZQ==?=");
    }

    #[test]
    fn a_long_non_ascii_header_becomes_several_words_split_on_characters() {
        let encoded = encode_word(&"\u{e9}".repeat(60));
        let words: Vec<&str> = encoded.split("\r\n ").collect();

        assert!(words.len() > 1, "expected a fold: {encoded}");
        let mut decoded = Vec::new();
        for word in words {
            assert!(word.len() <= 75, "encoded word too long: {word}");
            let payload = word.trim_start_matches("=?UTF-8?B?").trim_end_matches("?=");
            decoded.extend(base64_decode(payload).unwrap());
        }
        assert_eq!(String::from_utf8(decoded).unwrap(), "\u{e9}".repeat(60));
    }

    #[test]
    fn header_text_cannot_smuggle_a_second_header() {
        let injected = sanitize_header_text("Receipt\r\nBcc: evil@example.com");

        assert_eq!(injected, "Receipt Bcc: evil@example.com");
        assert!(!injected.contains('\r') && !injected.contains('\n'));
    }

    #[test]
    fn a_long_header_folds_at_spaces_only() {
        let line = header_line("Subject", &"word ".repeat(30));

        for (index, folded) in line.trim_end().split("\r\n").enumerate() {
            assert!(folded.len() <= HEADER_WIDTH, "line too long: {folded}");
            if index > 0 {
                assert!(folded.starts_with(' '), "a continuation must start with whitespace");
            }
        }
        assert!(line.ends_with("\r\n"));
    }

    #[test]
    fn a_short_header_is_one_line() {
        assert_eq!(header_line("To", "ada@example.com"), "To: ada@example.com\r\n");
    }

    #[test]
    fn lines_beginning_with_a_dot_are_stuffed() {
        assert_eq!(dot_stuff(".\r\nbody"), "..\r\nbody");
        assert_eq!(dot_stuff("...ellipsis"), "....ellipsis");
        assert_eq!(dot_stuff("no dots here"), "no dots here");
        // Only the start of a line matters.
        assert_eq!(dot_stuff("a.b"), "a.b");
    }

    #[test]
    fn dates_render_in_the_shape_rfc_5322_asks_for() {
        assert_eq!(
            rfc5322_date(UNIX_EPOCH),
            "Thu, 01 Jan 1970 00:00:00 +0000"
        );
        assert_eq!(
            rfc5322_date(UNIX_EPOCH + std::time::Duration::from_secs(1_234_567_890)),
            "Fri, 13 Feb 2009 23:31:30 +0000"
        );
        assert_eq!(
            rfc5322_date(UNIX_EPOCH + std::time::Duration::from_secs(1_709_164_800)),
            "Thu, 29 Feb 2024 00:00:00 +0000"
        );
    }

    #[test]
    fn unique_tokens_do_not_repeat() {
        let first = unique_token();
        let second = unique_token();
        assert_ne!(first, second);
    }

    #[test]
    fn filenames_percent_encode_for_rfc_2231() {
        assert_eq!(percent_encode("r\u{e9}sum\u{e9}.pdf"), "r%C3%A9sum%C3%A9.pdf");
        assert_eq!(percent_encode("plain.txt"), "plain.txt");
    }
}
