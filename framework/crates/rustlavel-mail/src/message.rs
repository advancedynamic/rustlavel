//! Building a message, and turning it into MIME.
//!
//! The builder collects errors instead of returning them, so a chain of
//! `.to(...).cc(...).subject(...)` stays a chain; everything that went wrong is
//! reported once, by name, when the message is validated or sent.

use crate::address::{Address, IntoAddress, address_list};
use crate::encode::{
    base64_body, dot_stuff, encode_word, header_line, percent_encode, quoted_printable,
    rfc5322_date, sanitize_header_text, unique_token,
};
use rustlavel_core::{Error, Result};
use std::path::Path;
use std::time::SystemTime;

/// A file travelling with the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

impl Attachment {
    /// An attachment whose type is guessed from the file extension.
    pub fn new(filename: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Attachment {
        let filename = sanitize_header_text(&filename.into());
        let content_type = guess_content_type(&filename).to_string();
        Attachment { filename, content_type, bytes: bytes.into() }
    }

    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Attachment {
        self.content_type = sanitize_header_text(&content_type.into());
        self
    }

    /// Read a file from disk, keeping its name.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Attachment> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|error| {
            Error::msg(format!("cannot attach `{}`: {error}", path.display()))
        })?;
        let filename =
            path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Attachment::new(filename, bytes))
    }

    /// The `Content-Type` and `Content-Disposition` parameters for the name.
    ///
    /// A non-ASCII filename goes out as RFC 2231 `filename*`, which is what
    /// modern clients read; an ASCII one stays a plain quoted string, which
    /// everything reads.
    fn name_parameter(&self, keyword: &str) -> String {
        if self.filename.is_ascii() {
            format!("; {keyword}=\"{}\"", self.filename.replace('"', ""))
        } else {
            format!("; {keyword}*=UTF-8''{}", percent_encode(&self.filename))
        }
    }
}

/// One email, before it becomes bytes.
#[derive(Debug, Clone, Default)]
pub struct Message {
    from: Option<Address>,
    to: Vec<Address>,
    cc: Vec<Address>,
    bcc: Vec<Address>,
    reply_to: Vec<Address>,
    subject: String,
    text: Option<String>,
    html: Option<String>,
    attachments: Vec<Attachment>,
    headers: Vec<(String, String)>,
    date: Option<String>,
    message_id: Option<String>,
    boundary: Option<String>,
    /// Addresses that would not parse, kept until validation so the builder
    /// stays chainable.
    errors: Vec<String>,
}

impl Message {
    pub fn new() -> Message {
        Message::default()
    }

    pub fn from(mut self, address: impl IntoAddress) -> Message {
        match address.into_address() {
            Ok(address) => self.from = Some(address),
            Err(error) => self.errors.push(format!("From: {error}")),
        }
        self
    }

    pub fn to(self, address: impl IntoAddress) -> Message {
        self.push("To", address, |message| &mut message.to)
    }

    pub fn cc(self, address: impl IntoAddress) -> Message {
        self.push("Cc", address, |message| &mut message.cc)
    }

    /// A blind copy. The address reaches the server in `RCPT TO` and appears in
    /// no header — that is the whole point, and getting it wrong discloses
    /// every recipient to every other one.
    pub fn bcc(self, address: impl IntoAddress) -> Message {
        self.push("Bcc", address, |message| &mut message.bcc)
    }

    pub fn reply_to(self, address: impl IntoAddress) -> Message {
        self.push("Reply-To", address, |message| &mut message.reply_to)
    }

    fn push(
        mut self,
        field: &str,
        address: impl IntoAddress,
        select: fn(&mut Message) -> &mut Vec<Address>,
    ) -> Message {
        match address.into_address() {
            Ok(address) => select(&mut self).push(address),
            Err(error) => self.errors.push(format!("{field}: {error}")),
        }
        self
    }

    pub fn subject(mut self, subject: impl Into<String>) -> Message {
        self.subject = sanitize_header_text(&subject.into());
        self
    }

    pub fn text(mut self, body: impl Into<String>) -> Message {
        self.text = Some(body.into());
        self
    }

    pub fn html(mut self, body: impl Into<String>) -> Message {
        self.html = Some(body.into());
        self
    }

    pub fn attach(mut self, attachment: Attachment) -> Message {
        self.attachments.push(attachment);
        self
    }

