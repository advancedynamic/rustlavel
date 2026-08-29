//! rustlavel-mail: sending email, and notifications that fan out to several
//! channels from one definition.
//!
//! The SMTP client is written here, on Tokio's TCP, the same way the HTTP
//! server and the PostgreSQL driver are — only TLS is delegated, to rustls.
//!
//! ```ignore
//! use rustlavel_mail::{Mail, Message};
//!
//! let mailer = Mail::from_config(&config)?;
//! mailer.send(
//!     Message::new()
//!         .to(("Ada Lovelace", "ada@example.com"))
//!         .subject("Your receipt")
//!         .html("<p>Thank you.</p>"),
//! ).await?;
//! ```
//!
//! # The three transports
//!
//! `mail.transport` picks one, and nothing else in an application changes:
//!
//! - `log` — writes the rendered message to the log. The local default: mail
//!   you can read without a mail server, an inbox, or a network.
//! - `file` — one `.eml` per message under `mail.path`, which any mail client
//!   will open, so a designer can look at the real thing.
//! - `smtp` — the real one.
//!
//! # Testing
//!
//! [`Mail::fake`] records what would have been sent and sends nothing, so a
//! test can assert on the subject and the body without a server anywhere:
//!
//! ```ignore
//! let mailer = Mail::fake();
//! ship_the_order(&mailer).await?;
//! mailer.fake().unwrap().assert_sent_to("ada@example.com");
//! ```

pub mod address;
pub mod config;
pub mod encode;
pub mod fake;
pub mod mailable;
pub mod message;
pub mod notification;
pub mod smtp;
pub mod transport;

pub use address::{Address, IntoAddress};
pub use config::{MailConfig, TransportKind};
pub use fake::Fake;
pub use mailable::{Mailable, html_to_text};
pub use message::{Attachment, Message};
pub use notification::{Delivery, Notification, Notified, Notifier, Recipient, channel};
pub use smtp::{Encryption, Reply, SmtpClient, SmtpConfig};
pub use transport::{FileTransport, LogTransport, Mailer, SmtpTransport, Transport};

use rustlavel_core::{Config, Result};
use std::path::PathBuf;

/// The entry point, in the shape Laravel's `Mail` facade has.
///
/// Every constructor returns a [`Mailer`]; this type only exists so the call
/// that picks a transport reads like the thing it does.
pub struct Mail;

impl Mail {
    /// Build the transport named in `mail.transport`.
    pub fn from_config(config: &Config) -> Result<Mailer> {
        Mailer::from_config(config)
    }

    /// Record instead of sending, for tests. This is `Mail::fake()`.
    pub fn fake() -> Mailer {
        Mailer::new(transport::LogTransport::silent()).faking(Fake::new())
    }

    /// Write each message to the log — the local development default.
    pub fn log() -> Mailer {
        Mailer::new(transport::LogTransport::new())
    }

    /// Write one `.eml` per message under `path`.
    pub fn file(path: impl Into<PathBuf>) -> Mailer {
        Mailer::new(transport::FileTransport::new(path))
    }

    /// Talk to a real SMTP server.
    pub fn smtp(settings: SmtpConfig) -> Mailer {
        Mailer::new(transport::SmtpTransport::new(settings))
    }
}
