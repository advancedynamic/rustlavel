//! A single PostgreSQL connection.

use super::auth::{self, Scram};
use super::protocol::{Authentication, Backend, Buffer, Field, ServerError, TransactionStatus};
use super::types;
use crate::config::DatabaseConfig;
use crate::random;
use crate::row::{Columns, Row};
use crate::value::Value;
use crate::driver::{BoxFuture, Driver, DriverConnection, QueryResult};
use rustlavel_core::events::Event;
use rustlavel_core::{Error, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Whether query parameters are included in the `db.query` event.
///
/// Bindings are what make a slow-query log useful, but they are also where a
/// password or a token ends up when one is being written. On by default because
/// the instrumentation bus only has subscribers in development; turned off for
/// production by the application at boot.
static LOG_BINDINGS: AtomicBool = AtomicBool::new(true);

pub fn set_log_bindings(enabled: bool) {
    LOG_BINDINGS.store(enabled, Ordering::Relaxed);
}

pub fn log_bindings() -> bool {
    LOG_BINDINGS.load(Ordering::Relaxed)
}

pub struct Connection {
    stream: TcpStream,
    /// Bytes read from the socket but not yet consumed as a message.
    buffer: Vec<u8>,
    config: DatabaseConfig,
    process_id: i32,
    secret: i32,
    status: TransactionStatus,
    /// Set when the connection is known to be unusable, so the pool discards it.
    broken: bool,
}

impl Connection {
    /// Open a connection and complete the startup handshake.
    pub async fn connect(config: &DatabaseConfig) -> Result<Connection> {
        let address = format!("{}:{}", config.host, config.port);

        let stream = tokio::time::timeout(config.connect_timeout, TcpStream::connect(&address))
            .await
            .map_err(|_| {
                Error::msg(format!(
                    "timed out connecting to {address}. Is PostgreSQL running and reachable?"
                ))
            })?
            .map_err(|e| {
                Error::msg(format!(
                    "cannot connect to {}: {e}",
                    config.redacted_url()
                ))
            })?;

        let _ = stream.set_nodelay(true);

        let mut connection = Connection {
            stream,
            buffer: Vec::with_capacity(8 * 1024),
            config: config.clone(),
            process_id: 0,
            secret: 0,
            status: TransactionStatus::Idle,
            broken: false,
        };

        connection.startup().await?;
        Ok(connection)
    }

    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// True while a transaction is open, so the pool never hands back a
    /// connection that would leak one.
    pub fn in_transaction(&self) -> bool {
        self.status != TransactionStatus::Idle
    }

    async fn startup(&mut self) -> Result<()> {
        let mut buffer = Buffer::new();
        buffer.startup(&[
            ("user", self.config.user.as_str()),
            ("database", self.config.database.as_str()),
            ("application_name", self.config.application_name.as_str()),
            // Timestamps and dates come back in a predictable shape.
            ("DateStyle", "ISO, MDY"),
            ("client_encoding", "UTF8"),
        ]);
        self.write(buffer).await?;

        let mut scram: Option<Scram> = None;

        loop {
            match self.read_message().await? {
                Backend::Authentication(Authentication::Ok) => continue,
                Backend::Authentication(Authentication::CleartextPassword) => {
                    let mut buffer = Buffer::new();
                    buffer.password(&self.config.password);
                    self.write(buffer).await?;
                }
                Backend::Authentication(Authentication::Md5Password { salt }) => {
                    let digest =
                        auth::md5_password(&self.config.user, &self.config.password, &salt);
                    let mut buffer = Buffer::new();
                    buffer.password(&digest);
                    self.write(buffer).await?;
                }
                Backend::Authentication(Authentication::Sasl { mechanisms }) => {
                    if !mechanisms.iter().any(|m| m == Scram::MECHANISM) {
                        return Err(Error::msg(format!(
                            "the server offers only {mechanisms:?}; this driver implements {}",
                            Scram::MECHANISM
                        )));
                    }
                    let exchange = Scram::new(&self.config.password, random::nonce(24));
                    let mut buffer = Buffer::new();
                    buffer.sasl_initial(Scram::MECHANISM, &exchange.client_first());
                    self.write(buffer).await?;
                    scram = Some(exchange);
                }
                Backend::Authentication(Authentication::SaslContinue { data }) => {
                    let exchange = scram
                        .as_mut()
                        .ok_or_else(|| Error::Protocol("SASL continue before SASL start".into()))?;
                    let response = exchange.client_final(&data)?;
                    let mut buffer = Buffer::new();
                    buffer.sasl_response(&response);
                    self.write(buffer).await?;
                }
                Backend::Authentication(Authentication::SaslFinal { data }) => {
                    scram
                        .as_ref()
                        .ok_or_else(|| Error::Protocol("SASL final before SASL start".into()))?
                        .verify(&data)?;
                }
                Backend::Authentication(Authentication::Unsupported(code)) => {
                    return Err(Error::msg(format!(
                        "the server requested authentication method {code}, which this driver does not implement"
                    )));
                }
                Backend::BackendKeyData { process_id, secret } => {
                    self.process_id = process_id;
                    self.secret = secret;
                }
                Backend::ParameterStatus { .. } | Backend::Notice(_) => continue,
                Backend::ReadyForQuery(status) => {
                    self.status = status;
                    return Ok(());
                }
                Backend::Error(error) => {
                    self.broken = true;
                    return Err(authentication_error(error, &self.config));
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "unexpected message during startup: {other:?}"
                    )));
                }
            }
        }
    }

    /// Run a statement with no parameters through the simple query protocol.
    ///
    /// Used for DDL and for statements that must run as one unit, such as
    /// `begin`/`commit`.
    pub async fn simple_query(&mut self, sql: &str) -> Result<QueryResult> {
        let started = Instant::now();
        let mut buffer = Buffer::new();
        buffer.query(sql);
        self.write(buffer).await?;

        let result = self.collect(sql).await;
        self.record(sql, &[], started, &result);
        result
    }

    /// Run a parameterised statement through the extended query protocol.
    ///
    /// Parameters never enter the SQL text, so a value cannot change the shape
    /// of the statement — this is what makes SQL injection structurally
    /// impossible rather than a matter of remembering to escape.
    pub async fn query(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult> {
        if params.is_empty() {
            // Still uses the extended protocol, so a single statement per call
            // is enforced either way.
            return self.extended(sql, params).await;
        }
        self.extended(sql, params).await
    }

    async fn extended(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult> {
        let started = Instant::now();
        let encoded: Vec<Option<String>> = params.iter().map(Value::to_sql_text).collect();

        let mut buffer = Buffer::new();
        buffer.parse("", sql);
        buffer.bind("", "", &encoded);
        buffer.describe_portal("");
        buffer.execute("", 0);
        buffer.sync();
        self.write(buffer).await?;

        let result = self.collect(sql).await;
        self.record(sql, params, started, &result);
        result
    }

    /// Read messages until `ReadyForQuery`, gathering rows on the way.
    ///
    /// The loop always runs to `ReadyForQuery` even after an error, otherwise
    /// the next query would read this one's leftovers.
    async fn collect(&mut self, sql: &str) -> Result<QueryResult> {
        let mut columns: Columns = Arc::new(Vec::new());
        let mut fields: Vec<Field> = Vec::new();
        let mut result = QueryResult::default();
        let mut failure: Option<ServerError> = None;

        loop {
            match self.read_message().await? {
                Backend::RowDescription(described) => {
                    columns = Arc::new(described.iter().map(|f| f.name.clone()).collect());
                    fields = described;
                }
                Backend::DataRow(raw) => {
                    let values = raw
                        .iter()
                        .enumerate()
                        .map(|(index, bytes)| {
                            let oid = fields.get(index).map_or(types::TEXT, |f| f.type_oid);
                            types::decode(oid, bytes.as_deref())
                        })
                        .collect();
                    result.rows.push(Row::new(Arc::clone(&columns), values));
                }
                Backend::CommandComplete(tag) => result.affected = affected_rows(&tag),
                Backend::Error(error) => failure = Some(error),
                Backend::ReadyForQuery(status) => {
                    self.status = status;
                    break;
                }
                Backend::EmptyQueryResponse
                | Backend::ParseComplete
                | Backend::BindComplete
                | Backend::CloseComplete
                | Backend::NoData
                | Backend::PortalSuspended
                | Backend::Notice(_)
                | Backend::ParameterStatus { .. }
                | Backend::NotificationResponse { .. }
                | Backend::BackendKeyData { .. }
                | Backend::Other(_)
                | Backend::Authentication(_) => {}
            }
        }

        match failure {
            Some(error) => Err(error.into_error(Some(sql))),
            None => Ok(result),
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

    async fn write(&mut self, buffer: Buffer) -> Result<()> {
        let bytes = buffer.into_bytes();
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

    /// Read exactly one backend message.
    async fn read_message(&mut self) -> Result<Backend> {
        // A message is a type byte plus a length that includes itself.
        self.fill_to(5).await?;
        let tag = self.buffer[0];
        let length = i32::from_be_bytes(self.buffer[1..5].try_into().expect("4 bytes")) as usize;

        if length < 4 {
            self.broken = true;
            return Err(Error::Protocol("message length is impossibly small".into()));
        }

        let total = length + 1;
        self.fill_to(total).await?;
        let body = self.buffer[5..total].to_vec();
        self.buffer.drain(..total);

        Backend::parse(tag, &body)
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

    /// Ask the server to close the session politely.
    pub async fn close(mut self) {
        let mut buffer = Buffer::new();
        buffer.terminate();
        let _ = self.write(buffer).await;
        let _ = self.stream.shutdown().await;
    }
}

/// Opens PostgreSQL connections.
///
/// The reference implementation of [`Driver`]: everything above the driver line
/// is written once, and this is what that line looks like from below.
pub struct PostgresDriver {
    config: DatabaseConfig,
    dialect: Arc<dyn crate::dialect::Dialect>,
}

impl PostgresDriver {
    pub fn new(config: DatabaseConfig) -> Self {
        PostgresDriver { config, dialect: Arc::new(crate::dialect::Postgres) }
    }

    pub fn config(&self) -> &DatabaseConfig {
        &self.config
    }
}

impl Driver for PostgresDriver {
    fn dialect(&self) -> Arc<dyn crate::dialect::Dialect> {
        Arc::clone(&self.dialect)
    }

    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn DriverConnection>>> {
        Box::pin(async move {
            let connection = Connection::connect(&self.config).await?;
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

impl DriverConnection for Connection {
    fn query<'a>(
        &'a mut self,
        sql: &'a str,
        params: &'a [Value],
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(Connection::query(self, sql, params))
    }

    fn simple_query<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(Connection::simple_query(self, sql))
    }

    fn is_broken(&self) -> bool {
        Connection::is_broken(self)
    }

    fn in_transaction(&self) -> bool {
        Connection::in_transaction(self)
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move { Connection::close(*self).await })
    }
}

/// `INSERT 0 3`, `UPDATE 2`, `SELECT 5` → the trailing count.
fn affected_rows(tag: &str) -> u64 {
    tag.split_whitespace().next_back().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// Turn an authentication failure into something the developer can act on.
fn authentication_error(error: ServerError, config: &DatabaseConfig) -> Error {
    let base = error.clone().into_error(None);

    let advice = match error.code.as_str() {
        "28P01" => Some(format!(
            "The password for `{}` was rejected. Check DATABASE_URL in your .env.",
            config.user
        )),
        "3D000" => Some(format!(
            "Database `{}` does not exist. Create it, or point DATABASE_URL at an existing one.",
            config.database
        )),
        "28000" => Some(
            "The server rejected this role or host. Check pg_hba.conf allows this connection."
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

    #[test]
    fn reads_the_row_count_from_a_command_tag() {
        assert_eq!(affected_rows("INSERT 0 3"), 3);
        assert_eq!(affected_rows("UPDATE 2"), 2);
        assert_eq!(affected_rows("DELETE 0"), 0);
        assert_eq!(affected_rows("CREATE TABLE"), 0);
    }

    #[test]
    fn bindings_can_be_kept_out_of_the_event_stream() {
        // Restored immediately: the flag is process-wide.
        assert!(log_bindings());
        set_log_bindings(false);
        assert!(!log_bindings());
        set_log_bindings(true);
    }

    #[test]
    fn a_wrong_password_explains_where_to_look() {
        let error = ServerError {
            code: "28P01".into(),
            message: "password authentication failed".into(),
            ..ServerError::default()
        };
        let config = DatabaseConfig { user: "ada".into(), ..DatabaseConfig::default() };

        let rendered = authentication_error(error, &config).to_string();
        assert!(rendered.contains("DATABASE_URL"));
        assert!(rendered.contains("`ada`"));
    }

    #[tokio::test]
    async fn connecting_to_a_closed_port_names_the_server() {
        let config = DatabaseConfig { port: 1, ..DatabaseConfig::default() };
        let error = match Connection::connect(&config).await {
            Err(error) => error.to_string(),
            Ok(_) => panic!("nothing should be listening on port 1"),
        };

        assert!(error.contains("127.0.0.1:1"));
        // The password must never appear, even in a connection error.
        assert!(!error.contains("***@") || !error.contains("hunter"));
    }
}
