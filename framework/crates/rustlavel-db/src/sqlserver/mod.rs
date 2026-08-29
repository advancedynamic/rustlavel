//! The SQL Server driver, implemented directly on TDS 7.4.
//!
//! TDS — the Tabular Data Stream — is written here from Microsoft's published
//! MS-TDS specification: the packet framing, the pre-login exchange, LOGIN7,
//! SQLBatch, RPC and the token stream that comes back. Nothing is borrowed from
//! an existing client, and nothing goes through ODBC.
//!
//! Three things separate this from the PostgreSQL driver next door:
//!
//! * **Encryption is negotiated inside the handshake.** The TLS records are
//!   tunnelled through TDS packets until the handshake completes. [`auth`]
//!   documents that in full, because it is the part that surprises everyone.
//! * **Parameters travel as arguments to a stored procedure.** Every
//!   parameterised statement is sent as an RPC call to `sp_executesql`, with
//!   the statement text and a declaration list as its first two arguments. A
//!   bound value is never part of the SQL text, so it can never become SQL.
//! * **Values arrive in binary.** [`types`] owns a decoder per type, where the
//!   PostgreSQL driver asks the server for text.
//!
//! # Authentication
//!
//! SQL Server authentication only — a username and a password. Windows
//! integrated authentication (NTLM, Kerberos, SSPI) and Entra federated
//! authentication are not implemented, and a server that demands one of them is
//! told so by name.

pub mod auth;
pub mod connection;
pub mod protocol;
pub mod types;

pub use auth::{Encryption, TlsOptions};
pub use connection::{SqlServerConnection, SqlServerDriver, SqlServerOptions};
