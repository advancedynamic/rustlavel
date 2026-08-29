//! rustlavel-vault: secrets from OpenBao or HashiCorp Vault.
//!
//! One package for both. OpenBao is a fork of Vault from before the licence
//! changed to BUSL, and the HTTP API is the same one — nothing here compiles
//! against either project, so neither licence reaches your binary.
//!
//! The point is to stop a database password living in `.env`. A file on disk
//! is a file that gets copied into a backup, a container image, a screenshot
//! and eventually a repository. A secret fetched at boot lives in memory, and a
//! *dynamic* one — an account the store creates for this process and deletes
//! when the lease ends — cannot be replayed from a leaked backup at all.
//!
//! Written from scratch on the published HTTP API, over `rustlavel-client`.

pub mod client;
pub mod error;
pub mod lease;

pub use client::{VaultClient, VaultResponse};
pub use error::VaultError;
pub use lease::Lease;

pub use rustlavel_core::{Error, Result};
