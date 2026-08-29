//! The client half: using somebody else's MCP server from an application.
//!
//! ```ignore
//! let mut mcp = McpClient::spawn("npx", &["-y", "@modelcontextprotocol/server-filesystem", "/tmp"])?;
//! mcp.initialize().await?;
//! let result = mcp.call_tool("read_file", Json::object([("path", "/tmp/a".into())])).await?;
//! ```
//!
//! Two transports, the two a server can offer: a child process spoken to over
//! its pipes, and HTTP. Both go through the same request/answer path, so the
//! handshake and id correlation are written once.

use crate::protocol::{
    self, Implementation, PROTOCOL_VERSION, PromptInfo, PromptMessage, ResourceInfo, ServerInfo,
    ToolInfo, ToolResult, method,
};
use crate::rpc::{self, Id, Message};
use rustlavel_client::Client as HttpClient;
use rustlavel_core::{Error, Json, Result};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines};
use tokio::process::Child;

/// A pipe both ways: a child process's stdio, or a duplex stream in a test.
struct Pipe {
    lines: Lines<BufReader<Box<dyn AsyncRead + Send + Unpin>>>,
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    /// Kept so the child is killed when the client is dropped rather than
    /// outliving the application that spawned it.
    child: Option<Child>,
}

enum Transport {
    Pipe(Pipe),
    Http { client: HttpClient, url: String },
}

/// A connection to an MCP server somebody else wrote.
pub struct McpClient {
    transport: Transport,
    client_info: Implementation,
    /// The next request id. Monotonic, so an answer can never be matched
    /// against a request from earlier in the session.
    next_id: i64,
    server: Option<ServerInfo>,
}

impl McpClient {
    /// Talk to a server over any pair of streams.
    pub fn over_pipe(
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        let boxed: Box<dyn AsyncRead + Send + Unpin> = Box::new(reader);
        McpClient {
            transport: Transport::Pipe(Pipe {
                lines: BufReader::new(boxed).lines(),
                writer: Box::new(writer),
                child: None,
            }),
            client_info: default_client_info(),
            next_id: 1,
            server: None,
        }
    }

