//! A single MySQL connection.

use super::auth::{self, FastAuth};
use super::protocol::{self, Buffer, Column, OkPacket, Packet, ServerError};
use super::types;
use crate::config::DatabaseConfig;
use crate::driver::{BoxFuture, Driver, DriverConnection, QueryResult};
use crate::row::{Columns, Row};
use crate::value::Value;
use rustlavel_core::events::Event;
use rustlavel_core::{Error, Result};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpStream;

/// MySQL's default port, which [`DatabaseConfig::from_url`] applies to a
/// `mysql://` URL that names no port.
pub const DEFAULT_PORT: u16 = 3306;

/// The driver name a [`DatabaseConfig`] carries when it points at MySQL.
pub const DRIVER_NAME: &str = "mysql";

pub struct MySqlConnection {
    stream: crate::tls::DbStream,
    /// Bytes read from the socket but not yet consumed as a packet.
    buffer: Vec<u8>,
    config: DatabaseConfig,
    connection_id: u32,
    server_version: String,
    /// The capabilities both sides agreed on, which decide the shape of every
    /// packet from here on.
    capabilities: u32,
    status: u16,
    /// The sequence id the next packet we send must carry. It restarts at zero
    /// for each command, and the server rejects a packet that arrives with the
    /// wrong one — which is how a desynchronised connection is caught early.
    sequence: u8,
    /// Set when the connection is known to be unusable, so the pool discards it.
    broken: bool,
}

impl MySqlConnection {
    /// Open a connection and complete the handshake.
    pub async fn connect(config: &DatabaseConfig) -> Result<MySqlConnection> {
        let address = format!("{}:{}", config.host, config.port);

        let stream = tokio::time::timeout(config.connect_timeout, TcpStream::connect(&address))
            .await
            .map_err(|_| {
                // 2003 is the code the MySQL client prints for this, so it is
                // the string people will already have searched for.
                Error::msg(format!(
                    "MySQL error 2003: timed out connecting to {address}. Is MySQL running and \
                     reachable?"
                ))
            })?
            .map_err(|e| {
                Error::msg(format!(
                    "MySQL error 2003: cannot connect to {address}: {e}\n  \
                     Check that the server is running and that DATABASE_URL points at it ({}).",
                    config.redacted_url()
                ))
            })?;

        let _ = stream.set_nodelay(true);

        let mut connection = MySqlConnection {
            stream: crate::tls::DbStream::Plain(stream),
            buffer: Vec::with_capacity(8 * 1024),
            config: config.clone(),
            connection_id: 0,
            server_version: String::new(),
            capabilities: 0,
            status: 0,
            sequence: 0,
            broken: false,
        };

        connection.handshake().await?;
        Ok(connection)
    }

    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// True while a transaction is open, so the pool never hands back a
    /// connection that would leak one.
    ///
    /// Read from the server's own status flag rather than from tracking
    /// `begin`/`commit`, which stays right even when a statement commits
    /// implicitly — DDL in MySQL always does.
    pub fn in_transaction(&self) -> bool {
        self.status & protocol::SERVER_STATUS_IN_TRANS != 0
    }

    /// The server's version string, as it introduced itself.
    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    /// The connection id, which is what `kill <id>` and the slow query log use.
    pub fn connection_id(&self) -> u32 {
        self.connection_id
    }

    /// The capabilities the handshake settled on, which decide what every
    /// packet after it looks like. Useful when a packet does not parse.
    pub fn capabilities(&self) -> u32 {
        self.capabilities
    }

