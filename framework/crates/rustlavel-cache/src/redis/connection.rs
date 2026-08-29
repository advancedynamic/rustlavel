//! One Redis connection: a TCP socket, a read buffer, and the handshake.
//!
//! Nothing here is generic over a transport — Redis is a request/response
//! protocol over a stream, so the whole client is "write a command, read one
//! reply", plus the discipline of never reusing a connection whose framing may
//! have drifted.

use super::config::RedisConfig;
use super::resp::{self, Value};
use rustlavel_core::{Error, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How much to read from the socket per syscall.
const READ_CHUNK: usize = 8 * 1024;

pub struct Connection {
    stream: TcpStream,
    /// Bytes read from the socket but not yet consumed as a reply. Redis is
    /// allowed to answer in as many TCP segments as it likes, and a reply for
    /// the *next* command can arrive in the same segment as this one.
    buffer: Vec<u8>,
    /// Where in `buffer` the unconsumed bytes start, so a reply that arrived
    /// alongside the previous one does not force a memmove per command.
    consumed: usize,
    config: RedisConfig,
    /// Set the moment framing might be wrong. A connection that timed out
    /// mid-reply must never go back into the pool: the next borrower would read
    /// somebody else's answer.
    broken: bool,
}

impl Connection {
    /// Open a connection and complete AUTH and SELECT.
    pub async fn connect(config: &RedisConfig) -> Result<Connection> {
        let address = config.address();

        let stream = tokio::time::timeout(config.connect_timeout, TcpStream::connect(&address))
            .await
            .map_err(|_| {
                Error::msg(format!("timed out connecting to {address}. Is Redis running and reachable?"))
            })?
            .map_err(|e| Error::msg(format!("cannot connect to {}: {e}", config.redacted_url())))?;

        // Cache reads are small and latency-sensitive; Nagle would batch them
        // into an extra round trip's worth of delay.
        let _ = stream.set_nodelay(true);

        let mut connection = Connection {
            stream,
            buffer: Vec::with_capacity(READ_CHUNK),
            consumed: 0,
            config: config.clone(),
            broken: false,
        };

        connection.handshake().await?;
        Ok(connection)
    }

    pub fn is_broken(&self) -> bool {
        self.broken
    }

    async fn handshake(&mut self) -> Result<()> {
        // Copied out first: `command` needs `&mut self`, so the arguments
        // cannot borrow from `self.config`.
        let username = self.config.username.clone();
        let password = self.config.password.clone();

        if !password.is_empty() {
            // Redis 6 takes `AUTH user password`; earlier servers only know
            // `AUTH password`, which is still what an empty username means.
            let reply = if username.is_empty() {
                self.command(&[b"AUTH", password.as_bytes()]).await?
            } else {
                self.command(&[b"AUTH", username.as_bytes(), password.as_bytes()]).await?
            };

            if let Value::Error(message) = reply {
                // Deliberately does not echo the password back in the error.
                return Err(Error::msg(format!(
                    "Redis rejected authentication for {}: {message}",
                    self.config.redacted_url()
                )));
            }
        }

        if self.config.database != 0 {
            let db = self.config.database.to_string();
            self.command(&[b"SELECT", db.as_bytes()]).await?.into_result().map_err(|e| {
                Error::msg(format!("cannot SELECT database {}: {e}", self.config.database))
            })?;
        }

        Ok(())
    }

    /// Send a command and read exactly one reply.
    ///
    /// An error *reply* comes back as [`Value::Error`]; only a transport
    /// failure is an `Err`, because a `WRONGTYPE` says nothing about whether
    /// the socket is still usable.
    pub async fn command(&mut self, args: &[&[u8]]) -> Result<Value> {
        let request = resp::encode_command(args);

        let write = self.stream.write_all(&request);
        match tokio::time::timeout(self.config.command_timeout, write).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                self.broken = true;
                return Err(Error::msg(format!("cannot write to Redis: {e}")));
            }
            Err(_) => {
                self.broken = true;
                return Err(Error::msg("timed out sending a command to Redis"));
            }
        }

        self.read_reply().await
    }

    async fn read_reply(&mut self) -> Result<Value> {
        loop {
            // Try what is already buffered first: a pipelined reply may have
            // arrived with the previous one, costing no syscall at all.
            match resp::decode(&self.buffer[self.consumed..]) {
                Ok(Some((value, used))) => {
                    self.consumed += used;
                    if self.consumed == self.buffer.len() {
                        self.buffer.clear();
                        self.consumed = 0;
                    }
                    return Ok(value);
                }
                Ok(None) => {}
                Err(e) => {
                    // The stream no longer parses; the connection is finished.
                    self.broken = true;
                    return Err(e);
                }
            }

            // Reclaim the front of the buffer before growing it, so a long-lived
            // connection does not accumulate consumed bytes.
            if self.consumed > 0 {
                self.buffer.drain(..self.consumed);
                self.consumed = 0;
            }

            let start = self.buffer.len();
            self.buffer.resize(start + READ_CHUNK, 0);

            let read = self.stream.read(&mut self.buffer[start..]);
            let count = match tokio::time::timeout(self.config.command_timeout, read).await {
                Ok(Ok(count)) => count,
                Ok(Err(e)) => {
                    self.buffer.truncate(start);
                    self.broken = true;
                    return Err(Error::msg(format!("cannot read from Redis: {e}")));
                }
                Err(_) => {
                    self.buffer.truncate(start);
                    self.broken = true;
                    return Err(Error::msg("timed out waiting for a reply from Redis"));
                }
            };

            self.buffer.truncate(start + count);

            if count == 0 {
                self.broken = true;
                return Err(Error::msg(format!(
                    "Redis at {} closed the connection mid-reply",
                    self.config.redacted_url()
                )));
            }
        }
    }

    /// Send `QUIT` and drop the socket, ignoring anything that goes wrong: the
    /// connection is being discarded either way.
    pub async fn close(mut self) {
        let _ = self.stream.write_all(&resp::encode_command(&[b"QUIT"])).await;
        let _ = self.stream.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;

    /// A one-shot fake server that replies with canned bytes, so the framing
    /// logic can be tested without a Redis installation. Each test binds port
    /// zero and is handed a real free port, so tests never collide.
    async fn fake_server(script: Vec<Vec<u8>>) -> (RedisConfig, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("a client");
            let mut received = Vec::new();

            for reply in script {
                let mut chunk = vec![0u8; 1024];
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(count) => received.extend_from_slice(&chunk[..count]),
                }
                if socket.write_all(&reply).await.is_err() {
                    break;
                }
            }
            received
        });

        let config = RedisConfig {
            port,
            connect_timeout: Duration::from_secs(2),
            command_timeout: Duration::from_secs(2),
            ..RedisConfig::default()
        };
        (config, handle)
    }

    #[tokio::test]
    async fn a_command_is_written_as_resp_and_its_reply_decoded() {
        let (config, server) = fake_server(vec![b"+PONG\r\n".to_vec()]).await;
        let mut connection = Connection::connect(&config).await.unwrap();

        assert_eq!(connection.command(&[b"PING"]).await.unwrap(), Value::Simple("PONG".into()));

        drop(connection);
        assert_eq!(server.await.unwrap(), b"*1\r\n$4\r\nPING\r\n".to_vec());
    }

    #[tokio::test]
    async fn the_handshake_sends_auth_and_select_before_anything_else() {
        let (mut config, server) =
            fake_server(vec![b"+OK\r\n".to_vec(), b"+OK\r\n".to_vec(), b":1\r\n".to_vec()]).await;
        config.password = "hunter2".into();
        config.database = 4;

        let mut connection = Connection::connect(&config).await.unwrap();
        connection.command(&[b"EXISTS", b"k"]).await.unwrap();
        drop(connection);

        let sent = server.await.unwrap();
        let expected = [
            resp::encode_command(&[b"AUTH", b"hunter2"]),
            resp::encode_command(&[b"SELECT", b"4"]),
            resp::encode_command(&[b"EXISTS", b"k"]),
        ]
        .concat();
        assert_eq!(sent, expected);
    }

    #[tokio::test]
    async fn a_rejected_password_is_reported_without_echoing_it() {
        let (mut config, _server) = fake_server(vec![b"-WRONGPASS invalid password\r\n".to_vec()]).await;
        config.password = "hunter2".into();

        let error = match Connection::connect(&config).await {
            Ok(_) => panic!("connecting should have failed"),
            Err(e) => e.to_string(),
        };

        assert!(error.contains("rejected authentication"), "got: {error}");
        assert!(!error.contains("hunter2"), "the error must not leak the password");
    }

    #[tokio::test]
    async fn a_reply_split_across_packets_is_reassembled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut chunk = vec![0u8; 1024];
            let _ = socket.read(&mut chunk).await;

            // Deliberately dribbled out: length header, then payload, then CRLF.
            for piece in [&b"$11\r\n"[..], &b"hello "[..], &b"world"[..], &b"\r\n"[..]] {
                socket.write_all(piece).await.unwrap();
                socket.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        let config = RedisConfig { port, ..RedisConfig::default() };
        let mut connection = Connection::connect(&config).await.unwrap();

        assert_eq!(
            connection.command(&[b"GET", b"greeting"]).await.unwrap(),
            Value::Bulk(b"hello world".to_vec())
        );
    }

    #[tokio::test]
    async fn a_server_that_hangs_up_mid_reply_marks_the_connection_broken() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut chunk = vec![0u8; 1024];
            let _ = socket.read(&mut chunk).await;
            // A length header promising eleven bytes that never come.
            socket.write_all(b"$11\r\n").await.unwrap();
            socket.shutdown().await.unwrap();
        });

        let config = RedisConfig { port, ..RedisConfig::default() };
        let mut connection = Connection::connect(&config).await.unwrap();

        let error = connection.command(&[b"GET", b"k"]).await.unwrap_err();
        assert!(error.to_string().contains("closed the connection"), "got: {error}");
        assert!(connection.is_broken(), "a truncated reply must never be reused");
    }

    #[tokio::test]
    async fn something_that_is_not_redis_is_reported_as_a_protocol_error() {
        let (config, _server) = fake_server(vec![b"HTTP/1.1 400 Bad Request\r\n".to_vec()]).await;
        let mut connection = Connection::connect(&config).await.unwrap();

        assert!(connection.command(&[b"PING"]).await.is_err());
        assert!(connection.is_broken());
    }

    #[tokio::test]
    async fn connecting_to_a_closed_port_explains_itself() {
        let config = RedisConfig { port: 1, connect_timeout: Duration::from_secs(2), ..RedisConfig::default() };
        let error = match Connection::connect(&config).await {
            Ok(_) => panic!("connecting should have failed"),
            Err(e) => e.to_string(),
        };

        assert!(error.contains("cannot connect to redis://"), "got: {error}");
    }
}