    /// Launch a server as a child process and speak to it over its pipes.
    ///
    /// The child's stderr is inherited rather than captured: MCP servers use it
    /// for their own logging, and swallowing it makes a misbehaving server
    /// impossible to diagnose.
    pub fn spawn(program: &str, args: &[&str]) -> Result<Self> {
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::msg(format!("cannot start MCP server `{program}`: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::msg(format!("`{program}` gave us no stdin")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::msg(format!("`{program}` gave us no stdout")))?;

        let mut client = McpClient::over_pipe(stdout, stdin);
        if let Transport::Pipe(pipe) = &mut client.transport {
            pipe.child = Some(child);
        }
        Ok(client)
    }

    /// Talk to a server over its HTTP endpoint.
    pub fn over_http(url: impl Into<String>) -> Self {
        McpClient {
            transport: Transport::Http {
                client: HttpClient::new()
                    .default_header("accept", "application/json, text/event-stream"),
                url: url.into(),
            },
            client_info: default_client_info(),
            next_id: 1,
            server: None,
        }
    }

    /// Announce this application as something other than "rustlavel".
    pub fn identifying_as(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.client_info = Implementation::new(name, version);
        self
    }

    /// What the server said about itself, once the handshake has run.
    pub fn server_info(&self) -> Option<&ServerInfo> {
        self.server.as_ref()
    }

    /// Run the handshake.
    ///
    /// Two messages, in the order the specification requires: a request, then
    /// the `notifications/initialized` that tells the server the session is
    /// live. A server is allowed to reject calls made before it.
    pub async fn initialize(&mut self) -> Result<ServerInfo> {
        let params = Json::object([
            ("protocolVersion", Json::from(PROTOCOL_VERSION)),
            ("capabilities", Json::Object(Default::default())),
            ("clientInfo", self.client_info.to_json()),
        ]);

        let result = self.request(method::INITIALIZE, params).await?;
        let info = ServerInfo::from_json(&result).ok_or_else(|| {
            Error::msg(format!("the server sent a malformed initialize result: {result}"))
        })?;

        self.notify(method::INITIALIZED, Json::Null).await?;
        self.server = Some(info.clone());
        Ok(info)
    }

    pub async fn list_tools(&mut self) -> Result<Vec<ToolInfo>> {
        let result = self.request(method::TOOLS_LIST, Json::Null).await?;
        Ok(collect(&result, "tools", ToolInfo::from_json))
    }

    /// Call a tool.
    ///
    /// A tool that reports its own failure comes back as an `Ok` result with
    /// `is_error` set — that is the tool answering, not the call going wrong.
    /// Only a protocol failure becomes an `Err`.
    pub async fn call_tool(&mut self, name: &str, arguments: Json) -> Result<ToolResult> {
        let params = Json::object([("name", Json::from(name)), ("arguments", arguments)]);
        let result = self.request(method::TOOLS_CALL, params).await?;

        ToolResult::from_json(&result)
            .ok_or_else(|| Error::msg(format!("`{name}` returned a malformed result: {result}")))
    }

    pub async fn list_resources(&mut self) -> Result<Vec<ResourceInfo>> {
        let result = self.request(method::RESOURCES_LIST, Json::Null).await?;
        Ok(collect(&result, "resources", ResourceInfo::from_json))
    }

    /// Read a resource, joining its parts into one document.
    pub async fn read_resource(&mut self, uri: &str) -> Result<String> {
        let params = Json::object([("uri", Json::from(uri))]);
        let result = self.request(method::RESOURCES_READ, params).await?;

        let parts: Vec<String> = result
            .get("contents")
            .and_then(Json::as_array)
            .unwrap_or(&[])
            .iter()
            .filter_map(|entry| entry.get("text").and_then(Json::as_str))
            .map(str::to_string)
            .collect();
        Ok(parts.join("\n"))
    }

    pub async fn list_prompts(&mut self) -> Result<Vec<PromptInfo>> {
        let result = self.request(method::PROMPTS_LIST, Json::Null).await?;
        Ok(collect(&result, "prompts", PromptInfo::from_json))
    }

    pub async fn get_prompt(&mut self, name: &str, arguments: Json) -> Result<Vec<PromptMessage>> {
        let params = Json::object([("name", Json::from(name)), ("arguments", arguments)]);
        let result = self.request(method::PROMPTS_GET, params).await?;
        Ok(collect(&result, "messages", PromptMessage::from_json))
    }

    /// Send any request and wait for its answer.
    pub async fn request(&mut self, method: &str, params: Json) -> Result<Json> {
        let id = Id::Number(self.next_id);
        self.next_id += 1;

        let request = rpc::Request::new(id.clone(), method, params);
        let answer = self.exchange(&request, &id).await?;

        answer.result.map_err(|error| Error::msg(format!("{method}: {error}")))
    }

    /// Send a notification. There is nothing to wait for.
    pub async fn notify(&mut self, method: &str, params: Json) -> Result<()> {
        let notification = rpc::Notification::new(method, params);
        match &mut self.transport {
            Transport::Pipe(pipe) => pipe.send(&notification.to_json().to_string()).await,
            Transport::Http { client, url } => {
                client
                    .post(url.clone())
                    .json(notification.to_json())
                    .send()
                    .await?
                    .error_for_status()?;
                Ok(())
            }
        }
    }

    async fn exchange(&mut self, request: &rpc::Request, id: &Id) -> Result<rpc::Response> {
        match &mut self.transport {
            Transport::Pipe(pipe) => {
                pipe.send(&request.to_json().to_string()).await?;
                pipe.await_answer(id).await
            }
            Transport::Http { client, url } => {
                let response = client
                    .post(url.clone())
                    .json(request.to_json())
                    .send()
                    .await?
                    .error_for_status()?;

                match Message::parse(&response.text()).map_err(|e| Error::msg(e.to_string()))? {
                    Message::Response(answer) => Ok(answer),
                    other => Err(Error::msg(format!(
                        "expected an answer to {}, got {other}",
                        request.method
                    ))),
                }
            }
        }
    }
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let transport = match &self.transport {
            Transport::Pipe(pipe) if pipe.child.is_some() => "child process".to_string(),
            Transport::Pipe(_) => "pipe".to_string(),
            Transport::Http { url, .. } => url.clone(),
        };
        f.debug_struct("McpClient")
            .field("transport", &transport)
            .field("server", &self.server.as_ref().map(|info| &info.server.name))
            .finish()
    }
}

impl Pipe {
    async fn send(&mut self, message: &str) -> Result<()> {
        self.writer.write_all(message.as_bytes()).await.map_err(Error::Io)?;
        self.writer.write_all(b"\n").await.map_err(Error::Io)?;
        self.writer.flush().await.map_err(Error::Io)?;
        Ok(())
    }

