//! rustlavel-db: the database package.
//!
//! A PostgreSQL driver written directly on the version 3 wire protocol, a
//! connection pool, a query builder, schema migrations, and seeding. Enabled
//! with `cargo add rustlavel-db` — an application that never adds it never
//! compiles a line of this.

pub mod base64;
pub mod builder;
pub mod config;
pub mod dialect;
pub mod driver;
pub mod migration;
pub mod model;
pub mod mysql;
pub mod pagination;
pub mod pool;
pub mod postgres;
pub mod random;
pub mod row;
pub mod schema;
pub mod sqlserver;
pub mod value;

pub use builder::{Direction, QueryBuilder};
pub use config::DatabaseConfig;
pub use dialect::{ColumnType, Dialect, ReturningStyle};
pub use driver::{Driver, DriverConnection, QueryResult};
pub use migration::{Faker, Migration, Migrator, Seeder};
pub use model::{Model, ModelExt, belongs_to, has_many};
pub use mysql::{MySqlConnection, MySqlDriver};
pub use pagination::{CursorPage, Page};
pub use postgres::connection::{log_bindings, set_log_bindings};
pub use pool::{Pool, PooledConnection};
pub use schema::{Schema, Table};
pub use sqlserver::{SqlServerConnection, SqlServerDriver};
pub use row::{Row, rows_to_json};
pub use value::{FromValue, Value};

pub use rustlavel_core::{Error, Result};
use std::sync::Arc;

/// Build the driver a configuration asks for.
///
/// A driver that is not compiled into this build says so by name, rather than
/// failing later with something about a connection.
fn driver_for(config: DatabaseConfig) -> Result<Arc<dyn Driver>> {
    match config.driver.as_str() {
        "postgres" => Ok(Arc::new(postgres::PostgresDriver::new(config))),
        "mysql" => Ok(Arc::new(mysql::MySqlDriver::new(config))),
        "sqlserver" => Ok(Arc::new(sqlserver::SqlServerDriver::new(config))),
        other => Err(Error::msg(format!(
            "the `{other}` driver is not available in this build. \
             Point DATABASE_URL at a database this build supports."
        ))),
    }
}

/// `#[derive(Model)]`.
pub use rustlavel_macros::Model;

/// What a migration, seeder, or model file imports.
pub mod prelude {
    pub use crate::migration::{Faker, Migrator, Seeder};
    pub use crate::model::{ModelExt, belongs_to, has_many};
    pub use crate::schema::{Schema, Table};
    pub use crate::{CursorPage, Database, Model, Page, QueryBuilder, Row, Value};
    pub use rustlavel_core::{Error, Json, Result};
}

/// The application's handle on the database.
///
/// Registered as application state, so a handler reaches it with
/// `req.state::<Database>()`.
#[derive(Clone)]
pub struct Database {
    pool: Pool,
    dialect: Arc<dyn Dialect>,
}

impl Database {
    /// Connect using a URL: `postgres://user:password@host:port/database`.
    pub async fn connect(url: &str) -> Result<Database> {
        Database::with_config(DatabaseConfig::from_url(url)?).await
    }

    /// Connect using explicit settings, verifying the connection works.
    pub async fn with_config(config: DatabaseConfig) -> Result<Database> {
        let database = Database::lazy(config)?;
        database.pool.verify().await?;
        Ok(database)
    }

    /// Build a handle without touching the network. Useful when the process
    /// should start even if the database is briefly down.
    pub fn lazy(config: DatabaseConfig) -> Result<Database> {
        Ok(Database::with_driver(driver_for(config)?))
    }

    /// Use a driver directly — how a database this crate does not know about
    /// would be plugged in.
    pub fn with_driver(driver: Arc<dyn Driver>) -> Database {
        let dialect = driver.dialect();
        Database { pool: Pool::new(driver), dialect }
    }

    /// What SQL this connection speaks.
    ///
    /// The query and schema builders take it, which is how one builder produces
    /// correct SQL for three different databases.
    pub fn dialect(&self) -> &dyn Dialect {
        self.dialect.as_ref()
    }

    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Start a query: `db.table("users").filter("active", true).get(&db).await`.
    pub fn table(&self, name: &str) -> QueryBuilder {
        QueryBuilder::new(name)
    }

    /// Run a query and return every row.
    pub async fn select(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        let mut connection = self.pool.acquire().await?;
        Ok(connection.query(sql, params).await?.rows)
    }

    /// Run a query expecting at most one row.
    pub async fn select_one(&self, sql: &str, params: &[Value]) -> Result<Option<Row>> {
        Ok(self.select(sql, params).await?.into_iter().next())
    }

    /// Run a statement and return the number of rows it affected.
    pub async fn execute(&self, sql: &str, params: &[Value]) -> Result<u64> {
        let mut connection = self.pool.acquire().await?;
        Ok(connection.query(sql, params).await?.affected)
    }

    /// Run one or more statements with no parameters — DDL, mostly.
    pub async fn run(&self, sql: &str) -> Result<u64> {
        let mut connection = self.pool.acquire().await?;
        Ok(connection.simple_query(sql).await?.affected)
    }