    /// Add a header of your own. `X-` headers, mostly.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Message {
        self.headers
            .push((sanitize_header_text(&name.into()), sanitize_header_text(&value.into())));
        self
    }

    /// Pin the `Date` header. Tests want a message that does not change between
    /// runs; nothing else should call this.
    pub fn with_date(mut self, date: impl Into<String>) -> Message {
        self.date = Some(sanitize_header_text(&date.into()));
        self
    }

    /// Pin the `Message-ID`, for the same reason as [`Message::with_date`].
    pub fn with_message_id(mut self, id: impl Into<String>) -> Message {
        self.message_id = Some(sanitize_header_text(&id.into()));
        self
    }

    /// Pin the MIME boundary, for the same reason again.
    pub fn with_boundary(mut self, boundary: impl Into<String>) -> Message {
        self.boundary = Some(sanitize_header_text(&boundary.into()));
        self
    }

    pub fn sender(&self) -> Option<&Address> {
        self.from.as_ref()
    }

    pub fn subject_text(&self) -> &str {
        &self.subject
    }

    /// The plain-text body, before any transfer encoding.
    ///
    /// What the `log` transport prints. Quoted-printable is correct on the
    /// wire and useless on a terminal: it breaks any line over 76 columns with
    /// a soft `=`, which lands inside a long link and makes the URL somebody
    /// copies a different URL from the one that was sent.
    pub fn text_body(&self) -> Option<&str> {
        self.text.as_deref()
    }

    pub fn html_body(&self) -> Option<&str> {
        self.html.as_deref()
    }

    pub fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }

    pub fn to_addresses(&self) -> &[Address] {
        &self.to
    }

    /// Everyone the message is addressed to, headers included.
    pub fn recipients(&self) -> Vec<&Address> {
        self.to.iter().chain(&self.cc).chain(&self.bcc).collect()
    }

    /// The envelope: who the server is asked to deliver to, Bcc included.
    pub fn envelope_recipients(&self) -> Vec<String> {
        self.recipients().iter().map(|address| address.email().to_string()).collect()
    }

    /// Fill in a `From` when the message did not set one.
    pub(crate) fn default_from(mut self, address: Address) -> Message {
        if self.from.is_none() {
            self.from = Some(address);
        }
        self
    }

    /// Everything that must be true before this can go anywhere.
    pub fn validate(&self) -> Result<()> {
        if !self.errors.is_empty() {
            return Err(Error::msg(format!(
                "this message has an invalid address — {}",
                self.errors.join("; ")
            )));
        }
        if self.from.is_none() {
            return Err(Error::msg(
                "this message has no From address. Set one with `.from(...)`, or set \
                 mail.from.address in your configuration.",
            ));
        }
        if self.recipients().is_empty() {
            return Err(Error::msg(
                "this message has no recipients. Add one with `.to(...)`, `.cc(...)` or `.bcc(...)`.",
            ));
        }
        Ok(())
    }

    /// Render the whole message: headers, then the MIME body.
    pub fn to_mime(&self) -> Result<String> {
        self.validate()?;

        let from = self.from.as_ref().expect("validated above");
        let boundary = self.boundary.clone().unwrap_or_else(unique_token);

        let mut out = String::with_capacity(1024 + self.attachments.iter().map(|a| a.bytes.len() * 4 / 3).sum::<usize>());

        out.push_str(&header_line(
            "Date",
            self.date.as_deref().unwrap_or(&rfc5322_date(SystemTime::now())),
        ));
        out.push_str(&header_line("From", &from.to_header()));
        if !self.to.is_empty() {
            out.push_str(&header_line("To", &address_list(&self.to)));
        }
        if !self.cc.is_empty() {
            out.push_str(&header_line("Cc", &address_list(&self.cc)));
        }
        if !self.reply_to.is_empty() {
            out.push_str(&header_line("Reply-To", &address_list(&self.reply_to)));
        }
        // Bcc is deliberately absent: it travels in the envelope only.
        out.push_str(&header_line("Subject", &encode_word(&self.subject)));
        out.push_str(&header_line(
            "Message-ID",
            &self
                .message_id
                .clone()
                .unwrap_or_else(|| format!("<{}@{}>", unique_token(), from.domain())),
        ));
        out.push_str(&header_line("MIME-Version", "1.0"));

        for (name, value) in &self.headers {
            out.push_str(&header_line(name, &encode_word(value)));
        }

        out.push_str(&self.content(&boundary));
        Ok(out)
    }

    /// The message as the DATA phase wants it: dot-stuffed, CRLF throughout.
    pub fn to_smtp_data(&self) -> Result<String> {
        Ok(dot_stuff(&self.to_mime()?))
    }

    /// Content headers and the body below them.
    fn content(&self, boundary: &str) -> String {
        if self.attachments.is_empty() {
            return self.body_part(boundary);
        }

        // Anything with a file becomes multipart/mixed: the body first, then
        // the files, so a client shows the message and offers the attachments.
        let mixed = format!("{boundary}_mixed");
        let mut out = String::new();
        out.push_str(&header_line("Content-Type", &format!("multipart/mixed; boundary=\"{mixed}\"")));
        out.push_str("\r\n");
        out.push_str("This is a message in MIME format.\r\n");

        out.push_str(&format!("\r\n--{mixed}\r\n"));
        out.push_str(&self.body_part(boundary));

        for attachment in &self.attachments {
            out.push_str(&format!("\r\n--{mixed}\r\n"));
            out.push_str(&header_line(
                "Content-Type",
                &format!("{}{}", attachment.content_type, attachment.name_parameter("name")),
            ));
            out.push_str(&header_line("Content-Transfer-Encoding", "base64"));
            out.push_str(&header_line(
                "Content-Disposition",
                &format!("attachment{}", attachment.name_parameter("filename")),
            ));
            out.push_str("\r\n");
            out.push_str(&base64_body(&attachment.bytes));
            out.push_str("\r\n");
        }

        out.push_str(&format!("\r\n--{mixed}--\r\n"));
        out
    }

    /// The readable part of the message, with its own content headers.
    fn body_part(&self, boundary: &str) -> String {
        match (&self.text, &self.html) {
            (Some(text), None) => text_part("text/plain", text),
            (None, Some(html)) => text_part("text/html", html),
            (None, None) => text_part("text/plain", ""),
            (Some(text), Some(html)) => {
                // multipart/alternative, plainest first: a client picks the last
                // part it can render, which is the rule RFC 2046 states.
                let alternative = format!("{boundary}_alt");
                let mut out = String::new();
                out.push_str(&header_line(
                    "Content-Type",
                    &format!("multipart/alternative; boundary=\"{alternative}\""),
                ));
                out.push_str("\r\n");
                out.push_str(&format!("--{alternative}\r\n"));
                out.push_str(&text_part("text/plain", text));
                out.push_str(&format!("\r\n--{alternative}\r\n"));
                out.push_str(&text_part("text/html", html));
                out.push_str(&format!("\r\n--{alternative}--\r\n"));
                out
            }
        }
    }
}