    /// Read until the answer to `id` arrives.
    ///
    /// Anything else on the stream is skipped rather than treated as an error:
    /// a server may interleave its own notifications — progress, log lines —
    /// between a request and its answer, and correlation is by id alone.
    async fn await_answer(&mut self, id: &Id) -> Result<rpc::Response> {
        while let Some(line) = self.lines.next_line().await.map_err(Error::Io)? {
            if line.trim().is_empty() {
                continue;
            }
            match Message::parse(&line) {
                Ok(Message::Response(answer)) if answer.id.as_ref() == Some(id) => {
                    return Ok(answer);
                }
                // An error with a null id is the server's answer to something
                // it could not even parse — our request, since we only have one
                // in flight at a time.
                Ok(Message::Response(answer)) if answer.id.is_none() => return Ok(answer),
                Ok(_) => continue,
                Err(error) => {
                    return Err(Error::msg(format!("unreadable message from the server: {error}")));
                }
            }
        }
        Err(Error::msg(format!("the server closed before answering request {id}")))
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // `kill_on_drop` handles the spawned case, but dropping the child
        // explicitly makes the intent visible: a client going away must not
        // leave a server process behind.
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

fn default_client_info() -> Implementation {
    Implementation::new("rustlavel", env!("CARGO_PKG_VERSION"))
}

/// Pull a list out of a result, dropping entries we cannot read.
///
/// Being lenient is deliberate: a foreign server that adds a field we do not
/// understand should not break a listing.
fn collect<T>(result: &Json, key: &str, parse: impl Fn(&Json) -> Option<T>) -> Vec<T> {
    result
        .get(key)
        .and_then(Json::as_array)
        .unwrap_or(&[])
        .iter()
        .filter_map(parse)
        .collect()
}

/// The protocol version this client asks for.
pub fn protocol_version() -> &'static str {
    protocol::PROTOCOL_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Prompt;
    use crate::protocol::PromptArgument;
    use crate::resource::Resource;
    use crate::schema::Schema;
    use crate::server::Server;
    use crate::stdio;
    use crate::tool::Tool;
    use std::sync::Arc;

    fn demo_server() -> Arc<Server> {
        Arc::new(
            Server::new("demo", "2.0.0")
                .instructions("Call `shout` to shout.")
                .tool(Tool::new(
                    "shout",
                    "Upper-case a word",
                    Schema::object().string("word", "The word to shout"),
                    |args: Json| async move {
                        Ok(Json::from(
                            args.get("word").and_then(Json::as_str).unwrap_or("").to_uppercase(),
                        ))
                    },
                ))
                .tool(Tool::new("sulks", "Always fails", Schema::object(), |_: Json| async {
                    Err(Error::msg("not today"))
                }))
                .resource(Resource::text("app://motd", "Message", || async {
                    Ok("be excellent".to_string())
                }))
                .prompt(
                    Prompt::new("greet", "Greet somebody", |args: Json| async move {
                        let who = args.get("who").and_then(Json::as_str).unwrap_or("world");
                        Ok(vec![PromptMessage::user(format!("Say hello to {who}"))])
                    })
                    .argument(PromptArgument::new("who", "Who to greet")),
                ),
        )
    }

    /// Wire the client to our own server over an in-memory pipe.
    ///
    /// This is a real end-to-end check of both halves — the bytes are framed,
    /// written, parsed and correlated exactly as they would be against a
    /// subprocess — with no process and no network involved.
    fn connected() -> McpClient {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server_side);
        tokio::spawn(stdio::serve(demo_server(), server_read, server_write));

        let (client_read, client_write) = tokio::io::split(client_side);
        McpClient::over_pipe(client_read, client_write)
    }

    #[tokio::test]
    async fn the_handshake_reports_what_the_server_is() {
        let mut client = connected();
        let info = client.initialize().await.unwrap();

        assert_eq!(info.protocol_version, PROTOCOL_VERSION);
        assert_eq!(info.server, Implementation::new("demo", "2.0.0"));
        assert_eq!(info.instructions.as_deref(), Some("Call `shout` to shout."));
        assert!(info.supports("tools"));
        assert_eq!(client.server_info().unwrap().server.name, "demo");
    }

    #[tokio::test]
    async fn tools_are_listed_with_their_schemas() {
        let mut client = connected();
        client.initialize().await.unwrap();

        let tools = client.list_tools().await.unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert_eq!(names, ["shout", "sulks"]);
        assert_eq!(
            tools[0].input_schema.get("properties.word.type").unwrap().as_str(),
            Some("string")
        );
    }

    #[tokio::test]
    async fn a_tool_call_returns_the_servers_content() {
        let mut client = connected();
        client.initialize().await.unwrap();

        let result = client
            .call_tool("shout", Json::object([("word", Json::from("hello"))]))
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(result.text_content(), "HELLO");
    }

    #[tokio::test]
    async fn several_calls_share_one_session_and_stay_correlated() {
        let mut client = connected();
        client.initialize().await.unwrap();

        for word in ["one", "two", "three"] {
            let result =
                client.call_tool("shout", Json::object([("word", Json::from(word))])).await.unwrap();
            assert_eq!(result.text_content(), word.to_uppercase());
        }
    }

