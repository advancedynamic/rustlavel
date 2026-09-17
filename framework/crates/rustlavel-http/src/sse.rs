//! Server-Sent Events: a response that stays open and delivers events as they
//! happen.
//!
//! Lighter than a WebSocket for the case it fits — the server talks, the
//! browser listens — and the browser's `EventSource` reconnects on its own,
//! sending `Last-Event-ID` so a handler can resume where the client left off.
//! Progress of a background job is the textbook case: one direction, a few
//! events a second at most, and a client that must not miss the last one.
//!
//! ```ignore
//! r.get("/jobs/{id}/events", |req: Request| async move {
//!     let (tx, rx) = sse::channel(16);
//!     tokio::spawn(async move {
//!         for step in 0..=100 {
//!             if tx.send(Event::json("progress", Json::object([("percent", step.into())]))).await.is_err() {
//!                 break; // the client went away
//!             }
//!         }
//!     });
//!     Response::events(rx)
//! });
//! ```
//!
//! # What the wire looks like
//!
//! One event is a few `field: value` lines and a blank line. `data:` may
//! repeat, one line each — a newline inside the data would otherwise end the
//! event early, so it is split for you. A comment line (`: …`) is sent every
//! fifteen seconds while nothing else is: a proxy that sees no bytes for a
//! minute closes the connection, and the client would then reconnect for no
//! reason.

use crate::response::Response;
use crate::upgrade::Upgraded;
use rustlavel_core::Json;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// How often a comment is written to keep an idle connection open.
pub const KEEPALIVE: Duration = Duration::from_secs(15);

/// One event.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Sent as `id:`. The browser remembers the last one and presents it as
    /// `Last-Event-ID` when it reconnects.
    pub id: Option<String>,
    /// Sent as `event:`. `EventSource` dispatches it by this name; without
    /// one it is a plain `message`.
    pub event: Option<String>,
    pub data: String,
    /// Sent as `retry:`, in milliseconds — how long the browser waits before
    /// reconnecting after this connection ends.
    pub retry: Option<Duration>,
}

impl Event {
    /// A plain `message` event.
    pub fn data(data: impl Into<String>) -> Event {
        Event { id: None, event: None, data: data.into(), retry: None }
    }

    /// A named event.
    pub fn named(event: impl Into<String>, data: impl Into<String>) -> Event {
        Event { id: None, event: Some(event.into()), data: data.into(), retry: None }
    }

    /// A named event carrying JSON, which is what a UI usually wants.
    pub fn json(event: impl Into<String>, data: Json) -> Event {
        Event::named(event, data.to_string())
    }

    pub fn id(mut self, id: impl Into<String>) -> Event {
        self.id = Some(id.into());
        self
    }

    pub fn retry(mut self, after: Duration) -> Event {
        self.retry = Some(after);
        self
    }

    /// The bytes on the wire.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = String::new();
        if let Some(id) = &self.id {
            out.push_str(&format!("id: {}\n", strip_newlines(id)));
        }
        if let Some(event) = &self.event {
            out.push_str(&format!("event: {}\n", strip_newlines(event)));
        }
        if let Some(retry) = self.retry {
            out.push_str(&format!("retry: {}\n", retry.as_millis()));
        }
        // One `data:` line per line of data. A newline left inside would end
        // the event where the sender did not mean it to.
        for line in self.data.split('\n') {
            out.push_str("data: ");
            out.push_str(line.trim_end_matches('\r'));
            out.push('\n');
        }
        out.push('\n');
        out.into_bytes()
    }
}

