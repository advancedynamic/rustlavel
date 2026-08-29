//! The SMTP conversation itself.
//!
//! Generic over the stream, so the same code that talks to a real server over
//! TLS is exercised in tests against `tokio::io::duplex` — the entire protocol
//! is covered without a socket, a port, or a server anywhere.

use super::reply::{Reply, complete_reply_end};
use super::stream::{self, SmtpStream};
use super::{Encryption, SmtpConfig, may_authenticate};
use crate::encode::base64_encode;
use rustlavel_core::{Error, Result};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// A reply longer than this is a server misbehaving, not a reply.
const MAX_REPLY_BYTES: usize = 64 * 1024;

/// One SMTP session.
pub struct SmtpClient<S> {
    stream: S,
    /// Bytes read from the socket but not yet consumed as a reply.
    buffer: Vec<u8>,
    timeout: Duration,
    /// The capability lines `EHLO` returned, upper-cased.
    capabilities: Vec<String>,
    /// The server's name, for error messages — the only thing that makes an
    /// SMTP failure actionable when three services send mail.
    server: String,
}

impl<S: AsyncRead + AsyncWrite + Unpin> SmtpClient<S> {
    /// Take over an open stream and read the server's greeting.
    pub async fn open(stream: S, server: impl Into<String>, timeout: Duration) -> Result<Self> {
        let mut client = SmtpClient {
            stream,
            buffer: Vec::with_capacity(4096),
            timeout,
            capabilities: Vec::new(),
            server: server.into(),
        };

        let greeting = client.read_reply().await?;
        client.require(greeting, "the connection", &[220])?;
        Ok(client)
    }

    /// Say hello and learn what the server supports.
    ///
    /// Falls back to `HELO` when `EHLO` is refused: a server old enough to
    /// reject it has no capabilities to report anyway.
    pub async fn ehlo(&mut self, name: &str) -> Result<()> {
        let reply = self.command(&format!("EHLO {name}")).await?;

        if reply.is_positive() {
            // The first line is the greeting text, not a capability.
            self.capabilities =
                reply.lines.iter().skip(1).map(|line| line.trim().to_uppercase()).collect();
            return Ok(());
        }

        let fallback = self.command(&format!("HELO {name}")).await?;
        self.require(fallback, "HELO", &[250])?;
        self.capabilities.clear();
        Ok(())
    }

    /// Every capability line, as the server sent them.
    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    /// Whether a capability such as `STARTTLS` or `8BITMIME` was advertised.
    pub fn supports(&self, capability: &str) -> bool {
        let capability = capability.to_uppercase();
        self.capabilities
            .iter()
            .any(|line| line == &capability || line.starts_with(&format!("{capability} ")))
    }

    /// The mechanisms listed on the `AUTH` capability line.
    pub fn auth_mechanisms(&self) -> Vec<String> {
        self.capabilities
            .iter()
            .filter_map(|line| line.strip_prefix("AUTH").map(|rest| rest.trim_start_matches('=')))
            .flat_map(|rest| rest.split_whitespace())
            .map(str::to_string)
            .collect()
    }

    /// Authenticate, preferring `AUTH PLAIN` and falling back to `AUTH LOGIN`.
    ///
    /// Both send the password base64-encoded, which is not encryption — the
    /// caller is responsible for having a TLS connection first.
    pub async fn authenticate(&mut self, username: &str, password: &str) -> Result<()> {
        let mechanisms = self.auth_mechanisms();
        let use_login = mechanisms.iter().any(|m| m == "LOGIN")
            && !mechanisms.iter().any(|m| m == "PLAIN");

        if use_login {
            self.auth_login(username, password).await
        } else {
            self.auth_plain(username, password).await
        }
    }

    async fn auth_plain(&mut self, username: &str, password: &str) -> Result<()> {
        // The PLAIN mechanism is authorize-id NUL authenticate-id NUL password.
        let secret = base64_encode(format!("\0{username}\0{password}").as_bytes());
        let reply = self.command_hiding_payload("AUTH PLAIN", &secret).await?;
        self.require(reply, "AUTH PLAIN", &[235])?;
        Ok(())
    }