    /// Run an insert and hand back the key the database generated.
    ///
    /// Three mechanisms, one method: PostgreSQL returns a row from `RETURNING`,
    /// SQL Server from `OUTPUT`, and MySQL reports the id in the packet that
    /// acknowledges the insert, with no row at all.
    pub async fn insert_returning_key(
        &self,
        sql: &str,
        params: &[Value],
        column: &str,
    ) -> Result<Option<Value>> {
        let mut connection = self.pool.acquire().await?;
        let result = connection.query(sql, params).await?;

        if let Some(row) = result.rows.first() {
            // Named lookup where the database labelled the column, positional
            // where it did not.
            return Ok(Some(row.value(column).or_else(|_| row.value_at(0))?.clone()));
        }
        Ok(result.last_insert_id.map(Value::Int))
    }

    /// Read a single value from the first column of the first row.
    pub async fn scalar<T: FromValue>(&self, sql: &str, params: &[Value]) -> Result<Option<T>> {
        match self.select_one(sql, params).await? {
            Some(row) => row.get_at::<T>(0).map(Some),
            None => Ok(None),
        }
    }

    /// Begin a transaction.
    ///
    /// Rust's borrow rules make Laravel's `DB::transaction(closure)` shape
    /// awkward — the closure's future would have to borrow the connection it
    /// was handed — so the transaction is a value you hold instead:
    ///
    /// ```ignore
    /// let mut tx = db.begin().await?;
    /// tx.execute("update accounts set balance = balance - $1", &[amount]).await?;
    /// tx.commit().await?;
    /// ```
    ///
    /// Dropping it without committing rolls back, so an early `?` cannot leave
    /// a half-finished transaction behind.
    pub async fn begin(&self) -> Result<Transaction> {
        let mut connection = self.pool.acquire().await?;
        connection.simple_query("begin").await?;
        Ok(Transaction { connection: Some(connection), finished: false })
    }

    /// Close every pooled connection.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// An open transaction, holding its connection until it ends.
pub struct Transaction {
    connection: Option<PooledConnection>,
    finished: bool,
}

impl Transaction {
    fn connection(&mut self) -> Result<&mut PooledConnection> {
        self.connection
            .as_mut()
            .ok_or_else(|| Error::msg("this transaction has already finished"))
    }

    pub async fn select(&mut self, sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        Ok(self.connection()?.query(sql, params).await?.rows)
    }

    pub async fn select_one(&mut self, sql: &str, params: &[Value]) -> Result<Option<Row>> {
        Ok(self.select(sql, params).await?.into_iter().next())
    }

    pub async fn execute(&mut self, sql: &str, params: &[Value]) -> Result<u64> {
        Ok(self.connection()?.query(sql, params).await?.affected)
    }

    pub async fn run(&mut self, sql: &str) -> Result<u64> {
        Ok(self.connection()?.simple_query(sql).await?.affected)
    }

    pub async fn scalar<T: FromValue>(&mut self, sql: &str, params: &[Value]) -> Result<Option<T>> {
        match self.select_one(sql, params).await? {
            Some(row) => row.get_at::<T>(0).map(Some),
            None => Ok(None),
        }
    }

    /// A named savepoint, so part of a transaction can be undone on its own.
    pub async fn savepoint(&mut self, name: &str) -> Result<()> {
        validate_identifier(name)?;
        self.connection()?.simple_query(&format!("savepoint {name}")).await?;
        Ok(())
    }

    pub async fn rollback_to(&mut self, name: &str) -> Result<()> {
        validate_identifier(name)?;
        self.connection()?.simple_query(&format!("rollback to savepoint {name}")).await?;
        Ok(())
    }

    /// Commit and release the connection.
    pub async fn commit(mut self) -> Result<()> {
        self.connection()?.simple_query("commit").await?;
        self.finished = true;
        Ok(())
    }

    /// Roll back and release the connection.
    pub async fn rollback(mut self) -> Result<()> {
        self.connection()?.simple_query("rollback").await?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Nothing committed it, so undo it. The pool would discard a connection
        // left in a transaction anyway; rolling back explicitly returns it to
        // service instead of throwing it away.
        if let Some(mut connection) = self.connection.take() {
            tokio::spawn(async move {
                let _ = connection.simple_query("rollback").await;
            });
        }
    }
}

/// Reject anything that is not a plain identifier.
///
/// Identifiers cannot be sent as parameters, so every place the framework
/// interpolates one into SQL passes through here first.
pub fn validate_identifier(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');

    if valid {
        Ok(())
    } else {
        Err(Error::msg(format!(
            "`{name}` is not a valid SQL identifier. Identifiers may contain letters, digits and \
             underscores, and must not start with a digit."
        )))
    }
}

/// Quote an identifier after validating it.
pub fn quote_identifier(name: &str) -> Result<String> {
    // A qualified name (`schema.table`) is validated one part at a time.
    let quoted: Result<Vec<String>> = name
        .split('.')
        .map(|part| validate_identifier(part).map(|_| format!("\"{part}\"")))
        .collect();
    Ok(quoted?.join("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_identifiers() {
        for name in ["users", "user_profiles", "_private", "t1"] {
            assert!(validate_identifier(name).is_ok(), "{name} should be valid");
        }
    }

    #[test]
    fn rejects_anything_that_could_alter_a_statement() {
        for name in ["users; drop table users", "user\"s", "1abc", "", "a b", "users--"] {
            assert!(validate_identifier(name).is_err(), "{name:?} should be rejected");
        }
    }

    #[test]
    fn quotes_qualified_names_part_by_part() {
        assert_eq!(quote_identifier("public.users").unwrap(), "\"public\".\"users\"");
        assert!(quote_identifier("public.users; drop table x").is_err());
    }
}
