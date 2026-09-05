//! A single SQL Server connection.

use super::auth::{self, Encryption, Negotiated, TdsStream, TlsOptions};
use super::protocol::{
    self, DEFAULT_PACKET_SIZE, EnvChange, HEADER_LEN, Login7, PacketHeader, ServerError,
    Token, TokenStream, packet,
};
use crate::config::DatabaseConfig;
use crate::dialect::Dialect;
use crate::driver::{BoxFuture, Driver, DriverConnection, QueryResult};
use crate::postgres::connection::log_bindings;
use crate::row::{Columns, Row};
use crate::tls::TlsMode;
use crate::value::Value;
use rustlavel_core::events::Event;
use rustlavel_core::{Error, Result};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpStream;

/// Settings that are specific to SQL Server and have no home in
/// [`DatabaseConfig`], which is shared with the other drivers.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqlServerOptions {
    pub encryption: Encryption,
    pub tls: TlsOptions,
}

impl SqlServerOptions {
    /// What the connection string asked for.
    ///
    /// `sslmode` was parsed and validated for `sqlserver://` URLs and then read
    /// by nobody: every connection used `Default`, which is "encrypt, and trust
    /// whatever certificate turns up". Somebody who wrote `sslmode=verify-full`
    /// got no verification at all, and `sslrootcert=` was discarded — a silent
    /// downgrade of exactly the kind the other two drivers are careful to avoid.
    ///
    /// `prefer`, the default, keeps the documented compromise: SQL Server
    /// generates a version 1 self-signed certificate at startup that rustls will
    /// not even parse, so verifying by default would refuse every stock
    /// installation. Asking for verification, however, now gets it.
    ///
    /// One honest imprecision: `verify-ca` is served by the same verifier as
    /// `verify-full`, so it checks the hostname too. That is stricter than the
    /// mode promises rather than weaker, and it is said here rather than left
    /// to be discovered.
    pub fn from_config(config: &DatabaseConfig) -> SqlServerOptions {
        let trust = !matches!(config.tls_mode, TlsMode::VerifyCa | TlsMode::VerifyFull);
        SqlServerOptions {
            encryption: match config.tls_mode {
                TlsMode::Disable => Encryption::Disabled,
                _ => Encryption::Required,
            },
            tls: TlsOptions { trust_server_certificate: trust },
        }
    }
}

pub struct SqlServerConnection {
    stream: TdsStream,
    /// Bytes read from the socket but not yet consumed as a packet.
    buffer: Vec<u8>,
    config: DatabaseConfig,
    /// The largest packet either side may send, as the server settled it.
    packet_size: usize,
    /// The descriptor every request must quote; zero when no transaction is
    /// open, which is also how `in_transaction` is answered.
    transaction: u64,
    /// Set when the connection is known to be unusable, so the pool discards it.
    broken: bool,
}

impl SqlServerConnection {
    /// Open a connection: pre-login, encryption, then login.
    pub async fn connect(config: &DatabaseConfig) -> Result<SqlServerConnection> {
        SqlServerConnection::connect_with(config, SqlServerOptions::from_config(config)).await
    }

    pub async fn connect_with(
        config: &DatabaseConfig,
        options: SqlServerOptions,
    ) -> Result<SqlServerConnection> {
        let address = format!("{}:{}", config.host, config.port);

        let socket = tokio::time::timeout(config.connect_timeout, TcpStream::connect(&address))
            .await
            .map_err(|_| {
                Error::msg(format!(
                    "timed out connecting to {address}. Is SQL Server running, and is its TCP/IP \
                     protocol enabled? It is off by default on Windows."
                ))
            })?
            .map_err(|e| {
                Error::msg(format!(
                    "cannot connect to {}: {e}\n  \
                     Check the host and port in DATABASE_URL; SQL Server listens on 1433 unless \
                     it was configured otherwise.",
                    config.redacted_url()
                ))
            })?;

        let _ = socket.set_nodelay(true);

        let mut connection = SqlServerConnection {
            stream: TdsStream::Plain(socket),
            buffer: Vec::with_capacity(8 * 1024),
            config: config.clone(),
            packet_size: DEFAULT_PACKET_SIZE,
            transaction: 0,
            broken: false,
        };

        let negotiated = connection.prelogin(options).await?;
        connection.login(negotiated).await?;
        Ok(connection)
    }

    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// True while a transaction is open, so the pool never hands back a
    /// connection that would leak one.
    pub fn in_transaction(&self) -> bool {
        self.transaction != 0
    }