    async fn auth_login(&mut self, username: &str, password: &str) -> Result<()> {
        let prompt = self.command("AUTH LOGIN").await?;
        self.require(prompt, "AUTH LOGIN", &[334])?;

        let user_reply = self.command_hiding_payload("", &base64_encode(username.as_bytes())).await?;
        self.require(user_reply, "AUTH LOGIN (username)", &[334])?;

        let final_reply =
            self.command_hiding_payload("", &base64_encode(password.as_bytes())).await?;
        self.require(final_reply, "AUTH LOGIN (password)", &[235])?;
        Ok(())
    }

    /// Send one message: envelope, then data.
    ///
    /// Every recipient is offered separately, and a rejection names the address
    /// the server refused — "send failed" tells nobody anything at 2am.
    pub async fn send_message(
        &mut self,
        from: &str,
        recipients: &[String],
        data: &str,
    ) -> Result<()> {
        let mail_from = self.command(&format!("MAIL FROM:<{from}>")).await?;
        self.require(mail_from, &format!("MAIL FROM:<{from}>"), &[250, 251])?;

        for recipient in recipients {
            let reply = self.command(&format!("RCPT TO:<{recipient}>")).await?;
            self.require(reply, &format!("RCPT TO:<{recipient}>"), &[250, 251])?;
        }

        let ready = self.command("DATA").await?;
        self.require(ready, "DATA", &[354])?;

        self.write_data(data).await?;
        let accepted = self.read_reply().await?;
        self.require(accepted, "the message body", &[250])?;
        Ok(())
    }

    /// Say goodbye and close. A failure here is not worth reporting: the
    /// message was already accepted.
    pub async fn quit(mut self) {
        let _ = self.write_line("QUIT").await;
        let _ = tokio::time::timeout(self.timeout, self.read_reply()).await;
        let _ = self.stream.shutdown().await;
    }

    /// Send one command and read its reply.
    pub async fn command(&mut self, line: &str) -> Result<Reply> {
        if !line.is_empty() {
            rustlavel_core::debug!("smtp > {line}");
        }
        self.write_line(line).await?;
        self.read_reply().await
    }

    /// Send `prefix` followed by a secret, logging only the prefix.
    ///
    /// The base64 in an `AUTH` line is the password. It must never reach a log
    /// file, an event, or an error message.
    async fn command_hiding_payload(&mut self, prefix: &str, secret: &str) -> Result<Reply> {
        rustlavel_core::debug!("smtp > {} <credentials>", if prefix.is_empty() { "AUTH" } else { prefix });
        let line =
            if prefix.is_empty() { secret.to_string() } else { format!("{prefix} {secret}") };
        self.write_line(&line).await?;
        self.read_reply().await
    }

    async fn write_line(&mut self, line: &str) -> Result<()> {
        self.write_all(format!("{line}\r\n").as_bytes()).await
    }

    /// Write the message body and the terminating `.` line.
    async fn write_data(&mut self, data: &str) -> Result<()> {
        self.write_all(data.as_bytes()).await?;
        // The terminator is CRLF `.` CRLF; if the body already ends in CRLF,
        // adding another would put an empty line at the end of every message.
        let terminator = if data.ends_with("\r\n") { ".\r\n" } else { "\r\n.\r\n" };
        self.write_all(terminator.as_bytes()).await
    }

    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        let write = async {
            self.stream.write_all(bytes).await?;
            self.stream.flush().await
        };

