//! The stdio transport: newline-delimited JSON over a pipe.
//!
//! This is the transport desktop clients use. They launch the application as a
//! child process and talk to it over its stdin and stdout, which means one
//! rule matters above all others: **nothing may print to stdout except
//! protocol messages**. A stray `println!` corrupts the stream. Rustlavel's
//! own logging goes to stderr, so it is safe alongside this.
//!
//! ```ignore
//! #[tokio::main]
//! async fn main() -> rustlavel_core::Result<()> {
//!     stdio::serve_stdio(Arc::new(mcp_server())).await
//! }
//! ```

use crate::server::Server;
use rustlavel_core::{Error, Result};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// Serve over the process's real stdin and stdout.
pub async fn serve_stdio(server: Arc<Server>) -> Result<()> {
    serve(server, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Serve over any pair of streams.
///
/// Being generic is what lets the tests drive the whole transport over
/// `tokio::io::duplex` instead of a real subprocess — the code under test is
/// then exactly the code that runs in production.
pub async fn serve<R, W>(server: Arc<Server>, reader: R, writer: W) -> Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let mut lines = BufReader::new(reader).lines();
    let mut writer = writer;

    while let Some(line) = lines.next_line().await.map_err(Error::Io)? {
        // Clients sometimes pad frames with blank lines; they are not messages.
        if line.trim().is_empty() {
            continue;
        }

        if let Some(answer) = server.handle_text(&line).await {
            write_frame(&mut writer, &answer).await?;
        }
    }

    Ok(())
}

/// Write one message and flush.
///
/// The flush is not optional: the client is blocked reading a line, and a
/// buffered answer is a deadlock rather than a slow response.
async fn write_frame<W>(writer: &mut W, message: &str) -> Result<()>
where
    W: AsyncWrite + Unpin + Send,
{
    writer.write_all(message.as_bytes()).await.map_err(Error::Io)?;
    writer.write_all(b"\n").await.map_err(Error::Io)?;
    writer.flush().await.map_err(Error::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PROTOCOL_VERSION;
    use crate::schema::Schema;
    use crate::tool::Tool;
    use rustlavel_core::Json;
    use tokio::io::AsyncBufReadExt;

    fn server() -> Arc<Server> {
        Arc::new(Server::new("stdio-test", "0.1.0").tool(Tool::new(
            "shout",
            "Upper-case a word",
            Schema::object().string("word", "The word to shout"),
            |args: Json| async move {
                Ok(Json::from(
                    args.get("word").and_then(Json::as_str).unwrap_or("").to_uppercase(),
                ))
            },
        )))
    }

    /// Feed a scripted stdin through the transport and collect what it wrote.
    ///
    /// The input is a complete script rather than a live pipe, so the loop ends
    /// on end-of-input and the test cannot hang.
    async fn exchange(input: &str) -> Vec<Json> {
        let mut output = Vec::new();
        serve(server(), input.as_bytes(), &mut output).await.unwrap();

        let mut lines = BufReader::new(output.as_slice()).lines();
        let mut messages = Vec::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            messages.push(Json::parse(&line).unwrap());
        }
        messages
    }

    #[tokio::test]
    async fn a_full_session_runs_over_newline_delimited_json() {
        let messages = exchange(concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"shout","arguments":{"word":"hello"}}}"#,
            "\n",
        ))
        .await;

        // Two answers, not three: the notification is silent.
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].get("id").unwrap().as_i64(), Some(1));
        assert_eq!(
            messages[0].get("result.protocolVersion").unwrap().as_str(),
            Some(PROTOCOL_VERSION)
        );
        assert_eq!(messages[1].get("id").unwrap().as_i64(), Some(2));
        assert_eq!(messages[1].get("result.content.0.text").unwrap().as_str(), Some("HELLO"));
    }

    #[tokio::test]
    async fn blank_lines_between_frames_are_ignored() {
        let messages = exchange("\n\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\n").await;

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].get("result").unwrap().to_string(), "{}");
    }

    #[tokio::test]
    async fn a_garbled_line_is_answered_and_the_session_continues() {
        let messages = exchange(concat!(
            "not json at all\n",
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#,
            "\n"
        ))
        .await;

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].get("error.code").unwrap().as_i64(), Some(-32700));
        assert_eq!(messages[1].get("id").unwrap().as_i64(), Some(9));
    }

    #[tokio::test]
    async fn each_message_is_written_on_exactly_one_line() {
        // A pretty-printed answer would break every client on this transport,
        // so the framing is asserted on the raw bytes.
        let mut output = Vec::new();
        serve(
            server(),
            &b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n"[..],
            &mut output,
        )
        .await
        .unwrap();

        let text = String::from_utf8(output).unwrap();
        assert_eq!(text.matches('\n').count(), 1);
        assert!(text.ends_with('\n'));
    }

    #[tokio::test]
    async fn the_transport_survives_a_panicking_tool_over_the_wire() {
        let server = Arc::new(Server::new("stdio-test", "0.1.0").tool(Tool::new(
            "explodes",
            "Panics",
            Schema::object(),
            |_: Json| async { panic!("boom") },
        )));

        let mut output = Vec::new();
        serve(
            Arc::clone(&server),
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"explodes"}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
                "\n"
            )
            .as_bytes(),
            &mut output,
        )
        .await
        .unwrap();

        let text = String::from_utf8(output).unwrap();
        let mut lines = text.lines();
        let first = Json::parse(lines.next().unwrap()).unwrap();
        let second = Json::parse(lines.next().unwrap()).unwrap();

        assert_eq!(first.get("result.isError").unwrap().as_bool(), Some(true));
        // The stream is still open and answering after the panic.
        assert_eq!(second.get("id").unwrap().as_i64(), Some(2));
    }

    #[tokio::test]
    async fn the_transport_serves_a_live_duplex_pipe() {
        // The closest thing to a real desktop client without spawning one: the
        // server reads and writes a socket-like stream, not a finished buffer.
        let (mut client, server_side) = tokio::io::duplex(8 * 1024);
        let (server_read, server_write) = tokio::io::split(server_side);
        let task = tokio::spawn(serve(server(), server_read, server_write));

        let request = format!("{}\n", r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        tokio::io::AsyncWriteExt::write_all(&mut client, request.as_bytes()).await.unwrap();

        let mut reader = BufReader::new(&mut client);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();

        let answer = Json::parse(&line).unwrap();
        assert_eq!(answer.get("result.tools.0.name").unwrap().as_str(), Some("shout"));

        // Closing the client's end ends the session cleanly.
        drop(client);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_dispatcher_error_still_carries_the_request_id() {
        let messages = exchange(concat!(
            r#"{"jsonrpc":"2.0","id":"x","method":"tools/call","params":{"name":"shout","arguments":{"word":7}}}"#,
            "\n"
        ))
        .await;

        assert_eq!(messages[0].get("id").unwrap().as_str(), Some("x"));
        assert_eq!(messages[0].get("error.code").unwrap().as_i64(), Some(-32602));
    }

    #[tokio::test]
    async fn a_batch_arriving_on_one_line_is_answered_on_one_line() {
        let messages = exchange(concat!(
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","id":2,"method":"ping"}]"#,
            "\n"
        ))
        .await;

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_unknown_method_over_the_transport_is_method_not_found() {
        let messages =
            exchange("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/teleport\"}\n").await;

        assert_eq!(messages[0].get("error.code").unwrap().as_i64(), Some(-32601));
    }
}
