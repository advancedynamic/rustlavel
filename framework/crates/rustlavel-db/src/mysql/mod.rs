//! The MySQL driver, implemented directly on the client/server wire protocol.
//!
//! Speaks to MySQL 5.7 and 8.x, and to MariaDB, over plain TCP. Parameterised
//! statements go through the prepared-statement protocol, so a bound value
//! never reaches the SQL parser as text.

pub mod auth;
pub mod connection;
pub mod protocol;
pub mod types;

pub use connection::{DEFAULT_PORT, DRIVER_NAME, MySqlConnection, MySqlDriver};
