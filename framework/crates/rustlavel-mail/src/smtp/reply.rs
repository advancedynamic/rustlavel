//! Parsing SMTP replies.
//!
//! A reply is a three-digit code and some text, except when it is several
//! lines, each repeating the code, with `-` after the code on every line but
//! the last. `EHLO` always answers that way, so this is the first thing an
//! SMTP client has to get right.

use rustlavel_core::{Error, Result};

/// One complete reply from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    /// The text of each line, without the code and the separator.
    pub lines: Vec<String>,
}

impl Reply {
    /// Parse a complete reply block, one or many lines.
    pub fn parse(raw: &str) -> Result<Reply> {
        let mut code: Option<u16> = None;
        let mut lines = Vec::new();
        let mut finished = false;

        for line in raw.replace("\r\n", "\n").split('\n') {
            if line.is_empty() {
                continue;
            }
            if finished {
                return Err(Error::Protocol(format!(
                    "the server sent `{line}` after its reply had already ended"
                )));
            }

            let bytes = line.as_bytes();
            if bytes.len() < 3 || !bytes[..3].iter().all(u8::is_ascii_digit) {
                return Err(Error::Protocol(format!(
                    "`{line}` is not an SMTP reply — a reply starts with a three-digit code"
                )));
            }
            let this_code: u16 = line[..3].parse().expect("three ASCII digits");

            match code {
                None => code = Some(this_code),
                Some(first) if first != this_code => {
                    return Err(Error::Protocol(format!(
                        "the server changed reply code from {first} to {this_code} mid-reply"
                    )));
                }
                Some(_) => {}
            }

            match bytes.get(3) {
                None | Some(b' ') => finished = true,
                Some(b'-') => {}
                Some(other) => {
                    return Err(Error::Protocol(format!(
                        "expected ` ` or `-` after the reply code, got `{}`",
                        *other as char
                    )));
                }
            }

            lines.push(line.get(4..).unwrap_or("").trim_end().to_string());
        }

        match code {
            Some(code) if finished => Ok(Reply { code, lines }),
            Some(_) => Err(Error::Protocol("the reply ended before its last line".into())),
            None => Err(Error::Protocol("the server sent an empty reply".into())),
        }
    }

    /// The reply text, joined — this is what goes into an error message.
    pub fn text(&self) -> String {
        self.lines.join(" ")
    }

    /// 2xx: it worked.
    pub fn is_positive(&self) -> bool {
        (200..300).contains(&self.code)
    }

    /// 3xx: the server wants more input before it will answer properly.
    pub fn is_intermediate(&self) -> bool {
        (300..400).contains(&self.code)
    }

    /// 4xx: try again later. 5xx means never.
    pub fn is_transient(&self) -> bool {
        (400..500).contains(&self.code)
    }
}

/// Where a complete reply ends in `buffer`, if one has arrived yet.
///
/// The terminator is a line whose code is followed by a space rather than a
/// dash; until then the server is still talking.
pub fn complete_reply_end(buffer: &[u8]) -> Option<usize> {
    let mut start = 0;

    while let Some(offset) = buffer[start..].iter().position(|byte| *byte == b'\n') {
        let end = start + offset + 1;
        let line = &buffer[start..end];
        let trimmed: &[u8] = line.strip_suffix(b"\r\n").or_else(|| line.strip_suffix(b"\n")).unwrap_or(line);

        // A short or malformed line is left for the parser to complain about,
        // rather than being read as "keep waiting" and hanging the connection.
        let terminal = trimmed.len() < 4 || trimmed[3] == b' ';
        if terminal {
            return Some(end);
        }
        start = end;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_line_reply_parses() {
        let reply = Reply::parse("250 OK\r\n").unwrap();

        assert_eq!(reply.code, 250);
        assert_eq!(reply.lines, vec!["OK"]);
        assert!(reply.is_positive());
    }

    #[test]
    fn a_multi_line_ehlo_reply_keeps_every_capability() {
        let raw = "250-smtp.example.com at your service\r\n\
                   250-SIZE 35882577\r\n\
                   250-STARTTLS\r\n\
                   250-AUTH LOGIN PLAIN XOAUTH2\r\n\
                   250 8BITMIME\r\n";

        let reply = Reply::parse(raw).unwrap();

        assert_eq!(reply.code, 250);
        assert_eq!(
            reply.lines,
            vec![
                "smtp.example.com at your service",
                "SIZE 35882577",
                "STARTTLS",
                "AUTH LOGIN PLAIN XOAUTH2",
                "8BITMIME",
            ]
        );
    }

    #[test]
    fn a_reply_code_with_no_text_is_still_a_reply() {
        let reply = Reply::parse("250\r\n").unwrap();
        assert_eq!((reply.code, reply.lines.as_slice()), (250, ["".to_string()].as_slice()));
    }

    #[test]
    fn a_reply_that_changes_its_code_is_rejected() {
        let error = Reply::parse("250-first\r\n451 second\r\n").unwrap_err().to_string();
        assert!(error.contains("changed reply code"), "{error}");
    }

    #[test]
    fn something_that_is_not_a_reply_is_rejected() {
        assert!(Reply::parse("hello?\r\n").is_err());
        assert!(Reply::parse("25 short\r\n").is_err());
        assert!(Reply::parse("").is_err());
        // A block that only ever continues never ends.
        assert!(Reply::parse("250-one\r\n250-two\r\n").is_err());
    }

    #[test]
    fn reply_classes_read_the_first_digit() {
        assert!(Reply::parse("354 Go ahead").unwrap().is_intermediate());
        assert!(Reply::parse("451 Try later").unwrap().is_transient());
        assert!(!Reply::parse("550 Nope").unwrap().is_transient());
    }

    #[test]
    fn a_reply_is_complete_only_after_its_terminating_line() {
        assert_eq!(complete_reply_end(b"250 OK\r\n"), Some(8));
        assert_eq!(complete_reply_end(b"250 OK\r"), None);
        assert_eq!(complete_reply_end(b"250-one\r\n"), None);
        assert_eq!(complete_reply_end(b"250-one\r\n250 two\r\n"), Some(18));
        // Bytes belonging to the next reply are left in the buffer.
        assert_eq!(complete_reply_end(b"250 one\r\n250 two\r\n"), Some(9));
    }
}
