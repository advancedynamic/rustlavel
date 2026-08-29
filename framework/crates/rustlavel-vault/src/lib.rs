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

pub mod auth;
pub mod client;
pub mod database;
pub mod error;
pub mod fake;
pub mod kv;
pub mod lease;
pub mod leases;
pub mod plugin;
pub mod renew;
pub mod resolve;

pub use auth::{AppRole, Kubernetes, Login, Token, UserPass};
pub use client::{VaultClient, VaultResponse};
pub use database::{DatabaseCredentials, DatabaseSecrets};
pub use kv::{Kv, Secret as KvSecret, SecretMetadata};
pub use error::VaultError;
pub use fake::{Fake, FakeVault};
pub use lease::Lease;
pub use plugin::Vault;
pub use renew::Renewer;
pub use resolve::{Resolver, Secret, SecretRef, SecretSource};

pub use rustlavel_core::{Error, Result};