/// A field value may not contain a line break: it would start a new field.
fn strip_newlines(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

/// The sending half of an event stream, and the receiver `Response::events`
/// takes. `capacity` is how many events may wait for a slow client before the
/// sender is made to wait too.
pub fn channel(capacity: usize) -> (mpsc::Sender<Event>, mpsc::Receiver<Event>) {
    mpsc::channel(capacity.max(1))
}

impl Response {
    /// A `200` that stays open and writes each event from `events` as it
    /// arrives, until the sender is dropped or the client goes away.
    ///
    /// Drop the sender to end the stream cleanly. A send that fails means the
    /// client has gone; stop producing.
    pub fn events(events: mpsc::Receiver<Event>) -> Response {
        let events = std::sync::Mutex::new(Some(events));
        Response::ok()
            .with_header("content-type", "text/event-stream")
            .with_header("cache-control", "no-cache")
            // The body ends when the connection does. Said so, rather than
            // letting a client wait for a length that is never coming.
            .with_header("connection", "close")
            // Nginx buffers responses by default, which turns a live stream
            // into one delivered when it ends. This header asks it not to.
            .with_header("x-accel-buffering", "no")
            .streaming(move |connection: Upgraded| {
                let events = events.lock().ok().and_then(|mut held| held.take());
                async move {
                    if let Some(events) = events {
                        pump(connection, events).await;
                    }
                }
            })
    }
}

/// Write events until the source ends or the client leaves.
async fn pump(mut connection: Upgraded, mut events: mpsc::Receiver<Event>) {
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    keepalive.tick().await; // the first tick is immediate; skip it

    loop {
        let bytes = tokio::select! {
            event = events.recv() => match event {
                Some(event) => event.to_bytes(),
                // Every sender dropped: the stream is over.
                None => break,
            },
            _ = keepalive.tick() => b": keepalive\n\n".to_vec(),
        };

        // A write that fails is the client gone. Not an error to log — a
        // browser tab closing is the ordinary end of an event stream.
        if connection.writer.write_all(&bytes).await.is_err() {
            break;
        }
        if connection.writer.flush().await.is_err() {
            break;
        }
    }
    let _ = connection.writer.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_is_written_in_the_shape_event_source_reads() {
        let bytes = Event::named("progress", "42").id("7").retry(Duration::from_secs(2)).to_bytes();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "id: 7\nevent: progress\nretry: 2000\ndata: 42\n\n"
        );
        assert_eq!(String::from_utf8(Event::data("hello").to_bytes()).unwrap(), "data: hello\n\n");
    }

    /// A newline inside the data would end the event where the sender did not
    /// mean it to, and everything after it would be read as the next event.
    #[test]
    fn multi_line_data_is_split_across_data_lines() {
        let text = String::from_utf8(Event::data("line one\nline two\r\nline three").to_bytes()).unwrap();
        assert_eq!(text, "data: line one\ndata: line two\ndata: line three\n\n");
    }

    /// A newline in an id or a name would start a new field.
    #[test]
    fn a_line_break_cannot_be_smuggled_into_a_field() {
        let text = String::from_utf8(Event::named("a\nevent: b", "x").id("1\n2").to_bytes()).unwrap();
        // One `event:` line and one `id:` line, whatever the values held.
        assert_eq!(text.lines().filter(|l| l.starts_with("event:")).count(), 1, "{text}");
        assert_eq!(text.lines().filter(|l| l.starts_with("id:")).count(), 1, "{text}");
        assert!(text.starts_with("id: 1 2\nevent: a event: b\n"), "{text}");
    }

    #[test]
    fn json_events_carry_the_document_on_one_line() {
        let text = String::from_utf8(
            Event::json("progress", Json::object([("percent", Json::from(50))])).to_bytes(),
        )
        .unwrap();
        assert_eq!(text, "event: progress\ndata: {\"percent\":50}\n\n");
    }

    /// Over a real socket: the client must see each event as it is sent, not
    /// when the stream ends, and the connection must close when the sender is
    /// dropped. A stream that buffered until the end would pass every test
    /// above and be useless for the one thing it is for.
    #[tokio::test]
    async fn events_arrive_as_they_are_sent_and_the_stream_ends_with_the_sender() {
        use crate::{Request, Router, Server};
        use rustlavel_core::Context;
        use tokio::io::AsyncReadExt;
        use tokio::net::{TcpListener, TcpStream};

        // A handler that hands the sender to a channel the test controls, so
        // the test decides when each event goes out.
        let (hand_over, mut take) = mpsc::channel::<mpsc::Sender<Event>>(1);
        let hand_over = std::sync::Arc::new(hand_over);
        let mut router = Router::new();
        router.get("/events", move |_req: Request| {
            let hand_over = std::sync::Arc::clone(&hand_over);
            async move {
                let (tx, rx) = channel(8);
                hand_over.try_send(tx).expect("the test is waiting for the sender");
                Response::events(rx)
            }
        });
        let server = std::sync::Arc::new(Server::new(router, Context::default()));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            let _ = server.serve_connection(stream, peer).await;
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(b"GET /events HTTP/1.1\r\nHost: t\r\n\r\n").await.unwrap();

        // The head arrives before any event exists.
        let mut head = vec![0u8; 512];
        let n = client.read(&mut head).await.unwrap();
        let head = String::from_utf8_lossy(&head[..n]).to_string();
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert!(head.contains("text/event-stream"), "{head}");
        assert!(!head.to_ascii_lowercase().contains("content-length"), "{head}");

        let tx = take.recv().await.expect("the handler ran");

        // Each event is readable on its own, before the next is sent.
        for step in [10, 50, 100] {
            tx.send(Event::json("progress", Json::object([("percent", Json::from(step))]))).await.unwrap();
            let mut chunk = vec![0u8; 256];
            let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut chunk))
                .await
                .expect("an event did not arrive within two seconds — the stream is buffering")
                .unwrap();
            let text = String::from_utf8_lossy(&chunk[..n]).to_string();
            assert!(text.contains(&format!("\"percent\":{step}")), "step {step}: {text}");
        }

        // Dropping the sender ends the stream: the client reads EOF.
        drop(tx);
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut rest))
            .await
            .expect("the connection stayed open after the sender was dropped")
            .unwrap();
    }

    /// The headers are what make a browser treat it as a stream, and the one
    /// that must be absent is a content length.
    #[test]
    fn the_response_is_a_stream_with_no_length() {
        let (_tx, rx) = channel(4);
        let response = Response::events(rx);
        let head = String::from_utf8(response.to_bytes(false)).unwrap();

        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(head.contains("content-type: text/event-stream\r\n"), "{head}");
        assert!(head.contains("cache-control: no-cache\r\n"), "{head}");
        assert!(!head.to_ascii_lowercase().contains("content-length"), "a length would end the stream: {head}");
        assert!(response.upgrades(), "the socket is not handed over");
    }
}
