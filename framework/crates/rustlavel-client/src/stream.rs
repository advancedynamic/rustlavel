//! Streaming response bodies, and the server-sent events AI providers use.

use crate::{Connection, find_head_end, parse_head};
use rustlavel_core::{Error, Json, Result};
use rustlavel_http::{Headers, Status};
use std::time::Duration;

/// A response whose body is read as it arrives.
pub struct Body {
    pub status: Status,
    pub headers: Headers,
    source: Source,
    buffer: Vec<u8>,
    /// Set once the transfer is complete, so `chunk` stops asking for more.
    finished: bool,
    /// Bytes still expected, when the body is chunked.
    chunk_remaining: Option<usize>,
    chunked: bool,
    timeout: Duration,
}

enum Source {
    Live(Box<Connection>),
    /// A faked body, already complete.
    Memory,
}

/// Send a request and hand back the body before it has finished arriving.
pub(crate) async fn open(
    mut connection: Connection,
    request: Vec<u8>,
    timeout: Duration,
) -> Result<Body> {
    connection.write_all(&request).await.map_err(Error::Io)?;
    connection.flush().await.map_err(Error::Io)?;

    let mut buffer = Vec::with_capacity(8 * 1024);
    let head_end = loop {
        if let Some(at) = find_head_end(&buffer) {
            break at;
        }
        let mut chunk = [0u8; 4096];
        let read = tokio::time::timeout(timeout, connection.read(&mut chunk))
            .await
            .map_err(|_| Error::msg("timed out waiting for response headers"))?
            .map_err(Error::Io)?;
        if read == 0 {
            return Err(Error::Protocol("the server closed before sending headers".into()));
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let (status, headers) = parse_head(&buffer[..head_end])?;
    let rest = buffer.split_off(head_end);
    let chunked = headers.get("transfer-encoding").is_some_and(|te| te.contains("chunked"));

    Ok(Body {
        status,
        headers,
        source: Source::Live(Box::new(connection)),
        buffer: rest,
        finished: false,
        chunk_remaining: None,
        chunked,
        timeout,
    })
}

impl Body {
    /// A body that is already in memory, for fakes and tests.
    pub fn from_bytes(status: Status, headers: Headers, body: Vec<u8>) -> Body {
        Body {
            status,
            headers,
            source: Source::Memory,
            buffer: body,
            finished: true,
            chunk_remaining: None,
            chunked: false,
            timeout: Duration::from_secs(30),
        }
    }

    /// The next piece of the body, or `None` when it is complete.
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if self.chunked {
                if let Some(chunk) = self.take_chunked()? {
                    return Ok(Some(chunk));
                }
            } else if !self.buffer.is_empty() {
                return Ok(Some(std::mem::take(&mut self.buffer)));
            }

            if self.finished {
                return Ok(None);
            }
            if !self.fill().await? {
                self.finished = true;
                if !self.chunked && !self.buffer.is_empty() {
                    return Ok(Some(std::mem::take(&mut self.buffer)));
                }
                return Ok(None);
            }
        }
    }

    /// Read the rest of the body into memory.
    pub async fn bytes(mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(chunk) = self.chunk().await? {
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    pub async fn text(self) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.bytes().await?).into_owned())
    }

    /// Read the body as a server-sent event stream.
    pub fn events(self) -> SseReader {
        SseReader { body: self, pending: String::new() }
    }

    /// Pull one chunk out of a chunked transfer, if a whole one is buffered.
    fn take_chunked(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            match self.chunk_remaining {
                None => {
                    let Some(line_end) = find_crlf(&self.buffer) else { return Ok(None) };
                    let header: Vec<u8> = self.buffer.drain(..line_end + 2).collect();
                    let text = String::from_utf8_lossy(&header[..line_end]);
                    let size =
                        usize::from_str_radix(text.split(';').next().unwrap_or("").trim(), 16)
                            .map_err(|_| Error::Protocol("invalid chunk size".into()))?;
                    if size == 0 {
                        self.finished = true;
                        return Ok(None);
                    }
                    self.chunk_remaining = Some(size);
                }
                Some(size) => {
                    if self.buffer.len() < size + 2 {
                        return Ok(None);
                    }
                    let chunk: Vec<u8> = self.buffer.drain(..size).collect();
                    self.buffer.drain(..2);
                    self.chunk_remaining = None;
                    return Ok(Some(chunk));
                }
            }
        }
    }

    async fn fill(&mut self) -> Result<bool> {
        let Source::Live(connection) = &mut self.source else {
            return Ok(false);
        };

        let mut chunk = [0u8; 8192];
        let read = tokio::time::timeout(self.timeout, connection.read(&mut chunk))
            .await
            .map_err(|_| Error::msg("timed out reading the response body"))?
            .map_err(Error::Io)?;

        self.buffer.extend_from_slice(&chunk[..read]);
        Ok(read > 0)
    }
}

