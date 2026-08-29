//! The contract every database driver implements.
//!
//! Above this line the framework is one codebase: the query builder, the schema
//! builder, the migrator and the ORM are written once. Below it sits a wire
//! protocol per database, each written from scratch.
//!
//! Both traits hand back boxed futures rather than using `async fn`, because a
//! trait with `async fn` is not object-safe, and the pool has to hold a driver
//! whose type it does not know.

use crate::dialect::Dialect;
use crate::row::Row;
use crate::value::Value;
use rustlavel_core::Result;
use std::pin::Pin;
use std::sync::Arc;

/// A future returned from a trait method.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What a statement returned.
#[derive(Debug, Default)]
pub struct QueryResult {
    pub rows: Vec<Row>,
    /// Rows affected, as the database reported it.
    pub affected: u64,
    /// The key an insert generated, when the database volunteered one.
    ///
    /// PostgreSQL and SQL Server return it as a row; MySQL reports it in the
    /// packet that acknowledges the insert, with no row at all — which is why
    /// this is separate from `rows`.
    pub last_insert_id: Option<i64>,
}

/// One physical connection.
pub trait DriverConnection: Send {
    /// Run a statement with bound parameters.
    ///
    /// Parameters never enter the SQL text, whatever the database: that is what
    /// makes injection structurally impossible rather than a matter of
    /// remembering to escape.
    fn query<'a>(
        &'a mut self,
        sql: &'a str,
        params: &'a [Value],
    ) -> BoxFuture<'a, Result<QueryResult>>;

    /// Run a statement with no parameters — DDL, and transaction control.
    fn simple_query<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, Result<QueryResult>>;

    /// Whether this connection is known to be unusable, so the pool discards it
    /// instead of handing it to the next caller.
    fn is_broken(&self) -> bool;

    /// Whether a transaction is still open. A connection left inside one must
    /// never go back into rotation.
    fn in_transaction(&self) -> bool;

    /// Say goodbye politely, then hang up.
    fn close(self: Box<Self>) -> BoxFuture<'static, ()>;
}

/// Opens connections, and knows what dialect they speak.
pub trait Driver: Send + Sync + 'static {
    fn dialect(&self) -> Arc<dyn Dialect>;

    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn DriverConnection>>>;

    /// How this connection is described in an error or a log line.
    ///
    /// Must never contain the password — it ends up in messages people paste
    /// into issues.
    fn describe(&self) -> String;

    /// How many connections the pool should allow at once.
    fn max_connections(&self) -> usize {
        10
    }

    /// Which generation of credentials a connection opened now would belong to.
    ///
    /// Zero when the driver's credentials never change, which is the common
    /// case and the one that costs nothing: the pool compares the number it
    /// stored against this, and zero always equals zero.
    fn generation(&self) -> u64 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Postgres;
    use rustlavel_core::Error;

    /// A driver that never connects, to prove the traits compose without a
    /// database anywhere near them.
    struct Offline;

    impl Driver for Offline {
        fn dialect(&self) -> Arc<dyn Dialect> {
            Arc::new(Postgres)
        }

        fn connect(&self) -> BoxFuture<'_, Result<Box<dyn DriverConnection>>> {
            Box::pin(async { Err(Error::msg("this driver never connects")) })
        }

        fn describe(&self) -> String {
            "offline://nowhere".into()
        }
    }

    #[tokio::test]
    async fn a_driver_can_be_held_without_naming_its_type() {
        let driver: Arc<dyn Driver> = Arc::new(Offline);

        assert_eq!(driver.dialect().name(), "postgres");
        assert_eq!(driver.describe(), "offline://nowhere");
        assert_eq!(driver.max_connections(), 10);
        assert!(driver.connect().await.is_err());
    }
}
