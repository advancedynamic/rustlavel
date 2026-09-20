//! Opening a SQLite database, and running statements against it.

use crate::config::DatabaseConfig;
use crate::dialect::{Dialect, Sqlite};
use crate::driver::{BoxFuture, Driver, DriverConnection, QueryResult};
use crate::row::{Columns, Row};
use crate::value::Value;
use rustlavel_core::{Error, Result};
use rusqlite::types::{ToSqlOutput, ValueRef};
use std::sync::Arc;
use std::time::Duration;

pub use crate::config::SQLITE_IN_MEMORY as IN_MEMORY;

pub struct SqliteDriver {
    config: DatabaseConfig,
    dialect: Arc<dyn Dialect>,
    /// For `:memory:`, the URI every connection opens — see [`shared_memory_uri`].
    /// For a file, the path itself.
    target: String,
    /// One connection held open for as long as the driver lives, and only for
    /// an in-memory database.
    ///
    /// A shared-cache in-memory database exists exactly as long as *some*
    /// connection to it is open; when the last one closes, SQLite throws the
    /// database away. The pool legitimately drops to zero idle connections, so
    /// without this a quiet moment would silently empty the database. It is
    /// never handed out and never used for a query — it is a keepalive.
    keeper: std::sync::Mutex<Option<rusqlite::Connection>>,
}

impl SqliteDriver {
    pub fn new(config: DatabaseConfig) -> SqliteDriver {
        let in_memory = config.database == IN_MEMORY || config.database.is_empty();
        let target = if in_memory { shared_memory_uri() } else { config.database.clone() };
        SqliteDriver {
            config,
            dialect: Arc::new(Sqlite),
            target,
            keeper: std::sync::Mutex::new(None),
        }
    }

    /// Where the database lives. `:memory:` is not a path.
    pub fn path(&self) -> &str {
        &self.config.database
    }

    pub fn is_in_memory(&self) -> bool {
        self.config.database == IN_MEMORY || self.config.database.is_empty()
    }
}

impl Driver for SqliteDriver {
    fn dialect(&self) -> Arc<dyn Dialect> {
        self.dialect.clone()
    }

    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn DriverConnection>>> {
        let target = self.target.clone();
        let in_memory = self.is_in_memory();
        let busy = self.config.connect_timeout;
        Box::pin(async move {
            let connection = {
                let target = target.clone();
                tokio::task::spawn_blocking(move || open(&target, in_memory, busy))
                    .await
                    .map_err(|error| {
                        Error::msg(format!("opening a SQLite database panicked: {error}"))
                    })??
            };

            // The first connection to an in-memory database leaves a twin
            // behind to hold it open. Taken here rather than in `new` so that
            // building a driver never blocks and never fails.
            // The lock is taken twice and never held across the `await`: a
            // `std::sync::MutexGuard` is not `Send`, and holding one over a
            // suspension point makes the whole future non-`Send` — which the
            // pool cannot store. Two connects racing here both open a keeper
            // and the loser's is dropped, which costs one file handle once.
            if in_memory && self.keeper.lock().is_ok_and(|keeper| keeper.is_none()) {
                let spare = {
                    let target = target.clone();
                    tokio::task::spawn_blocking(move || open(&target, true, busy))
                        .await
                        .ok()
                        .and_then(Result::ok)
                };
                if let Ok(mut keeper) = self.keeper.lock()
                    && keeper.is_none()
                {
                    *keeper = spare;
                }
            }
            Ok(Box::new(SqliteConnection {
                connection: Some(connection),
                in_transaction: false,
                broken: false,
            }) as Box<dyn DriverConnection>)
        })
    }

    fn describe(&self) -> String {
        // No credentials exist to leak — a file has permissions, not a
        // password — so the path itself is the whole description.
        format!("sqlite://{}", self.config.database)
    }

    fn max_connections(&self) -> usize {
        // SQLite serialises writes whatever this says, and a shared in-memory
        // database is reached through a cache that locks per table. More
        // connections buy concurrent *readers*, which a file benefits from and
        // an in-memory test database does not.
        if self.is_in_memory() {
            return 1;
        }
        self.config.max_connections.max(1)
    }
}