    /// Exchange PRELOGIN packets and put TLS up if either side asked for it.
    async fn prelogin(&mut self, options: SqlServerOptions) -> Result<Negotiated> {
        self.write_message(packet::PRE_LOGIN, &protocol::prelogin(options.encryption.as_byte()))
            .await?;

        let response = protocol::parse_prelogin(&self.read_message().await?)?;
        let negotiated = auth::negotiate(options.encryption, response.encryption)?;

        if negotiated != Negotiated::None {
            // The socket is handed to the handshake wrapper whole; nothing can
            // be buffered here, because PRELOGIN is one message and it has
            // already been consumed in full.
            debug_assert!(self.buffer.is_empty());
            let socket = match self.stream.take() {
                TdsStream::Plain(socket) => socket,
                _ => return Err(Error::Protocol("encryption was negotiated twice".into())),
            };
            let tls =
                auth::start_tls(socket, &self.config.host, options.tls, self.packet_size).await?;
            self.stream = TdsStream::Tls(Box::new(tls));
        }

        Ok(negotiated)
    }

    /// Send LOGIN7 and read the response through to its DONE.
    async fn login(&mut self, negotiated: Negotiated) -> Result<()> {
        let password = auth::obfuscate_password(&self.config.password);
        let hostname = hostname();

        let payload = protocol::login7(&Login7 {
            hostname: &hostname,
            username: &self.config.user,
            password: &password,
            application: &self.config.application_name,
            server: &self.config.host,
            library: "rustlavel-db",
            language: "",
            database: &self.config.database,
            packet_size: self.packet_size,
        });

        self.write_message(packet::LOGIN7, &payload).await?;

        // ENCRYPT_OFF means exactly this: the login packet was the only thing
        // encrypted, and the response already comes back in the clear.
        if negotiated == Negotiated::LoginOnly {
            self.stream = self.stream.take().into_plain()?;
        }

        let message = self.read_message().await?;
        let mut stream = TokenStream::new(&message);
        let mut failure: Option<ServerError> = None;
        let mut acknowledged = false;

        while let Some(token) = stream.next_token()? {
            match token {
                Token::LoginAck(_) => acknowledged = true,
                Token::EnvChange(change) => self.apply(change),
                Token::Error(error) => failure = Some(error),
                Token::Info(_) | Token::Done(_) | Token::Ignored(_) => {}
                other => {
                    return Err(Error::Protocol(format!(
                        "unexpected token during login: {other:?}"
                    )));
                }
            }
        }

        if let Some(error) = failure {
            self.broken = true;
            return Err(login_error(error, &self.config));
        }
        if !acknowledged {
            self.broken = true;
            return Err(Error::Protocol(
                "the server ended the login exchange without accepting or refusing it. If it \
                 requires Windows authentication, this driver implements SQL Server \
                 authentication only."
                    .into(),
            ));
        }

        Ok(())
    }

    /// Run a statement with no parameters, as a batch.
    ///
    /// Used for DDL and for transaction control, both of which have to run
    /// outside `sp_executesql` — a `begin transaction` inside a procedure ends
    /// when the procedure does.
    pub async fn simple_query(&mut self, sql: &str) -> Result<QueryResult> {
        let started = Instant::now();
        let payload = protocol::sql_batch(sql, self.transaction);
        self.write_message(packet::SQL_BATCH, &payload).await?;

        let result = self.collect(sql).await;
        self.record(sql, &[], started, &result);
        result
    }