        tokio::time::timeout(self.timeout, write)
            .await
            .map_err(|_| Error::msg(format!("timed out writing to {}", self.server)))?
            .map_err(|e| Error::msg(format!("cannot write to {}: {e}", self.server)))
    }

    /// Read exactly one complete reply, however many lines it takes.
    pub async fn read_reply(&mut self) -> Result<Reply> {
        loop {
            if let Some(end) = complete_reply_end(&self.buffer) {
                let raw = String::from_utf8_lossy(&self.buffer[..end]).into_owned();
                self.buffer.drain(..end);
                let reply = Reply::parse(&raw)?;
                rustlavel_core::debug!("smtp < {} {}", reply.code, reply.text());
                return Ok(reply);
            }

            if self.buffer.len() > MAX_REPLY_BYTES {
                return Err(Error::Protocol(format!(
                    "{} sent more than {MAX_REPLY_BYTES} bytes without finishing a reply",
                    self.server
                )));
            }

            let mut chunk = [0u8; 4096];
            let read = tokio::time::timeout(self.timeout, self.stream.read(&mut chunk))
                .await
                .map_err(|_| {
                    Error::msg(format!(
                        "timed out waiting for a reply from {}. Is mail.host right, and is \
                         anything filtering port traffic?",
                        self.server
                    ))
                })?
                .map_err(|e| Error::msg(format!("cannot read from {}: {e}", self.server)))?;

            if read == 0 {
                return Err(Error::Protocol(format!(
                    "{} closed the connection without replying",
                    self.server
                )));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }

    /// Accept the reply, or turn it into an error that says what to do.
    fn require(&self, reply: Reply, doing: &str, accepted: &[u16]) -> Result<Reply> {
        if accepted.contains(&reply.code) {
            return Ok(reply);
        }

        let base = format!(
            "{} rejected {doing} with {} {}",
            self.server,
            reply.code,
            reply.text()
        );

        match advice(reply.code) {
            Some(advice) => Err(Error::msg(format!("{base}\n  {advice}"))),
            None => Err(Error::msg(base)),
        }
    }
}

impl SmtpClient<SmtpStream> {
    /// Connect, greet, upgrade to TLS, and authenticate — everything up to the
    /// point where a message can be sent.
    pub async fn connect(config: &SmtpConfig) -> Result<SmtpClient<SmtpStream>> {
        let stream =
            stream::connect(&config.host, config.port, config.implicit_tls(), config.timeout)
                .await?;

        let mut client = SmtpClient::open(stream, config.host.clone(), config.timeout).await?;
        client.ehlo(&config.hello).await?;

        if config.wants_starttls() {
            if client.supports("STARTTLS") {
                client = client.start_tls(&config.host, &config.hello).await?;
            } else if config.requires_starttls() {
                return Err(Error::msg(format!(
                    "{} does not offer STARTTLS, but mail.encryption asks for it. Either the \
                     port is wrong (465 for implicit TLS, 587 for STARTTLS) or the server is \
                     not the one you meant.",
                    config.host
                )));
            }
        }

        if !config.username.is_empty() {
            if !may_authenticate(client.stream.is_encrypted(), &config.host) {
                return Err(Error::msg(format!(
                    "refusing to send the password for `{}` to {} over an unencrypted \
                     connection. Use port 587 with STARTTLS, or 465, or set mail.encryption to \
                     `none` only for a mail catcher on localhost.",
                    config.username, config.host
                )));
            }
            client.authenticate(&config.username, &config.password).await?;
        } else if config.encryption == Encryption::StartTls && !client.stream.is_encrypted() {
            return Err(Error::msg(format!("could not establish TLS with {}", config.host)));
        }

        Ok(client)
    }

    /// Upgrade the connection in place, then greet again over TLS.
    ///
    /// Consumes the client because the stream changes type. Everything the
    /// server said before the handshake is discarded: capabilities learned in
    /// cleartext are not trustworthy, which is exactly the STARTTLS stripping
    /// attack this re-`EHLO` defends against.
    pub async fn start_tls(
        mut self,
        host: &str,
        hello: &str,
    ) -> Result<SmtpClient<SmtpStream>> {
        let reply = self.command("STARTTLS").await?;
        self.require(reply, "STARTTLS", &[220])?;

        if !self.buffer.is_empty() {
            return Err(Error::Protocol(format!(
                "{host} sent data after agreeing to STARTTLS, before the handshake — refusing \
                 to continue"
            )));
        }

        let upgraded = self.stream.upgrade(host).await?;
        let mut client = SmtpClient {
            stream: upgraded,
            buffer: Vec::with_capacity(4096),
            timeout: self.timeout,
            capabilities: Vec::new(),
            server: self.server,
        };

        client.ehlo(hello).await?;
        Ok(client)
    }

    pub fn is_encrypted(&self) -> bool {
        self.stream.is_encrypted()
    }
}

/// What to do about the codes that actually come up.
fn advice(code: u16) -> Option<&'static str> {
    match code {
        421 => Some("The server is closing the connection — it is overloaded or throttling. Retry later."),
        450..=452 => Some("A temporary failure: the message was not lost, but was not sent either. Retry."),
        454 => Some("The server could not start TLS. Check mail.port and mail.encryption."),
        530 => Some("The server wants authentication first. Set mail.username and mail.password."),
        534 | 535 => Some("Authentication was refused. Check mail.username and mail.password — a provider may also need an app-specific password rather than the account one."),
        550 | 551 | 553 => Some("The recipient was refused. Check the address, and whether this sender is allowed to send as that From address."),
        552 => Some("The message is too large. Reduce the attachments."),
        554 => Some("The server refused the transaction outright — often a spam rule, or a From address the server will not relay for."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream};
    use tokio::task::JoinHandle;

    /// One step of a scripted conversation: what the server expects to be told,
    /// and what it answers.
    struct Step {
        expects: &'static str,
        replies: &'static str,
    }

    fn step(expects: &'static str, replies: &'static str) -> Step {
        Step { expects, replies }
    }

    /// The marker for "read a whole DATA block, not a single line".
    const BODY: &str = "<body>";

    /// A scripted SMTP server on an in-memory pipe.
    ///
    /// Returns the client end plus a handle that yields everything the client
    /// said, so assertions happen on the test's thread where a failure is
    /// readable. Each test gets its own pipe — there is no shared fixture and
    /// no port, so tests can run concurrently without coordinating.
    fn scripted(greeting: &'static str, script: Vec<Step>) -> (DuplexStream, JoinHandle<Vec<String>>) {
        let (client, server) = tokio::io::duplex(64 * 1024);

        let handle = tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(server);
            let mut reader = BufReader::new(reader);
            let mut said: Vec<String> = Vec::new();

            writer.write_all(greeting.as_bytes()).await.expect("greeting");

            for step in script {
                if step.expects == BODY {
                    let mut body = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            break;
                        }
                        if line == ".\r\n" || line == ".\n" {
                            break;
                        }
                        body.push_str(&line);
                    }
                    said.push(body);
                } else {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        break;
                    }
                    said.push(line.trim_end().to_string());
                }
                writer.write_all(step.replies.as_bytes()).await.expect("reply");
            }

            said
        });

        (client, handle)
    }

    fn timeout() -> Duration {
        Duration::from_secs(5)
    }

    #[tokio::test]
    async fn a_whole_conversation_runs_from_greeting_to_quit() {
        let (pipe, server) = scripted(
            "220 smtp.example.com ESMTP ready\r\n",
            vec![
                step(
                    "EHLO app.example.com",
                    "250-smtp.example.com\r\n250-SIZE 35882577\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n",
                ),
                step("AUTH PLAIN AGFkYQBodW50ZXIy", "235 2.7.0 Accepted\r\n"),
                step("MAIL FROM:<ada@example.com>", "250 2.1.0 Ok\r\n"),
                step("RCPT TO:<grace@example.com>", "250 2.1.5 Ok\r\n"),
                step("RCPT TO:<alan@example.com>", "250 2.1.5 Ok\r\n"),
                step("DATA", "354 End data with <CR><LF>.<CR><LF>\r\n"),
                step(BODY, "250 2.0.0 Ok: queued as 4A2B1\r\n"),
                step("QUIT", "221 2.0.0 Bye\r\n"),
            ],
        );

        let mut client = SmtpClient::open(pipe, "smtp.example.com", timeout()).await.unwrap();
        client.ehlo("app.example.com").await.unwrap();

        assert!(client.supports("8BITMIME"));
        assert!(client.supports("size"));
        assert!(!client.supports("STARTTLS"));
        assert_eq!(client.auth_mechanisms(), vec!["PLAIN", "LOGIN"]);

        client.authenticate("ada", "hunter2").await.unwrap();
        client
            .send_message(
                "ada@example.com",
                &["grace@example.com".into(), "alan@example.com".into()],
                "Subject: Hi\r\n\r\nbody\r\n",
            )
            .await
            .unwrap();
        client.quit().await;

        let said = server.await.unwrap();
        assert_eq!(said[0], "EHLO app.example.com");
        assert_eq!(said[1], "AUTH PLAIN AGFkYQBodW50ZXIy");
        assert_eq!(said[2], "MAIL FROM:<ada@example.com>");
        assert_eq!(said[3], "RCPT TO:<grace@example.com>");
        assert_eq!(said[4], "RCPT TO:<alan@example.com>");
        assert_eq!(said[5], "DATA");
        assert_eq!(said[6], "Subject: Hi\r\n\r\nbody\r\n");
        assert_eq!(said[7], "QUIT");
    }

    #[tokio::test]
    async fn auth_login_is_used_when_it_is_the_only_mechanism_offered() {
        let (pipe, server) = scripted(
            "220 ready\r\n",
            vec![
                step("EHLO app", "250-hello\r\n250 AUTH LOGIN\r\n"),
                step("AUTH LOGIN", "334 VXNlcm5hbWU6\r\n"),
                step("YWRh", "334 UGFzc3dvcmQ6\r\n"),
                step("aHVudGVyMg==", "235 Accepted\r\n"),
            ],
        );

        let mut client = SmtpClient::open(pipe, "smtp.example.com", timeout()).await.unwrap();
        client.ehlo("app").await.unwrap();
        client.authenticate("ada", "hunter2").await.unwrap();

        let said = server.await.unwrap();
        assert_eq!(said, vec!["EHLO app", "AUTH LOGIN", "YWRh", "aHVudGVyMg=="]);
    }

    #[tokio::test]
    async fn a_rejected_password_names_the_code_and_the_server_text_but_not_the_password() {
        let (pipe, server) = scripted(
            "220 ready\r\n",
            vec![
                step("EHLO app", "250-hello\r\n250 AUTH PLAIN\r\n"),
                step(
                    "AUTH PLAIN AGFkYQBodW50ZXIy",
                    "535 5.7.8 Error: authentication failed: bad credentials\r\n",
                ),
            ],
        );

        let mut client = SmtpClient::open(pipe, "smtp.example.com", timeout()).await.unwrap();
        client.ehlo("app").await.unwrap();
        let error = client.authenticate("ada", "hunter2").await.unwrap_err().to_string();

        assert!(error.contains("535"), "{error}");
        assert!(error.contains("authentication failed: bad credentials"), "{error}");
        assert!(error.contains("smtp.example.com"), "{error}");
        assert!(error.contains("mail.username"), "{error}");
        // The password, and its base64, must not be anywhere in the message.
        assert!(!error.contains("hunter2"), "{error}");
        assert!(!error.contains("AGFkYQBodW50ZXIy"), "{error}");

        let _ = server.await;
    }

    #[tokio::test]
    async fn a_rejected_recipient_names_the_address_the_server_refused() {
        let (pipe, server) = scripted(
            "220 ready\r\n",
            vec![
                step("EHLO app", "250 hello\r\n"),
                step("MAIL FROM:<ada@example.com>", "250 Ok\r\n"),
                step("RCPT TO:<grace@example.com>", "250 Ok\r\n"),
                step(
                    "RCPT TO:<nobody@example.com>",
                    "550 5.1.1 <nobody@example.com>: Recipient address rejected: User unknown\r\n",
                ),
            ],
        );

        let mut client = SmtpClient::open(pipe, "smtp.example.com", timeout()).await.unwrap();
        client.ehlo("app").await.unwrap();

        let error = client
            .send_message(
                "ada@example.com",
                &["grace@example.com".into(), "nobody@example.com".into()],
                "body",
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("RCPT TO:<nobody@example.com>"), "{error}");
        assert!(error.contains("550"), "{error}");
        assert!(error.contains("User unknown"), "{error}");
        assert!(error.contains("recipient was refused"), "{error}");

        let _ = server.await;
    }

    #[tokio::test]
    async fn a_server_offering_starttls_is_recognised_before_anything_is_sent() {
        // The handshake itself needs a TLS server and is not exercised offline;
        // what is testable here is the decision that leads to it, which is
        // where a misconfigured port actually goes wrong.
        let (pipe, server) = scripted(
            "220 ready\r\n",
            vec![step("EHLO app", "250-hello\r\n250-STARTTLS\r\n250 AUTH=LOGIN PLAIN\r\n")],
        );

        let mut client = SmtpClient::open(pipe, "smtp.example.com", timeout()).await.unwrap();
        client.ehlo("app").await.unwrap();

        assert!(client.supports("STARTTLS"));
        // Some servers still write the old `AUTH=` form.
        assert_eq!(client.auth_mechanisms(), vec!["LOGIN", "PLAIN"]);

        let _ = server.await;
    }

    #[tokio::test]
    async fn a_server_that_refuses_the_greeting_is_reported_as_such() {
        let (pipe, server) = scripted("554 no service here\r\n", vec![]);

        let message = match SmtpClient::open(pipe, "smtp.example.com", timeout()).await {
            Err(error) => error.to_string(),
            Ok(_) => panic!("a 554 greeting is not a usable session"),
        };

        assert!(message.contains("554"), "{message}");
        assert!(message.contains("no service here"), "{message}");

        let _ = server.await;
    }

    #[tokio::test]
    async fn ehlo_falls_back_to_helo_on_an_old_server() {
        let (pipe, server) = scripted(
            "220 ready\r\n",
            vec![
                step("EHLO app", "500 5.5.1 Command unrecognized\r\n"),
                step("HELO app", "250 hello\r\n"),
            ],
        );

        let mut client = SmtpClient::open(pipe, "smtp.example.com", timeout()).await.unwrap();
        client.ehlo("app").await.unwrap();

        assert!(client.capabilities().is_empty());
        assert_eq!(server.await.unwrap(), vec!["EHLO app", "HELO app"]);
    }

    #[tokio::test]
    async fn a_body_ending_without_crlf_still_gets_a_terminator_of_its_own() {
        let (pipe, server) = scripted(
            "220 ready\r\n",
            vec![
                step("EHLO app", "250 hello\r\n"),
                step("MAIL FROM:<ada@example.com>", "250 Ok\r\n"),
                step("RCPT TO:<grace@example.com>", "250 Ok\r\n"),
                step("DATA", "354 go\r\n"),
                step(BODY, "250 queued\r\n"),
            ],
        );

        let mut client = SmtpClient::open(pipe, "smtp.example.com", timeout()).await.unwrap();
        client.ehlo("app").await.unwrap();
        client
            .send_message("ada@example.com", &["grace@example.com".into()], "no trailing newline")
            .await
            .unwrap();

        let said = server.await.unwrap();
        assert_eq!(said[4], "no trailing newline\r\n");
    }

    #[tokio::test]
    async fn a_server_that_hangs_up_mid_conversation_says_so() {
        // The server reads the EHLO, answers nothing, and drops the pipe.
        let (pipe, server) = scripted("220 ready\r\n", vec![step("EHLO app", "")]);

        let mut client = SmtpClient::open(pipe, "smtp.example.com", timeout()).await.unwrap();
        let error = client.ehlo("app").await.unwrap_err().to_string();

        assert!(error.contains("closed the connection"), "{error}");
        assert!(error.contains("smtp.example.com"), "{error}");
        let _ = server.await;
    }

    #[test]
    fn advice_covers_the_codes_that_actually_come_up() {
        assert!(advice(535).unwrap().contains("mail.username"));
        assert!(advice(552).unwrap().contains("too large"));
        assert!(advice(250).is_none());
    }
}