/// A URI naming one shared in-memory database, unique to this driver.
///
/// **`Connection::open_in_memory()` would be the obvious call and it is the
/// wrong one.** It gives each connection its own private blank database, and
/// the pool opens more than one: `PooledConnection::drop` hands a connection
/// back through `tokio::spawn`, but releases its permit straight away, so the
/// next caller can win the permit, find the idle list not yet refilled, and
/// open a second connection. Against a server that is invisible. Against
/// `open_in_memory` it means the table you just created is not there —
/// intermittently, depending on which side of that race you land.
///
/// `mode=memory&cache=shared` makes every connection in this process address
/// the *same* database, so the race stops mattering. The name is unique per
/// driver so two in-memory databases in one process — two tests — cannot see
/// each other's tables.
fn shared_memory_uri() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "file:rustlavel-{}-{}?mode=memory&cache=shared",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn open(path: &str, in_memory: bool, busy: Duration) -> Result<rusqlite::Connection> {
    let connection = if in_memory {
        // `SQLITE_OPEN_URI` is what makes the `file:...?mode=memory` form mean
        // anything; without it SQLite treats the whole string as a filename.
        rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
    } else {
        rusqlite::Connection::open(path)
    }
    .map_err(|error| Error::msg(format!("cannot open the SQLite database `{path}`: {error}")))?;

    // Foreign keys are off by default in SQLite — a compatibility promise it
    // made in 2005 and cannot take back. Every other database in this crate
    // enforces them, so a migration tested here would behave differently on a
    // server. Switch them on before anything else runs.
    connection
        .execute_batch("pragma foreign_keys = on")
        .map_err(|error| Error::msg(format!("cannot enable foreign keys: {error}")))?;

    if !in_memory {
        // WAL lets readers carry on while a writer holds the file. It is a
        // property of the database, not the connection, so it survives — but
        // setting it on each open costs nothing and makes a database created
        // elsewhere behave the same way.
        let _ = connection.execute_batch("pragma journal_mode = wal");
    }

    // Without this, a second writer fails instantly with SQLITE_BUSY instead
    // of waiting its turn.
    connection
        .busy_timeout(busy)
        .map_err(|error| Error::msg(format!("cannot set the busy timeout: {error}")))?;

    Ok(connection)
}

pub struct SqliteConnection {
    /// Taken for the duration of every call, because the handle has to move
    /// onto a blocking thread and `rusqlite::Connection` is not `Sync`. `None`
    /// outside a call means the blocking task panicked and the handle is gone.
    connection: Option<rusqlite::Connection>,
    in_transaction: bool,
    broken: bool,
}

impl SqliteConnection {
    /// Run a closure on the blocking pool with the connection moved into it,
    /// and move the connection back afterwards.
    async fn with<T, F>(&mut self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Connection) -> Result<T> + Send + 'static,
    {
        let Some(connection) = self.connection.take() else {
            self.broken = true;
            return Err(Error::msg(
                "this SQLite connection was lost by an earlier panic and cannot be used",
            ));
        };

        let handle = tokio::task::spawn_blocking(move || {
            let result = work(&connection);
            (connection, result)
        })
        .await;

        match handle {
            Ok((connection, result)) => {
                self.connection = Some(connection);
                if result.is_err() {
                    // A failed statement does not break a connection — a
                    // syntax error or a constraint violation leaves it
                    // perfectly usable, and discarding it would turn every
                    // rejected insert into a reconnect.
                }
                result
            }
            Err(error) => {
                self.broken = true;
                Err(Error::msg(format!("a SQLite statement panicked: {error}")))
            }
        }
    }
}

impl DriverConnection for SqliteConnection {
    fn query<'a>(&'a mut self, sql: &'a str, params: &'a [Value]) -> BoxFuture<'a, Result<QueryResult>> {
        let sql = sql.to_string();
        let params = params.to_vec();
        Box::pin(async move {
            let result = self.with(move |connection| run(connection, &sql, &params)).await?;
            self.in_transaction = connection_in_transaction(self);
            Ok(result)
        })
    }

    fn simple_query<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, Result<QueryResult>> {
        let sql = sql.to_string();
        Box::pin(async move {
            let result = self.with(move |connection| run(connection, &sql, &[])).await?;
            self.in_transaction = connection_in_transaction(self);
            Ok(result)
        })
    }

    fn is_broken(&self) -> bool {
        self.broken || self.connection.is_none()
    }

    fn in_transaction(&self) -> bool {
        self.in_transaction
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            if let Some(connection) = self.connection {
                // `close` hands the connection *back* inside its error, so the
                // whole `Result` is large enough for clippy to object. Nothing
                // here wants either half — the handle is dropped on both paths,
                // which is the entire point — so the result is discarded inside
                // the closure rather than carried out of it.
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = connection.close();
                })
                .await;
            }
        })
    }
}

