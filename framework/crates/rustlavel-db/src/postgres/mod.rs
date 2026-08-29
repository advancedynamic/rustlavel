//! The PostgreSQL driver, implemented directly on the version 3 wire protocol.

pub mod auth;
pub mod connection;
pub mod protocol;
pub mod types;

pub use connection::{Connection, QueryResult};