fn find_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|w| w == b"\r\n")
}

/// One server-sent event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerSentEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

impl ServerSentEvent {
    /// The data parsed as JSON, which is how every AI provider sends it.
    pub fn json(&self) -> Result<Json> {
        Json::parse(&self.data)
    }

    /// Whether this is the conventional end-of-stream marker.
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }
}

/// Reads a body as a sequence of server-sent events.
pub struct SseReader {
    body: Body,
    /// Text received but not yet ending in a blank line.
    pending: String,
}

impl SseReader {
    /// The next event, or `None` at the end of the stream.
    pub async fn next(&mut self) -> Result<Option<ServerSentEvent>> {
        loop {
            if let Some(event) = self.take_event() {
                return Ok(Some(event));
            }
            match self.body.chunk().await? {
                Some(bytes) => self.pending.push_str(&String::from_utf8_lossy(&bytes)),
                None => return Ok(self.take_event().or_else(|| self.take_remainder())),
            }
        }
    }

    /// Collect every event, for a caller that does not need them as they arrive.
    pub async fn collect(mut self) -> Result<Vec<ServerSentEvent>> {
        let mut events = Vec::new();
        while let Some(event) = self.next().await? {
            events.push(event);
        }
        Ok(events)
    }

    fn take_event(&mut self) -> Option<ServerSentEvent> {
        // An event ends at a blank line; both line endings appear in the wild.
        let end = self
            .pending
            .find("\n\n")
            .map(|at| (at, 2))
            .or_else(|| self.pending.find("\r\n\r\n").map(|at| (at, 4)))?;

        let block: String = self.pending.drain(..end.0 + end.1).collect();
        parse_event(&block)
    }

    fn take_remainder(&mut self) -> Option<ServerSentEvent> {
        if self.pending.trim().is_empty() {
            return None;
        }
        let block = std::mem::take(&mut self.pending);
        parse_event(&block)
    }
}

fn parse_event(block: &str) -> Option<ServerSentEvent> {
    let mut event = ServerSentEvent::default();
    let mut data_lines: Vec<&str> = Vec::new();

    for line in block.lines() {
        let line = line.trim_end_matches('\r');
        // A line starting with `:` is a comment, used as a keep-alive.
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => event.event = Some(value.to_string()),
            "data" => data_lines.push(value),
            "id" => event.id = Some(value.to_string()),
            _ => {}
        }
    }

    if data_lines.is_empty() && event.event.is_none() {
        return None;
    }
    event.data = data_lines.join("\n");
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(text: &str) -> Body {
        Body::from_bytes(Status::OK, Headers::new(), text.as_bytes().to_vec())
    }

    #[tokio::test]
    async fn reads_a_stream_of_events() {
        let mut events = body("data: one\n\ndata: two\n\ndata: [DONE]\n\n").events();

        assert_eq!(events.next().await.unwrap().unwrap().data, "one");
        assert_eq!(events.next().await.unwrap().unwrap().data, "two");
        assert!(events.next().await.unwrap().unwrap().is_done());
        assert!(events.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn keeps_named_events_and_ids() {
        let events = body("event: message_start\nid: 42\ndata: {\"a\":1}\n\n")
            .events()
            .collect()
            .await
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].id.as_deref(), Some("42"));
        assert_eq!(events[0].json().unwrap().get("a").unwrap().as_i64(), Some(1));
    }

    #[tokio::test]
    async fn joins_multi_line_data_and_skips_comments() {
        let events = body(": keep-alive\n\ndata: first\ndata: second\n\n").events().collect().await.unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "first\nsecond");
    }

    #[tokio::test]
    async fn handles_crlf_line_endings() {
        let events = body("data: one\r\n\r\ndata: two\r\n\r\n").events().collect().await.unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[1].data, "two");
    }

    #[tokio::test]
    async fn an_unterminated_final_event_is_still_delivered() {
        let events = body("data: one\n\ndata: trailing").events().collect().await.unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[1].data, "trailing");
    }

    #[tokio::test]
    async fn reads_a_whole_body_as_text() {
        assert_eq!(body("hello").text().await.unwrap(), "hello");
    }
}