    #[tokio::test]
    async fn a_tool_that_fails_comes_back_as_an_error_result() {
        let mut client = connected();
        client.initialize().await.unwrap();

        let result = client.call_tool("sulks", Json::Null).await.unwrap();

        assert!(result.is_error);
        assert!(result.text_content().contains("not today"));
    }

    #[tokio::test]
    async fn a_protocol_failure_surfaces_as_an_error() {
        let mut client = connected();
        client.initialize().await.unwrap();

        let unknown = client.call_tool("nope", Json::Null).await.unwrap_err().to_string();
        assert!(unknown.contains("unknown tool `nope`"), "{unknown}");

        let wrong_type = client
            .call_tool("shout", Json::object([("word", Json::from(1))]))
            .await
            .unwrap_err()
            .to_string();
        assert!(wrong_type.contains("must be a string"), "{wrong_type}");
    }

    #[tokio::test]
    async fn resources_are_listed_and_read() {
        let mut client = connected();
        client.initialize().await.unwrap();

        let resources = client.list_resources().await.unwrap();
        assert_eq!(resources[0].uri, "app://motd");

        assert_eq!(client.read_resource("app://motd").await.unwrap(), "be excellent");
    }

    #[tokio::test]
    async fn prompts_are_listed_and_rendered() {
        let mut client = connected();
        client.initialize().await.unwrap();

        let prompts = client.list_prompts().await.unwrap();
        assert_eq!(prompts[0].name, "greet");
        assert!(prompts[0].arguments[0].required);

        let messages =
            client.get_prompt("greet", Json::object([("who", Json::from("Ada"))])).await.unwrap();
        assert_eq!(messages, [PromptMessage::user("Say hello to Ada")]);
    }

    #[tokio::test]
    async fn a_server_that_hangs_up_is_reported_rather_than_hanging() {
        // An empty script: the pipe is already at end-of-input.
        let mut client = McpClient::over_pipe(&b""[..], Vec::new());
        let error = client.initialize().await.unwrap_err().to_string();

        assert!(error.contains("closed before answering"), "{error}");
    }

    #[tokio::test]
    async fn notifications_from_the_server_are_skipped_while_correlating() {
        // A scripted server that chatters before answering, which a real one
        // does when it reports progress.
        let script = concat!(
            r#"{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#,
            "\n"
        );

        let mut client = McpClient::over_pipe(script.as_bytes(), Vec::new());
        assert_eq!(client.list_tools().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn an_answer_to_an_earlier_request_is_never_mistaken_for_this_one() {
        // Id 1 is stale by the time the second request goes out; the client
        // must keep reading until it sees id 2.
        let script = concat!(
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"stale","inputSchema":{}}]}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"fresh","inputSchema":{}}]}}"#,
            "\n"
        );

        let mut client = McpClient::over_pipe(script.as_bytes(), Vec::new());
        client.next_id = 2;

        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools[0].name, "fresh");
    }

    #[tokio::test]
    async fn the_client_writes_the_frames_a_server_expects() {
        // Reading back what was written proves the handshake is two messages in
        // the required order, each on its own line.
        let script = concat!(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"x","version":"1"}}}"#,
            "\n"
        );

        // A duplex stands in for the server: we prime it with the answer, then
        // read back everything the client sent.
        let (mut peer, client_side) = tokio::io::duplex(64 * 1024);
        peer.write_all(script.as_bytes()).await.unwrap();

        let (reader, writer) = tokio::io::split(client_side);
        let mut client = McpClient::over_pipe(reader, writer).identifying_as("my-app", "9.9");
        client.initialize().await.unwrap();
        drop(client);

        let mut written = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut written).await.unwrap();
        let text = String::from_utf8(written).unwrap();
        let lines: Vec<&str> = text.lines().collect();

        assert_eq!(lines.len(), 2);
        let first = Json::parse(lines[0]).unwrap();
        assert_eq!(first.get("method").unwrap().as_str(), Some("initialize"));
        assert_eq!(first.get("params.clientInfo.name").unwrap().as_str(), Some("my-app"));
        assert_eq!(first.get("params.protocolVersion").unwrap().as_str(), Some(PROTOCOL_VERSION));

        let second = Json::parse(lines[1]).unwrap();
        assert_eq!(second.get("method").unwrap().as_str(), Some("notifications/initialized"));
        assert!(second.get("id").is_none(), "the initialized message is a notification");
    }

    #[tokio::test]
    async fn spawning_a_program_that_does_not_exist_says_so() {
        let error = McpClient::spawn("rustlavel-no-such-mcp-server", &[]).unwrap_err().to_string();
        assert!(error.contains("cannot start MCP server"), "{error}");
    }

    #[test]
    fn the_client_announces_the_protocol_version_it_speaks() {
        assert_eq!(protocol_version(), "2025-06-18");
    }
}