/// Ask SQLite itself whether a transaction is open, rather than counting
/// `begin`s and `commit`s. A statement can end one — a failed `commit`, a
/// rollback SQLite performs on its own — and a counter would not know.
fn connection_in_transaction(connection: &SqliteConnection) -> bool {
    connection.connection.as_ref().is_some_and(|c| !c.is_autocommit())
}

fn run(connection: &rusqlite::Connection, sql: &str, params: &[Value]) -> Result<QueryResult> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| Error::msg(format!("{error}\n  in: {sql}")))?;

    let bound: Vec<Bound> = params.iter().map(Bound::from_value).collect();
    let as_sql: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|b| b as &dyn rusqlite::ToSql).collect();

    // A statement that returns no columns is a write, and `query` on one
    // yields an empty set rather than the row count — so the two are told
    // apart before being run, not after.
    if statement.column_count() == 0 {
        let affected = statement
            .execute(as_sql.as_slice())
            .map_err(|error| Error::msg(format!("{error}\n  in: {sql}")))?;
        return Ok(QueryResult {
            rows: Vec::new(),
            affected: affected as u64,
            last_insert_id: Some(connection.last_insert_rowid()),
        });
    }

    let columns: Columns = Arc::new(statement.column_names().into_iter().map(str::to_string).collect());
    let mut rows = Vec::new();
    let mut cursor = statement
        .query(as_sql.as_slice())
        .map_err(|error| Error::msg(format!("{error}\n  in: {sql}")))?;

    while let Some(row) = cursor.next().map_err(|error| Error::msg(error.to_string()))? {
        let mut values = Vec::with_capacity(columns.len());
        for index in 0..columns.len() {
            values.push(read(row, index)?);
        }
        rows.push(Row::new(columns.clone(), values));
    }

    Ok(QueryResult { rows, affected: 0, last_insert_id: None })
}

/// Read one column out of a row.
///
/// SQLite has no types, only *affinities*: a column declared `integer` can
/// hold a string, and will hand it back as one. So the value is read by what
/// it actually is, not by what the column claimed.
fn read(row: &rusqlite::Row<'_>, index: usize) -> Result<Value> {
    let raw = row.get_ref(index).map_err(|error| Error::msg(error.to_string()))?;
    Ok(match raw {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(number) => Value::Int(number),
        ValueRef::Real(number) => Value::Float(number),
        ValueRef::Text(bytes) => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Value::Bytes(bytes.to_vec()),
    })
}

/// A `Value` on its way into a statement.
///
/// `Value` is this crate's and `ToSql` is rusqlite's, so neither can implement
/// the other's trait; this carries the conversion.
enum Bound {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

impl Bound {
    fn from_value(value: &Value) -> Bound {
        match value {
            Value::Null => Bound::Null,
            // SQLite has no boolean, and `booleans_are_integers` on the
            // dialect says so; the binding has to agree or a `where active =
            // ?` would never match.
            Value::Bool(flag) => Bound::Int(i64::from(*flag)),
            Value::Int(number) => Bound::Int(*number),
            Value::Float(number) => Bound::Float(*number),
            Value::Text(text) => Bound::Text(text.clone()),
            Value::Bytes(bytes) => Bound::Bytes(bytes.clone()),
            // Stored as text, matching the dialect's `json` affinity. Round
            // trips through `Json::parse` on the way out, in `FromValue`.
            Value::Json(json) => Bound::Text(json.to_string()),
        }
    }
}

impl rusqlite::ToSql for Bound {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Bound::Null => ToSqlOutput::Borrowed(ValueRef::Null),
            Bound::Int(number) => ToSqlOutput::Borrowed(ValueRef::Integer(*number)),
            Bound::Float(number) => ToSqlOutput::Borrowed(ValueRef::Real(*number)),
            Bound::Text(text) => ToSqlOutput::Borrowed(ValueRef::Text(text.as_bytes())),
            Bound::Bytes(bytes) => ToSqlOutput::Borrowed(ValueRef::Blob(bytes)),
        })
    }
}