    /// The handshake: read the server's greeting, answer it, and follow the
    /// authentication plugin wherever it goes.
    /// Whether this connection is encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.stream.is_encrypted()
    }

    /// Ask MySQL to encrypt the connection, and return the capabilities to
    /// carry forward.
    ///
    /// MySQL has no separate "please encrypt" message the way PostgreSQL does.
    /// The request *is* the first 32 bytes of the handshake response with
    /// `CLIENT_SSL` set: the server reads that much, sees the flag, and expects
    /// a TLS handshake next instead of the rest of the packet. The credentials
    /// then go inside the tunnel.
    ///
    /// The packet sequence number keeps counting across the boundary — it is
    /// not reset by the upgrade — which [`Self::write_packet`] already does,
    /// and getting it wrong makes the server drop the connection without
    /// saying why.
    async fn negotiate_tls(&mut self, capabilities: u32, server: u32) -> Result<u32> {
        let mode = self.config.tls_mode;
        if !mode.wants_tls() {
            return Ok(capabilities);
        }

        if server & protocol::CLIENT_SSL == 0 {
            if mode.demands_tls() {
                self.broken = true;
                return Err(Error::msg(format!(
                    "sslmode is `{mode}` but this MySQL does not offer TLS: it did not advertise                      CLIENT_SSL in its handshake. The server was built or configured without a                      certificate — set `ssl_cert` and `ssl_key` on it, or set sslmode=prefer to                      accept a connection in clear text."
                )));
            }
            // Nothing to ask for, so the connection stays as it is.
            return Ok(capabilities);
        }

        let mut request = Buffer::new();
        request.ssl_request(capabilities);
        self.write_packet(request).await?;

        let plain = self.stream.take_plain()?;
        let encrypted = crate::tls::upgrade(plain, &self.config.host, &self.config).await?;
        self.stream = crate::tls::DbStream::Tls(Box::new(encrypted));

        Ok(capabilities | protocol::CLIENT_SSL)
    }

    async fn handshake(&mut self) -> Result<()> {
        let greeting = self.read_packet().await?;
        if protocol::is_err(&greeting) {
            // A server that is out of connections, or that has banned this
            // host, refuses before it ever says hello.
            self.broken = true;
            return Err(server_error(protocol::parse_err(&greeting)?, &self.config, None));
        }

        let handshake = protocol::parse_handshake(&greeting)?;
        self.connection_id = handshake.connection_id;
        self.server_version = handshake.server_version.clone();

        let mut capabilities = protocol::CLIENT_CAPABILITIES & handshake.capabilities;
        // The plugin flags are ours to assert: a server that does not advertise
        // them still has to be told which plugin we answered with.
        capabilities |= protocol::CLIENT_PROTOCOL_41 | protocol::CLIENT_SECURE_CONNECTION;
        if !self.config.database.is_empty() {
            capabilities |= protocol::CLIENT_CONNECT_WITH_DB;
        }
        // Before the credentials, and after the capabilities are settled: the
        // server has to be told we want TLS in the same field that tells it
        // everything else about the connection.
        self.capabilities = self.negotiate_tls(capabilities, handshake.capabilities).await?;
        let capabilities = self.capabilities;

        // Default to the SHA-1 plugin only when the server named nothing, which
        // is what a pre-4.1 greeting looks like.
        let mut plugin = if handshake.auth_plugin.is_empty() {
            auth::MYSQL_NATIVE_PASSWORD.to_string()
        } else {
            handshake.auth_plugin.clone()
        };

        if !auth::is_supported(&plugin) {
            self.broken = true;
            return Err(auth::insecure_plugin_error(&plugin));
        }

        let response = auth::respond(&plugin, &self.config.password, &handshake.scramble)?;

        let mut reply = Buffer::new();
        reply.handshake_response(
            capabilities,
            &self.config.user,
            &response,
            Some(&self.config.database),
            &plugin,
            &[
                ("_client_name", "rustlavel"),
                ("program_name", self.config.application_name.as_str()),
            ],
        );
        self.write_packet(reply).await?;

        // Whatever the plugin, the exchange ends at an OK or an ERR; everything
        // between is the plugin negotiating.
        loop {
            let payload = self.read_packet().await?;

            match Packet::parse(&payload)? {
                Packet::Ok(ok) => {
                    self.status = ok.status;
                    return Ok(());
                }
                Packet::Err(error) => {
                    self.broken = true;
                    return Err(authentication_error(error, &self.config));
                }
                Packet::AuthSwitch { plugin: wanted, data } => {
                    if !auth::is_supported(&wanted) {
                        self.broken = true;
                        return Err(auth::insecure_plugin_error(&wanted));
                    }
                    // The switch carries a fresh scramble; the old one belongs
                    // to a plugin we are no longer speaking.
                    plugin = wanted;
                    let response = auth::respond(&plugin, &self.config.password, &data)?;
                    let mut reply = Buffer::new();
                    reply.auth_response(&response);
                    self.write_packet(reply).await?;
                }
                Packet::AuthMoreData(data) if plugin == auth::CACHING_SHA2_PASSWORD => {
                    match auth::fast_auth_status(&data)? {
                        // Nothing to send: the OK packet is already on its way.
                        FastAuth::Succeeded => continue,
                        // The password itself, and only ever inside the
                        // tunnel. This is what MySQL's own client does, and
                        // the check is on the stream rather than on the
                        // configured mode: `prefer` against a server that
                        // declined leaves the mode saying "tls" and the socket
                        // saying otherwise, and the socket is the one telling
                        // the truth.
                        FastAuth::FullAuthRequired if self.stream.is_encrypted() => {
                            let mut reply = Buffer::new();
                            reply.auth_response(&auth::cleartext_password(
                                &self.config.password,
                            ));
                            self.write_packet(reply).await?;
                        }
                        FastAuth::FullAuthRequired => {
                            self.broken = true;
                            return Err(auth::full_auth_error(
                                &self.config.user,
                                &format!("{}:{}", self.config.host, self.config.port),
                            ));
                        }
                    }
                }
                Packet::AuthMoreData(_) | Packet::Eof(_) | Packet::Other(_) => {
                    self.broken = true;
                    return Err(Error::Protocol(format!(
                        "unexpected packet during {plugin} authentication"
                    )));
                }
            }
        }
    }

    /// Run a statement with no parameters, as text.
    ///
    /// Used for DDL and for transaction control — the statements MySQL will not
    /// let a client prepare.
    pub async fn simple_query(&mut self, sql: &str) -> Result<QueryResult> {
        let started = Instant::now();

        let mut command = Buffer::new();
        command.com_query(sql);
        self.sequence = 0;
        let result = match self.write_packet(command).await {
            Ok(()) => self.read_result_set(sql, false).await,
            Err(e) => Err(e),
        };

        self.record(sql, &[], started, &result);
        result
    }

    /// Run a parameterised statement as a prepared statement.
    ///
    /// Prepare, execute, close. The values travel in their own typed section of
    /// the execute packet and never enter the statement text, which is what
    /// makes SQL injection structurally impossible rather than a matter of
    /// remembering to escape. Statements with no parameters go the same way, so
    /// there is one code path to be right about rather than two.
    pub async fn query(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult> {
        let started = Instant::now();
        let result = self.prepared(sql, params).await;
        self.record(sql, params, started, &result);
        result
    }

    async fn prepared(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult> {
        let statement = self.prepare(sql).await?;

        if statement.params as usize != params.len() {
            self.close_statement(statement.statement_id).await?;
            return Err(Error::msg(format!(
                "the statement takes {} parameter(s) but {} were bound.\n  SQL: {sql}",
                statement.params,
                params.len()
            )));
        }

        let mut command = Buffer::new();
        command.com_stmt_execute(statement.statement_id, params);
        self.sequence = 0;
        let result = match self.write_packet(command).await {
            Ok(()) => self.read_result_set(sql, true).await,
            Err(e) => Err(e),
        };

        // Closed whether or not the execute worked: the handle is the server's
        // memory, and an error is no reason to leak it for the session's life.
        self.close_statement(statement.statement_id).await?;
        result
    }

    /// Ask the server to parse a statement, and read back its metadata.
    async fn prepare(&mut self, sql: &str) -> Result<protocol::PrepareOk> {
        let mut command = Buffer::new();
        command.com_stmt_prepare(sql);
        self.sequence = 0;
        self.write_packet(command).await?;

        let payload = self.read_packet().await?;
        if protocol::is_err(&payload) {
            return Err(server_error(protocol::parse_err(&payload)?, &self.config, Some(sql)));
        }

        let prepared = protocol::parse_prepare_ok(&payload)?;

        // The parameter and column metadata follow, each section closed by an
        // EOF. The driver does not need them — the execute response describes
        // the columns again — but they must be consumed or the next read would
        // pick one of them up as a result.
        if prepared.params > 0 {
            self.skip_metadata(prepared.params as usize).await?;
        }
        if prepared.columns > 0 {
            self.skip_metadata(prepared.columns as usize).await?;
        }

        Ok(prepared)
    }

    async fn skip_metadata(&mut self, count: usize) -> Result<()> {
        for _ in 0..count {
            self.read_packet().await?;
        }
        // The trailing EOF, present because the driver does not negotiate
        // CLIENT_DEPRECATE_EOF.
        let payload = self.read_packet().await?;
        if !protocol::is_eof(&payload) {
            return Err(Error::Protocol(
                "expected an EOF after a metadata block from the server".into(),
            ));
        }
        Ok(())
    }

    async fn close_statement(&mut self, statement_id: u32) -> Result<()> {
        let mut command = Buffer::new();
        command.com_stmt_close(statement_id);
        self.sequence = 0;
        // COM_STMT_CLOSE is the one command the server never answers.
        self.write_packet(command).await
    }

    /// Read one command's response: an OK, an error, or a whole result set.
    ///
    /// `binary` selects the row format — text for `COM_QUERY`, binary for
    /// `COM_STMT_EXECUTE` — which is the only thing that differs between them.
    async fn read_result_set(&mut self, sql: &str, binary: bool) -> Result<QueryResult> {
        let mut result = QueryResult::default();
        let mut first = true;

        loop {
            let payload = self.read_packet().await?;

            if protocol::is_err(&payload) {
                return Err(server_error(protocol::parse_err(&payload)?, &self.config, Some(sql)));
            }

            if payload.first() == Some(&0x00) {
                // No result set: an OK packet, carrying the row count and any
                // generated key.
                let ok = protocol::parse_ok(&payload)?;
                self.status = ok.status;
                if first {
                    apply_ok(&mut result, &ok);
                }
                if ok.status & protocol::SERVER_MORE_RESULTS_EXISTS != 0 {
                    first = false;
                    continue;
                }
                return Ok(result);
            }

            let count = protocol::Reader::new(&payload).lenenc_int()? as usize;
            let columns = self.read_columns(count).await?;
            let more = self.read_rows(&columns, binary, first.then_some(&mut result)).await?;

            if !more {
                return Ok(result);
            }
            first = false;
        }
    }

    /// Read `count` column definitions and the EOF that closes them.
    async fn read_columns(&mut self, count: usize) -> Result<Vec<Column>> {
        let mut columns = Vec::with_capacity(count);
        for _ in 0..count {
            let payload = self.read_packet().await?;
            columns.push(protocol::parse_column(&payload)?);
        }

        let payload = self.read_packet().await?;
        if !protocol::is_eof(&payload) {
            return Err(Error::Protocol(
                "expected an EOF after the column definitions".into(),
            ));
        }
        Ok(columns)
    }

    /// Read rows until the EOF that closes them, returning whether another
    /// result set follows.
    ///
    /// `into` is `None` for the second and later result sets of a multi-result
    /// response: they are drained so the connection stays in step, but only the
    /// first one's rows are handed back.
    async fn read_rows(
        &mut self,
        columns: &[Column],
        binary: bool,
        into: Option<&mut QueryResult>,
    ) -> Result<bool> {
        let names: Columns = Arc::new(columns.iter().map(|c| c.name.clone()).collect());
        let mut rows = Vec::new();

        let status = loop {
            let payload = self.read_packet().await?;

            if protocol::is_err(&payload) {
                return Err(server_error(protocol::parse_err(&payload)?, &self.config, None));
            }
            if protocol::is_eof(&payload) {
                let eof = protocol::parse_eof(&payload)?;
                self.status = eof.status;
                break eof.status;
            }

            let values = if binary {
                decode_binary_row(&payload, columns)?
            } else {
                protocol::parse_text_row(&payload, columns.len())?
                    .iter()
                    .zip(columns)
                    .map(|(raw, column)| types::decode_text(column, raw.as_deref()))
                    .collect()
            };
            rows.push(Row::new(Arc::clone(&names), values));
        };

        if let Some(result) = into {
            // A select reports the rows it returned; MySQL's OK-less result set
            // has no affected count of its own.
            result.affected = rows.len() as u64;
            result.rows = rows;
        }

        Ok(status & protocol::SERVER_MORE_RESULTS_EXISTS != 0)
    }

    /// Ask the server whether it is still there.
    pub async fn ping(&mut self) -> Result<()> {
        let mut command = Buffer::new();
        command.com_ping();
        self.sequence = 0;
        self.write_packet(command).await?;

        let payload = self.read_packet().await?;
        match Packet::parse(&payload)? {
            Packet::Ok(ok) => {
                self.status = ok.status;
                Ok(())
            }
            Packet::Err(error) => Err(server_error(error, &self.config, None)),
            _ => Err(Error::Protocol("the server answered a ping with something else".into())),
        }
    }

    /// Publish the query on the event bus for Telescope and slow-query logging.
    fn record(&self, sql: &str, params: &[Value], started: Instant, result: &Result<QueryResult>) {
        let elapsed = started.elapsed();

        if rustlavel_core::events::has_subscribers() {
            // The same switch as the PostgreSQL driver's, deliberately: it is
            // process-wide, and one application should not have to turn binding
            // capture off once per database it talks to.
            let bindings = if crate::postgres::connection::log_bindings() {
                params.iter().map(Value::to_display).collect::<Vec<_>>().join(", ")
            } else {
                format!("{} value(s) hidden", params.len())
            };
            Event::new("db.query")
                .with("sql", sql)
                .with("bindings", bindings)
                .with("rows", result.as_ref().map(|r| r.rows.len()).unwrap_or(0))
                .with("ok", result.is_ok())
                .took(elapsed)
                .dispatch();
        }

        rustlavel_core::debug!("db: {sql} ({:.1}ms)", elapsed.as_secs_f64() * 1000.0);
    }

    /// Frame a payload with the next sequence id and send it.
    async fn write_packet(&mut self, buffer: Buffer) -> Result<()> {
        let (bytes, next) = protocol::frame(&buffer.into_bytes(), self.sequence);
        self.sequence = next;

        if let Err(e) = self.stream.write_all(&bytes).await {
            self.broken = true;
            return Err(Error::Io(e));
        }
        if let Err(e) = self.stream.flush().await {
            self.broken = true;
            return Err(Error::Io(e));
        }
        Ok(())
    }

    /// Read exactly one logical packet, rejoining a payload that was split
    /// across frames.
    async fn read_packet(&mut self) -> Result<Vec<u8>> {
        let mut payload = Vec::new();

        loop {
            self.fill_to(4).await?;
            let length =
                u32::from_le_bytes([self.buffer[0], self.buffer[1], self.buffer[2], 0]) as usize;
            let sequence = self.buffer[3];

            self.fill_to(4 + length).await?;
            payload.extend_from_slice(&self.buffer[4..4 + length]);
            self.buffer.drain(..4 + length);
            self.sequence = sequence.wrapping_add(1);

            // Only a maximum-length frame can have a continuation; anything
            // shorter is the end of the payload.
            if length < protocol::MAX_PAYLOAD {
                return Ok(payload);
            }
        }
    }

    /// Read from the socket until the buffer holds at least `wanted` bytes.
    async fn fill_to(&mut self, wanted: usize) -> Result<()> {
        while self.buffer.len() < wanted {
            let mut chunk = [0u8; 8192];
            let read = match self.stream.read(&mut chunk).await {
                Ok(read) => read,
                Err(e) => {
                    self.broken = true;
                    return Err(Error::Io(e));
                }
            };
            if read == 0 {
                self.broken = true;
                return Err(Error::Protocol(
                    "the database closed the connection unexpectedly".into(),
                ));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
        Ok(())
    }

    /// Say goodbye politely, then hang up.
    pub async fn close(mut self) {
        let mut command = Buffer::new();
        command.com_quit();
        self.sequence = 0;
        let _ = self.write_packet(command).await;
        let _ = self.stream.shutdown().await;
    }
}

/// Turn a server error into one that says what to do about it.
fn server_error(error: ServerError, config: &DatabaseConfig, sql: Option<&str>) -> Error {
    let advice = match error.code {
        1049 => Some(format!(
            "Database `{}` does not exist. Create it, or point DATABASE_URL at one that does.",
            config.database
        )),
        1044 => Some(format!(
            "User `{}` has no rights on `{}`. Grant them, or connect as a user who has.",
            config.user, config.database
        )),
        1146 => Some("The table is missing. Have the migrations been run?".to_string()),
        _ => None,
    };

    let base = error.into_error(sql);
    match advice {
        Some(advice) => Error::msg(format!("{base}\n  {advice}")),
        None => base,
    }
}

/// Turn an authentication failure into something the developer can act on.
fn authentication_error(error: ServerError, config: &DatabaseConfig) -> Error {
    let advice = match error.code {
        1045 => Some(format!(
            "The password for `{}` was rejected. Check DATABASE_URL in your .env.",
            config.user
        )),
        1049 => Some(format!(
            "Database `{}` does not exist. Create it, or point DATABASE_URL at one that does.",
            config.database
        )),
        1130 | 1698 => Some(format!(
            "The server will not let `{}` in from this host. Check the account's host pattern \
             and its authentication plugin.",
            config.user
        )),
        1040 | 1203 => Some(
            "The server is out of connections. Lower max_connections in DATABASE_URL, or raise \
             the server's."
                .to_string(),
        ),
        _ => None,
    };

    let base = error.into_error(None);
    match advice {
        Some(advice) => Error::msg(format!("{base}\n  {advice}")),
        None => base,
    }
}

/// Split a binary-protocol row into its values.
///
/// The row opens with a `0x00` header and a NULL bitmap. The bitmap is offset
/// by two bits — a quirk left over from the header having once been two bytes —
/// so column `i` is at bit `i + 2`, and getting that wrong shifts every NULL in
/// the row by one column.
fn decode_binary_row(payload: &[u8], columns: &[Column]) -> Result<Vec<Value>> {
    let mut reader = protocol::Reader::new(payload);
    reader.skip(1)?; // the 0x00 header

    let bitmap = reader.take((columns.len() + 2).div_ceil(8))?;
    let mut values = Vec::with_capacity(columns.len());

    for (index, column) in columns.iter().enumerate() {
        let bit = index + 2;
        if bitmap[bit / 8] & (1 << (bit % 8)) != 0 {
            values.push(Value::Null);
        } else {
            values.push(types::decode_binary(column, &mut reader)?);
        }
    }

    Ok(values)
}

fn apply_ok(result: &mut QueryResult, ok: &OkPacket) {
    result.affected = ok.affected_rows;
    // Zero means "this statement generated no key", not "the key was zero":
    // an `auto_increment` column never issues zero.
    result.last_insert_id = (ok.last_insert_id != 0).then_some(ok.last_insert_id as i64);
}

/// Opens MySQL connections.
pub struct MySqlDriver {
    config: DatabaseConfig,
    dialect: Arc<dyn crate::dialect::Dialect>,
}

impl MySqlDriver {
    pub fn new(mut config: DatabaseConfig) -> Self {
        // A config assembled by hand keeps `DatabaseConfig::default`'s driver
        // name, and that name is what `redacted_url` prints as the scheme — so
        // it is corrected here rather than leaving `describe()` claiming to be
        // a PostgreSQL connection.
        config.driver = DRIVER_NAME.into();
        MySqlDriver { config, dialect: Arc::new(crate::dialect::MySql) }
    }

    /// Build a driver from `mysql://user:password@host:port/database`.
    pub fn from_url(url: &str) -> Result<Self> {
        let config = DatabaseConfig::from_url(url)?;

        // A `postgres://` URL handed to the MySQL driver would otherwise fail
        // much later, as a handshake that makes no sense.
        if config.driver != DRIVER_NAME {
            return Err(Error::msg(format!(
                "`{url}` is a {} URL, not a MySQL one. Expected \
                 mysql://user:password@host:port/database",
                config.driver
            )));
        }

        Ok(MySqlDriver::new(config))
    }

    pub fn config(&self) -> &DatabaseConfig {
        &self.config
    }
}

impl Driver for MySqlDriver {
    fn generation(&self) -> u64 {
        self.config.generation()
    }

    fn dialect(&self) -> Arc<dyn crate::dialect::Dialect> {
        Arc::clone(&self.dialect)
    }

    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn DriverConnection>>> {
        Box::pin(async move {
            let connection = MySqlConnection::connect(&self.config.resolved()).await?;
            Ok(Box::new(connection) as Box<dyn DriverConnection>)
        })
    }

    fn describe(&self) -> String {
        self.config.redacted_url()
    }

    fn max_connections(&self) -> usize {
        self.config.max_connections
    }
}

impl DriverConnection for MySqlConnection {
    fn query<'a>(
        &'a mut self,
        sql: &'a str,
        params: &'a [Value],
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(MySqlConnection::query(self, sql, params))
    }

    fn simple_query<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(MySqlConnection::simple_query(self, sql))
    }

    fn is_broken(&self) -> bool {
        MySqlConnection::is_broken(self)
    }

    fn in_transaction(&self) -> bool {
        MySqlConnection::in_transaction(self)
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move { MySqlConnection::close(*self).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mysql::protocol::{CHARSET_BINARY, CHARSET_UTF8MB4};

    fn column(name: &str, column_type: u8) -> Column {
        Column {
            name: name.into(),
            column_type,
            charset: CHARSET_UTF8MB4 as u16,
            ..Column::default()
        }
    }

    #[test]
    fn a_generated_key_of_zero_means_no_key_was_generated() {
        let mut result = QueryResult::default();
        apply_ok(&mut result, &OkPacket { affected_rows: 1, last_insert_id: 0, ..OkPacket::default() });
        assert_eq!(result.last_insert_id, None);
        assert_eq!(result.affected, 1);

        apply_ok(&mut result, &OkPacket { affected_rows: 1, last_insert_id: 42, ..OkPacket::default() });
        assert_eq!(result.last_insert_id, Some(42));
    }

    #[test]
    fn a_binary_row_reads_its_nulls_from_a_bitmap_offset_by_two_bits() {
        let columns = vec![column("id", types::LONGLONG), column("name", types::VAR_STRING)];

        // Header, bitmap marking column 1 (bit 3) NULL, then only column 0.
        let mut payload = vec![0x00, 0b0000_1000];
        payload.extend_from_slice(&7i64.to_le_bytes());

        let values = decode_binary_row(&payload, &columns).unwrap();
        assert_eq!(values, vec![Value::Int(7), Value::Null]);
    }

    #[test]
    fn a_binary_row_with_no_nulls_decodes_every_column() {
        let columns = vec![
            column("id", types::LONGLONG),
            column("name", types::VAR_STRING),
            Column { charset: CHARSET_BINARY, ..column("blob", types::BLOB) },
        ];

        let mut payload = vec![0x00, 0x00];
        payload.extend_from_slice(&7i64.to_le_bytes());
        payload.extend_from_slice(b"\x03ada");
        payload.extend_from_slice(b"\x02\xde\xad");

        let values = decode_binary_row(&payload, &columns).unwrap();
        assert_eq!(
            values,
            vec![Value::Int(7), Value::Text("ada".into()), Value::Bytes(vec![0xDE, 0xAD])]
        );
    }

    #[test]
    fn a_wide_row_needs_more_than_one_bitmap_byte() {
        // Seven columns plus the two offset bits is nine, so the bitmap is two
        // bytes; a one-byte bitmap would silently truncate.
        let columns: Vec<Column> =
            (0..7).map(|i| column(&format!("c{i}"), types::TINY)).collect();

        // Mark the last column (bit 8) NULL.
        let mut payload = vec![0x00, 0b0000_0000, 0b0000_0001];
        payload.extend_from_slice(&[1, 2, 3, 4, 5, 6]);

        let values = decode_binary_row(&payload, &columns).unwrap();
        assert_eq!(values.len(), 7);
        assert_eq!(values[6], Value::Null);
        assert_eq!(values[0], Value::Int(1));
    }

    #[test]
    fn a_truncated_row_is_an_error_rather_than_a_panic() {
        let columns = vec![column("id", types::LONGLONG)];
        assert!(decode_binary_row(&[0x00], &columns).is_err());
        assert!(decode_binary_row(&[0x00, 0x00, 1, 2], &columns).is_err());
    }

    #[test]
    fn a_url_defaults_to_mysqls_port() {
        let driver = MySqlDriver::from_url("mysql://localhost/blog").unwrap();

        assert_eq!(driver.config().host, "localhost");
        assert_eq!(driver.config().port, DEFAULT_PORT);
        assert_eq!(driver.config().database, "blog");
        assert_eq!(driver.config().driver, DRIVER_NAME);
    }

    #[test]
    fn a_url_keeps_everything_it_states() {
        let driver =
            MySqlDriver::from_url("mysql://ada:hunter2@db.internal:3307/blog").unwrap();
        let config = driver.config();

        assert_eq!(config.user, "ada");
        assert_eq!(config.password, "hunter2");
        assert_eq!(config.host, "db.internal");
        assert_eq!(config.port, 3307);
        assert_eq!(config.database, "blog");
    }

    #[test]
    fn mariadb_urls_are_accepted_too() {
        // MariaDB speaks the same wire protocol, so it is the same driver.
        let driver = MySqlDriver::from_url("mariadb://host/blog").unwrap();

        assert_eq!(driver.config().database, "blog");
        assert_eq!(driver.config().port, DEFAULT_PORT);
    }

    #[test]
    fn rejects_a_url_that_points_at_another_database() {
        // Better here than as a handshake that makes no sense later.
        let error = match MySqlDriver::from_url("postgres://host/blog") {
            Err(error) => error.to_string(),
            Ok(driver) => panic!("accepted a {} URL", driver.config().driver),
        };
        assert!(error.contains("not a MySQL one"), "{error}");
    }

    #[test]
    fn a_hand_built_config_still_describes_itself_as_mysql() {
        // `DatabaseConfig::default` says `postgres`, and that name is the
        // scheme in every log line and error message this driver prints.
        let driver = MySqlDriver::new(DatabaseConfig::default());
        assert!(driver.describe().starts_with("mysql://"), "{}", driver.describe());
    }

    #[test]
    fn the_driver_speaks_the_mysql_dialect_and_never_prints_the_password() {
        let driver =
            MySqlDriver::from_url("mysql://ada:hunter2@host/blog?max_connections=4").unwrap();

        assert_eq!(driver.dialect().name(), "mysql");
        assert!(driver.dialect().booleans_are_integers());
        assert_eq!(driver.max_connections(), 4);

        let shown = driver.describe();
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.starts_with("mysql://ada:***@"), "{shown}");
    }

    #[test]
    fn a_wrong_password_explains_where_to_look() {
        let config = DatabaseConfig { user: "ada".into(), ..DatabaseConfig::default() };
        let error = authentication_error(
            ServerError {
                code: 1045,
                sql_state: "28000".into(),
                message: "Access denied for user 'ada'@'localhost'".into(),
            },
            &config,
        )
        .to_string();

        assert!(error.contains("1045"), "{error}");
        assert!(error.contains("28000"), "{error}");
        assert!(error.contains("Access denied"), "{error}");
        assert!(error.contains("DATABASE_URL"), "{error}");
    }

    #[test]
    fn an_unknown_database_names_the_one_that_was_asked_for() {
        let config = DatabaseConfig { database: "blog".into(), ..DatabaseConfig::default() };
        let error = server_error(
            ServerError {
                code: 1049,
                sql_state: "42000".into(),
                message: "Unknown database 'blog'".into(),
            },
            &config,
            None,
        )
        .to_string();

        assert!(error.contains("1049"), "{error}");
        assert!(error.contains("`blog` does not exist"), "{error}");
    }

    #[test]
    fn a_statement_error_carries_the_sql_that_caused_it() {
        let error = server_error(
            ServerError {
                code: 1146,
                sql_state: "42S02".into(),
                message: "Table 'blog.nope' doesn't exist".into(),
            },
            &DatabaseConfig::default(),
            Some("select * from nope"),
        )
        .to_string();

        assert!(error.contains("SQL: select * from nope"), "{error}");
        assert!(error.contains("migrations"), "{error}");
    }

    #[tokio::test]
    async fn connecting_to_a_closed_port_names_the_server_and_the_client_error_code() {
        let config = DatabaseConfig { port: 1, ..DatabaseConfig::default() };
        let error = match MySqlConnection::connect(&config).await {
            Err(error) => error.to_string(),
            Ok(_) => panic!("nothing should be listening on port 1"),
        };

        assert!(error.contains("127.0.0.1:1"), "{error}");
        assert!(error.contains("2003"), "{error}");
    }

    #[tokio::test]
    async fn a_connection_that_never_opened_reports_no_transaction() {
        // Built without a socket so the state machine can be inspected offline.
        let connection = offline(DatabaseConfig::default());

        assert!(!connection.in_transaction());
        assert!(!connection.is_broken());
        assert_eq!(connection.connection_id(), 0);
        assert_eq!(connection.server_version(), "");
    }

    /// A connection whose socket is a closed loopback pair, for testing the
    /// parts that never touch the network.
    fn offline(config: DatabaseConfig) -> MySqlConnection {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let address = listener.local_addr().expect("an address");
        let stream = std::net::TcpStream::connect(address).expect("a loopback connection");
        stream.set_nonblocking(true).expect("non-blocking");

        MySqlConnection {
            stream: crate::tls::DbStream::Plain(
                TcpStream::from_std(stream).expect("a tokio stream"),
            ),
            buffer: Vec::new(),
            config,
            connection_id: 0,
            server_version: String::new(),
            capabilities: protocol::CLIENT_CAPABILITIES,
            status: 0,
            sequence: 0,
            broken: false,
        }
    }
}
