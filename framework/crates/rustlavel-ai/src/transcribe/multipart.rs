//! `multipart/form-data`, the one request body the HTTP client does not
//! build for us: a file upload with a few fields beside it.

use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct Multipart {
    boundary: String,
    body: Vec<u8>,
}

impl Multipart {
    pub(crate) fn new() -> Multipart {
        // Unique per request within a process, and not a byte sequence an
        // audio file is likely to contain. RFC 2046 gives 70 characters.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let boundary = format!(
            "----rustlavel-{nanos:x}-{:x}-{:x}",
            COUNTER.fetch_add(1, Ordering::Relaxed),
            std::process::id()
        );
        Multipart { boundary, body: Vec::new() }
    }

    pub(crate) fn text(mut self, name: &str, value: &str) -> Multipart {
        self.body.extend_from_slice(
            format!("--{}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n", self.boundary)
                .as_bytes(),
        );
        self
    }

    pub(crate) fn file(mut self, name: &str, file_name: &str, mime: &str, bytes: &[u8]) -> Multipart {
        // A quote or a newline in a file name would end the header early.
        let file_name: String = file_name.chars().filter(|c| !matches!(c, '"' | '\r' | '\n' | '\\')).collect();
        self.body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{file_name}\"\r\n\
                 Content-Type: {mime}\r\n\r\n",
                self.boundary
            )
            .as_bytes(),
        );
        self.body.extend_from_slice(bytes);
        self.body.extend_from_slice(b"\r\n");
        self
    }

    pub(crate) fn content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.boundary)
    }

    pub(crate) fn finish(mut self) -> (String, Vec<u8>) {
        let content_type = self.content_type();
        self.body.extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        (content_type, self.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_is_framed_and_the_bytes_are_untouched() {
        let audio = [0u8, 0xff, b'\r', b'\n', b'-', b'-', 7];
        let (content_type, body) = Multipart::new()
            .text("model", "whisper-1")
            .file("file", "take \"1\".mp3", "audio/mpeg", &audio)
            .finish();
        let boundary = content_type.strip_prefix("multipart/form-data; boundary=").unwrap();
        assert!(boundary.len() <= 70);

        let text = String::from_utf8_lossy(&body);
        assert!(text.contains(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n")), "{text}");
        assert!(text.contains("filename=\"take 1.mp3\"\r\nContent-Type: audio/mpeg\r\n\r\n"), "{text}");
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
        assert!(body.windows(audio.len()).any(|w| w == audio), "the file bytes were altered");
    }
}