/// One `text/*` part: headers, a blank line, and a quoted-printable body.
fn text_part(content_type: &str, body: &str) -> String {
    let mut out = String::new();
    out.push_str(&header_line("Content-Type", &format!("{content_type}; charset=\"utf-8\"")));
    out.push_str(&header_line("Content-Transfer-Encoding", "quoted-printable"));
    out.push_str("\r\n");
    // `quoted_printable` normalises the line endings on the way through, so a
    // body written with `\n` still leaves here as CRLF.
    out.push_str(&quoted_printable(body));
    out
}

/// A small table, not a MIME database: enough for what people actually attach.
fn guess_content_type(filename: &str) -> &'static str {
    let extension =
        filename.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default();

    match extension.as_str() {
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ics" => "text/calendar",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message with everything variable pinned, so an expectation can be
    /// written by hand and stay true tomorrow.
    fn fixed(message: Message) -> Message {
        message
            .with_date("Fri, 13 Feb 2009 23:31:30 +0000")
            .with_message_id("<abc123@example.com>")
            .with_boundary("RL")
    }

    #[test]
    fn a_text_only_message_is_a_single_plain_part() {
        let mime = fixed(
            Message::new()
                .from(("Ada Lovelace", "ada@example.com"))
                .to("grace@example.com")
                .subject("Your receipt")
                .text("Thank you.\nThe Team"),
        )
        .to_mime()
        .unwrap();

        assert_eq!(
            mime,
            "Date: Fri, 13 Feb 2009 23:31:30 +0000\r\n\
             From: Ada Lovelace <ada@example.com>\r\n\
             To: grace@example.com\r\n\
             Subject: Your receipt\r\n\
             Message-ID: <abc123@example.com>\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: text/plain; charset=\"utf-8\"\r\n\
             Content-Transfer-Encoding: quoted-printable\r\n\
             \r\n\
             Thank you.\r\n\
             The Team"
        );
    }

    #[test]
    fn an_html_only_message_is_a_single_html_part() {
        let mime = fixed(
            Message::new()
                .from("ada@example.com")
                .to("grace@example.com")
                .subject("Hello")
                .html("<p>Hello &amp; welcome</p>"),
        )
        .to_mime()
        .unwrap();

        assert!(mime.contains("Content-Type: text/html; charset=\"utf-8\"\r\n"), "{mime}");
        assert!(!mime.contains("multipart"), "{mime}");
        assert!(mime.ends_with("\r\n\r\n<p>Hello &amp; welcome</p>"), "{mime}");
    }

    #[test]
    fn a_message_with_neither_body_is_still_a_valid_empty_plain_part() {
        let mime = fixed(Message::new().from("ada@example.com").to("grace@example.com"))
            .to_mime()
            .unwrap();

        assert!(mime.ends_with("Content-Transfer-Encoding: quoted-printable\r\n\r\n"), "{mime}");
    }

    #[test]
    fn text_and_html_become_multipart_alternative_with_plain_text_first() {
        let mime = fixed(
            Message::new()
                .from("ada@example.com")
                .to("grace@example.com")
                .subject("Hello")
                .text("Hello there")
                .html("<p>Hello there</p>"),
        )
        .to_mime()
        .unwrap();

        assert!(
            mime.contains("Content-Type: multipart/alternative; boundary=\"RL_alt\"\r\n"),
            "{mime}"
        );
        assert!(mime.ends_with("--RL_alt--\r\n"), "{mime}");

        let plain_at = mime.find("text/plain").unwrap();
        let html_at = mime.find("text/html").unwrap();
        assert!(plain_at < html_at, "the plain part must come first");

        assert!(mime.contains("--RL_alt\r\nContent-Type: text/plain; charset=\"utf-8\"\r\n"));
        assert!(mime.contains("quoted-printable\r\n\r\nHello there\r\n--RL_alt\r\n"));
        assert!(mime.contains("quoted-printable\r\n\r\n<p>Hello there</p>\r\n--RL_alt--"));
    }

    #[test]
    fn an_attachment_wraps_the_body_in_multipart_mixed() {
        let mime = fixed(
            Message::new()
                .from("ada@example.com")
                .to("grace@example.com")
                .subject("Invoice")
                .text("Attached.")
                .attach(Attachment::new("invoice.pdf", b"%PDF-1.4".to_vec())),
        )
        .to_mime()
        .unwrap();

        assert!(mime.contains("Content-Type: multipart/mixed; boundary=\"RL_mixed\"\r\n"), "{mime}");
        assert!(mime.contains("\r\n--RL_mixed\r\nContent-Type: text/plain; charset=\"utf-8\""));
        assert!(mime.contains("Content-Type: application/pdf; name=\"invoice.pdf\"\r\n"), "{mime}");
        assert!(mime.contains("Content-Transfer-Encoding: base64\r\n"));
        assert!(
            mime.contains("Content-Disposition: attachment; filename=\"invoice.pdf\"\r\n"),
            "{mime}"
        );
        // `%PDF-1.4` in base64.
        assert!(mime.contains("\r\n\r\nJVBERi0xLjQ=\r\n"), "{mime}");
        assert!(mime.ends_with("\r\n--RL_mixed--\r\n"));
    }

    #[test]
    fn a_non_ascii_subject_and_name_travel_as_encoded_words() {
        let mime = fixed(
            Message::new()
                .from(("Bj\u{f6}rn Str\u{f6}m", "bjorn@example.com"))
                .to("grace@example.com")
                .subject("Gr\u{fc}\u{df}e aus M\u{fc}nchen")
                .text("hi"),
        )
        .to_mime()
        .unwrap();

        assert!(mime.contains("From: =?UTF-8?B?QmrDtnJuIFN0csO2bQ==?= <bjorn@example.com>\r\n"), "{mime}");
        assert!(
            mime.contains("Subject: =?UTF-8?B?R3LDvMOfZSBhdXMgTcO8bmNoZW4=?=\r\n"),
            "{mime}"
        );
        // Nothing outside ASCII may reach a header.
        for line in mime.split("\r\n").take_while(|line| !line.is_empty()) {
            assert!(line.is_ascii(), "non-ASCII in a header: {line}");
        }
    }

    #[test]
    fn a_bcc_recipient_reaches_the_envelope_and_never_a_header() {
        let message = fixed(
            Message::new()
                .from("ada@example.com")
                .to("grace@example.com")
                .cc("alan@example.com")
                .bcc("secret@example.com")
                .subject("Hi")
                .text("hi"),
        );

        let mime = message.to_mime().unwrap();
        assert!(mime.contains("Cc: alan@example.com\r\n"));
        assert!(!mime.contains("secret@example.com"), "a Bcc leaked into the headers");

        assert_eq!(
            message.envelope_recipients(),
            vec!["grace@example.com", "alan@example.com", "secret@example.com"]
        );
    }

    #[test]
    fn a_subject_cannot_inject_a_header() {
        let mime = fixed(
            Message::new()
                .from("ada@example.com")
                .to("grace@example.com")
                .subject("Receipt\r\nBcc: evil@example.com")
                .text("hi"),
        )
        .to_mime()
        .unwrap();

        assert!(mime.contains("Subject: Receipt Bcc: evil@example.com\r\n"), "{mime}");
        assert!(!mime.contains("\r\nBcc:"), "the injected header survived");
    }

    #[test]
    fn the_smtp_form_is_dot_stuffed() {
        let data = fixed(
            Message::new()
                .from("ada@example.com")
                .to("grace@example.com")
                .subject("Ellipsis")
                .text("ok\n.\nnot the end"),
        )
        .to_smtp_data()
        .unwrap();

        assert!(data.contains("ok\r\n..\r\nnot the end"), "{data}");
    }

    #[test]
    fn a_message_without_a_sender_or_a_recipient_says_which_is_missing() {
        let no_from = Message::new().to("grace@example.com").validate().unwrap_err().to_string();
        assert!(no_from.contains("mail.from.address"), "{no_from}");

        let no_to = Message::new().from("ada@example.com").validate().unwrap_err().to_string();
        assert!(no_to.contains("no recipients"), "{no_to}");
    }

    #[test]
    fn a_bad_address_is_reported_at_validation_naming_the_field() {
        let error = Message::new()
            .from("ada@example.com")
            .to("not-an-address")
            .validate()
            .unwrap_err()
            .to_string();

        assert!(error.contains("To:"), "{error}");
        assert!(error.contains("not-an-address"), "{error}");
    }

    #[test]
    fn a_non_ascii_filename_uses_the_rfc_2231_form() {
        let mime = fixed(
            Message::new()
                .from("ada@example.com")
                .to("grace@example.com")
                .text("see attached")
                .attach(Attachment::new("r\u{e9}sum\u{e9}.pdf", b"x".to_vec())),
        )
        .to_mime()
        .unwrap();

        assert!(mime.contains("filename*=UTF-8''r%C3%A9sum%C3%A9.pdf"), "{mime}");
    }

    #[test]
    fn content_types_are_guessed_from_the_extension() {
        assert_eq!(Attachment::new("a.pdf", vec![]).content_type, "application/pdf");
        assert_eq!(Attachment::new("a.PNG", vec![]).content_type, "image/png");
        assert_eq!(Attachment::new("mystery", vec![]).content_type, "application/octet-stream");
        assert_eq!(
            Attachment::new("a.bin", vec![]).with_content_type("application/x-thing").content_type,
            "application/x-thing"
        );
    }

    #[test]
    fn a_default_sender_only_applies_when_the_message_has_none() {
        let configured = Address::named("Rustlavel", "no-reply@example.com").unwrap();

        let filled = Message::new().to("g@example.com").default_from(configured.clone());
        assert_eq!(filled.sender(), Some(&configured));

        let explicit =
            Message::new().from("ada@example.com").to("g@example.com").default_from(configured);
        assert_eq!(explicit.sender().unwrap().email(), "ada@example.com");
    }
}