    /// Run a statement through `sp_executesql`.
    ///
    /// Every statement takes this route, parameters or not, because it is the
    /// route where a bound value is a value: the statement text and the
    /// parameter declarations are themselves arguments to a stored procedure,
    /// so nothing a caller binds is ever concatenated into SQL. A value cannot
    /// change the shape of a statement it was never part of.
    pub async fn query(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult> {
        let started = Instant::now();
        let payload = protocol::execute_sql(sql, params, self.transaction);
        self.write_message(packet::RPC, &payload).await?;

        let result = self.collect(sql).await;
        self.record(sql, params, started, &result);
        result
    }

    /// Read the whole response and turn it into rows, a count and an error.
    async fn collect(&mut self, sql: &str) -> Result<QueryResult> {
        let message = self.read_message().await?;
        let mut stream = TokenStream::new(&message);

        let mut columns: Columns = Arc::new(Vec::new());
        let mut result = QueryResult::default();
        let mut failure: Option<ServerError> = None;
        let mut changes = Vec::new();

        while let Some(token) = stream.next_token()? {
            match token {
                Token::ColumnMetadata(described) => {
                    columns = Arc::new(described.iter().map(|c| c.name.clone()).collect());
                }
                Token::Row(values) => result.rows.push(Row::new(Arc::clone(&columns), values)),
                Token::Done(done) => {
                    // Several DONE tokens arrive per call — one per statement
                    // inside the procedure, one for the procedure. Only those
                    // carrying DONE_COUNT have a number worth believing.
                    if done.has_count() {
                        result.affected = done.rows;
                    }
                }
                Token::Error(error) => failure = Some(error),
                // Applied after the loop, because `stream` borrows the message.
                Token::EnvChange(change) => changes.push(change),
                Token::Info(_) | Token::LoginAck(_) | Token::ReturnStatus(_)
                | Token::Ignored(_) => {}
            }
        }

        for change in changes {
            self.apply(change);
        }

        match failure {
            Some(error) => Err(error.into_error(Some(sql))),
            None => {
                result.last_insert_id = generated_key(sql, &result.rows);
                Ok(result)
            }
        }
    }

    fn apply(&mut self, change: EnvChange) {
        match change {
            EnvChange::PacketSize(size) => {
                self.packet_size = size.clamp(HEADER_LEN + 1, 32 * 1024)
            }
            EnvChange::BeginTransaction(descriptor) => self.transaction = descriptor,
            EnvChange::CommitTransaction | EnvChange::RollbackTransaction => self.transaction = 0,
            EnvChange::Database(_) | EnvChange::Other(_) => {}
        }
    }

    /// Publish the query on the event bus for Telescope and slow-query logging.
    fn record(&self, sql: &str, params: &[Value], started: Instant, result: &Result<QueryResult>) {
        let elapsed = started.elapsed();

        if rustlavel_core::events::has_subscribers() {
            let bindings = if log_bindings() {
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

    /// Frame a payload into packets and send it.
    ///
    /// One write per packet, so an encrypted connection puts each packet in its
    /// own TLS record — see [`protocol::split_message`] for why that matters.
    async fn write_message(&mut self, kind: u8, payload: &[u8]) -> Result<()> {
        for packet in protocol::split_message(kind, payload, self.packet_size) {
            if let Err(e) = self.stream.write_all(&packet).await {
                self.broken = true;
                return Err(Error::Io(e));
            }
            if let Err(e) = self.stream.flush().await {
                self.broken = true;
                return Err(Error::Io(e));
            }
        }
        Ok(())
    }

    /// Read packets until one carries the end-of-message bit, returning the
    /// payloads joined back together.
    async fn read_message(&mut self) -> Result<Vec<u8>> {
        let mut payload = Vec::new();

        loop {
            self.fill_to(HEADER_LEN).await?;
            let header = PacketHeader::parse(&self.buffer)?;
            let total = header.length as usize;

            if total < HEADER_LEN {
                self.broken = true;
                return Err(Error::Protocol("packet length is impossibly small".into()));
            }

            self.fill_to(total).await?;
            payload.extend_from_slice(&self.buffer[HEADER_LEN..total]);
            self.buffer.drain(..total);

            if header.is_end_of_message() {
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

    /// Hang up.
    ///
    /// TDS has no goodbye token — MS-TDS says a client ends a session by
    /// closing the transport — so there is nothing to send first.
    pub async fn close(mut self) {
        let _ = self.stream.shutdown().await;
    }
}

/// Opens SQL Server connections.
pub struct SqlServerDriver {
    config: DatabaseConfig,
    options: SqlServerOptions,
    dialect: Arc<dyn Dialect>,
}

impl SqlServerDriver {
    pub fn new(config: DatabaseConfig) -> Self {
        let options = SqlServerOptions::from_config(&config);
        SqlServerDriver::with_options(config, options)
    }

    pub fn with_options(config: DatabaseConfig, options: SqlServerOptions) -> Self {
        SqlServerDriver {
            config,
            options,
            dialect: Arc::new(crate::dialect::SqlServer),
        }
    }

    pub fn config(&self) -> &DatabaseConfig {
        &self.config
    }

    pub fn options(&self) -> &SqlServerOptions {
        &self.options
    }
}

impl Driver for SqlServerDriver {
    fn generation(&self) -> u64 {
        self.config.generation()
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        Arc::clone(&self.dialect)
    }

    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn DriverConnection>>> {
        Box::pin(async move {
            let connection = SqlServerConnection::connect_with(&self.config.resolved(), self.options).await?;
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

impl DriverConnection for SqlServerConnection {
    fn query<'a>(
        &'a mut self,
        sql: &'a str,
        params: &'a [Value],
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(SqlServerConnection::query(self, sql, params))
    }

    fn simple_query<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(SqlServerConnection::simple_query(self, sql))
    }

    fn is_broken(&self) -> bool {
        SqlServerConnection::is_broken(self)
    }

    fn in_transaction(&self) -> bool {
        SqlServerConnection::in_transaction(self)
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move { SqlServerConnection::close(*self).await })
    }
}

/// The key an `output inserted` clause handed back.
///
/// SQL Server returns a generated key as an ordinary row rather than in the
/// acknowledgement, so there is nothing on the wire that says "this is the
/// identity". The statement is what says so: the dialect emits `output
/// inserted.[id]`, and only a statement carrying that clause has a key to read.
fn generated_key(sql: &str, rows: &[Row]) -> Option<i64> {
    if !sql.to_ascii_lowercase().contains("output inserted") {
        return None;
    }
    rows.first()?.get_at::<i64>(0).ok()
}

/// The host name to send in LOGIN7, which shows up in `sys.dm_exec_sessions`.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "rustlavel".to_string())
}

/// Turn a login failure into something the developer can act on.
///
/// SQL Server's login errors are famously terse — 18456 says "Login failed for
/// user" and nothing else, because saying more would help an attacker — so the
/// actionable half has to come from this side.
fn login_error(error: ServerError, config: &DatabaseConfig) -> Error {
    let number = error.number;
    let base = error.into_error(None);

    let advice = match number {
        18456 => Some(format!(
            "The password for `{}` was rejected, or that login does not exist. Check \
             DATABASE_URL in your .env. SQL Server logs the real reason in its error log; the \
             wire deliberately does not carry it.",
            config.user
        )),
        4060 => Some(format!(
            "Database `{}` cannot be opened by `{}`. Create it, grant access to it, or point \
             DATABASE_URL at one that exists.",
            config.database, config.user
        )),
        18452 => Some(
            "The login is from an untrusted domain and cannot be used with Windows \
             authentication. This driver implements SQL Server authentication only: give \
             DATABASE_URL a username and password."
                .to_string(),
        ),
        _ => None,
    };

    match advice {
        Some(advice) => Error::msg(format!("{base}\n  {advice}")),
        None => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(value: Value) -> Row {
        Row::new(Arc::new(vec!["id".to_string()]), vec![value])
    }

    #[test]
    fn a_generated_key_is_read_only_from_a_statement_that_asked_for_one() {
        let rows = vec![row(Value::Int(42))];

        assert_eq!(
            generated_key("insert into [t] ([n]) output inserted.[id] values (@P1)", &rows),
            Some(42)
        );
        // Case does not matter; the builder may emit either.
        assert_eq!(generated_key("INSERT INTO [t] OUTPUT INSERTED.[id] ...", &rows), Some(42));

        // A plain select returns rows too, and none of them is an identity.
        assert_eq!(generated_key("select id from t", &rows), None);
        // A statement that asked but got nothing back.
        assert_eq!(generated_key("insert ... output inserted.[id] ...", &[]), None);
        // A non-integer key, such as a uuid, is not an insert id.
        assert_eq!(
            generated_key("output inserted.[id]", &[row(Value::Text("abc".into()))]),
            None
        );
    }

    #[test]
    fn a_rejected_password_says_where_to_look() {
        let error = ServerError {
            number: 18456,
            severity: 14,
            state: 1,
            message: "Login failed for user 'ada'.".into(),
            ..ServerError::default()
        };
        let config = DatabaseConfig { user: "ada".into(), ..DatabaseConfig::default() };

        let rendered = login_error(error, &config).to_string();
        assert!(rendered.contains("18456"), "{rendered}");
        assert!(rendered.contains("severity 14"), "{rendered}");
        assert!(rendered.contains("DATABASE_URL"), "{rendered}");
        assert!(rendered.contains("`ada`"), "{rendered}");
    }

    #[test]
    fn a_missing_database_names_the_database_that_is_missing() {
        let error = ServerError {
            number: 4060,
            severity: 11,
            message: "Cannot open database \"blog\" requested by the login.".into(),
            ..ServerError::default()
        };
        let config = DatabaseConfig { database: "blog".into(), ..DatabaseConfig::default() };

        let rendered = login_error(error, &config).to_string();
        assert!(rendered.contains("`blog` cannot be opened"), "{rendered}");
    }

    #[test]
    fn windows_authentication_is_refused_by_name_rather_than_retried() {
        let error = ServerError { number: 18452, severity: 14, ..ServerError::default() };

        let rendered = login_error(error, &DatabaseConfig::default()).to_string();
        assert!(rendered.contains("SQL Server authentication only"), "{rendered}");
    }

    #[test]
    fn an_error_with_no_advice_is_still_reported_verbatim() {
        let error = ServerError {
            number: 208,
            severity: 16,
            message: "Invalid object name 'nope'.".into(),
            ..ServerError::default()
        };

        let rendered = login_error(error, &DatabaseConfig::default()).to_string();
        assert!(rendered.contains("Invalid object name"), "{rendered}");
    }

    #[test]
    fn the_driver_speaks_the_sql_server_dialect_and_never_prints_the_password() {
        let config = DatabaseConfig {
            driver: "sqlserver".into(),
            user: "sa".into(),
            password: "hunter2".into(),
            port: 1433,
            ..DatabaseConfig::default()
        };
        let driver = SqlServerDriver::new(config);

        assert_eq!(driver.dialect().name(), "sqlserver");
        assert_eq!(driver.options().encryption, Encryption::Required);
        assert!(!driver.describe().contains("hunter2"), "{}", driver.describe());
    }

    #[tokio::test]
    async fn connecting_to_a_closed_port_names_the_server_and_the_default_port() {
        let config = DatabaseConfig {
            driver: "sqlserver".into(),
            port: 1,
            password: "hunter2".into(),
            ..DatabaseConfig::default()
        };

        let error = match SqlServerConnection::connect(&config).await {
            Err(error) => error.to_string(),
            Ok(_) => panic!("nothing should be listening on port 1"),
        };

        assert!(error.contains("127.0.0.1:1"), "{error}");
        assert!(error.contains("1433"), "{error}");
        // The password must never appear, even in a connection error.
        assert!(!error.contains("hunter2"), "{error}");
    }
}
