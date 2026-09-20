//! The SQLite driver — the one database here that is not on the other end of a
//! socket.
//!
//! Every other driver in this crate implements a wire protocol from scratch,
//! because the database is a server and the protocol is published. SQLite is a
//! C library that reads a file in this process, so there is no protocol to
//! write and nothing to connect to. What this module does instead is bridge
//! two mismatches: **blocking to async**, and **a pool to a file**.
//!
//! # Blocking to async
//!
//! Every SQLite call blocks. Calling one directly from an async task stalls a
//! runtime worker thread and, under load, starves every other task on it — the
//! classic way to make an async program slower than a threaded one. So each
//! statement runs inside [`tokio::task::spawn_blocking`], on a thread pool
//! meant for exactly this.
//!
//! The connection has to move onto that thread and back, because
//! `rusqlite::Connection` is `Send` but not `Sync`. It is held in an `Option`
//! and taken for the duration of the call; if the blocking task panics the
//! `None` is left behind, and [`SqliteConnection::is_broken`] reports it so
//! the pool discards the connection rather than handing out one that has no
//! handle inside it.
//!
//! # A pool to a file
//!
//! **`:memory:` gets exactly one connection, and that is not a tuning
//! decision.** Each SQLite in-memory connection is its own separate blank
//! database. A pool of ten would hand out ten different databases, and a test
//! would create a table on one and be told it does not exist on the next —
//! intermittently, depending on which connection the pool picked. The driver
//! caps it at one so that cannot happen.
//!
//! A file-backed database can take several connections, and gets WAL, which
//! lets readers run while a writer holds the file. Writes are still serialised
//! by SQLite itself; `busy_timeout` makes a blocked writer wait rather than
//! fail immediately.

pub mod connection;

pub use connection::{SqliteConnection, SqliteDriver};
