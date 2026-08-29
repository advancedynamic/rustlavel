//! SMTP, written here rather than taken from a crate.
//!
//! The protocol is a line conversation: the server greets, the client says
//! `EHLO` and learns what the server can do, they may upgrade to TLS, the
//! client authenticates, and then each message is an envelope (`MAIL FROM`,
//! one `RCPT TO` per recipient) followed by `DATA`.

pub mod client;
pub mod reply;
pub mod stream;

pub use client::SmtpClient;
pub use reply::Reply;
pub use stream::SmtpStream;

use rustlavel_core::{Error, Result};
use std::fmt;
use std::time::Duration;

/// How the connection is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encryption {
    /// Implicit TLS on port 465, STARTTLS everywhere else when the server
    /// offers it. The right answer almost always, and the default.
    #[default]
    Auto,
    /// STARTTLS, and refuse to continue if the server does not offer it.
    StartTls,
    /// TLS from the first byte, the way port 465 works.
    Tls,
    /// Plaintext. Only sane against a mail catcher on localhost.
    None,
}

impl Encryption {
    pub fn parse(value: &str) -> Result<Encryption> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Encryption::Auto),
            "tls" | "ssl" | "implicit" => Ok(Encryption::Tls),
            "starttls" => Ok(Encryption::StartTls),
            "none" | "off" | "null" => Ok(Encryption::None),
            other => Err(Error::msg(format!(
                "`{other}` is not an encryption mode. Use one of: auto, starttls, tls, none."
            ))),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Encryption::Auto => "auto",
            Encryption::StartTls => "starttls",
            Encryption::Tls => "tls",
            Encryption::None => "none",
        }
    }
}

/// Everything needed to reach one SMTP server.
///
/// `Debug` is written by hand: a config struct ends up in log lines and in
/// error contexts, and a password that reaches either is a password that has
/// to be rotated.
#[derive(Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub encryption: Encryption,
    /// How long any single read or write may take.
    pub timeout: Duration,
    /// The name announced in `EHLO`.
    pub hello: String,
}

impl Default for SmtpConfig {
    fn default() -> Self {
        SmtpConfig {
            host: "127.0.0.1".into(),
            port: 587,
            username: String::new(),
            password: String::new(),
            encryption: Encryption::Auto,
            timeout: Duration::from_secs(30),
            hello: "localhost".into(),
        }
    }
}

impl SmtpConfig {
    pub fn new(host: impl Into<String>, port: u16) -> SmtpConfig {
        SmtpConfig { host: host.into(), port, ..SmtpConfig::default() }
    }

    pub fn credentials(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.username = username.into();
        self.password = password.into();
        self
    }

    pub fn encryption(mut self, encryption: Encryption) -> Self {
        self.encryption = encryption;
        self
    }

    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Whether TLS is negotiated before the greeting, as port 465 requires.
    pub fn implicit_tls(&self) -> bool {
        match self.encryption {
            Encryption::Tls => true,
            Encryption::Auto => self.port == 465,
            _ => false,
        }
    }

    pub fn wants_starttls(&self) -> bool {
        !self.implicit_tls() && matches!(self.encryption, Encryption::Auto | Encryption::StartTls)
    }

    pub fn requires_starttls(&self) -> bool {
        !self.implicit_tls() && self.encryption == Encryption::StartTls
    }

    /// Safe to describe in a log line or an error.
    pub fn describe(&self) -> String {
        let user = if self.username.is_empty() { "anonymous" } else { self.username.as_str() };
        format!("{}:{} as {} ({})", self.host, self.port, user, self.encryption.name())
    }
}

impl fmt::Debug for SmtpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            // Never the password, not even redacted per character: a length is
            // information too.
            .field("password", &"<redacted>")
            .field("encryption", &self.encryption)
            .field("timeout", &self.timeout)
            .field("hello", &self.hello)
            .finish()
    }
}

/// Whether credentials may be sent over this connection.
///
/// `AUTH PLAIN` is base64, not encryption: sending it over a cleartext socket
/// hands the password to anything between here and the server. Refused unless
/// the server is on this machine, where there is no wire to listen to.
pub fn may_authenticate(encrypted: bool, host: &str) -> bool {
    encrypted || is_loopback(host)
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") || host.starts_with("127.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encryption_modes_parse_by_the_names_people_write_in_env_files() {
        assert_eq!(Encryption::parse("").unwrap(), Encryption::Auto);
        assert_eq!(Encryption::parse("SSL").unwrap(), Encryption::Tls);
        assert_eq!(Encryption::parse(" starttls ").unwrap(), Encryption::StartTls);
        assert!(Encryption::parse("maybe").unwrap_err().to_string().contains("auto, starttls"));
    }

    #[test]
    fn port_465_means_tls_before_the_greeting() {
        assert!(SmtpConfig::new("smtp.example.com", 465).implicit_tls());
        assert!(!SmtpConfig::new("smtp.example.com", 587).implicit_tls());
        assert!(SmtpConfig::new("smtp.example.com", 587).wants_starttls());
        assert!(!SmtpConfig::new("smtp.example.com", 465).wants_starttls());

        let off = SmtpConfig::new("localhost", 1025).encryption(Encryption::None);
        assert!(!off.wants_starttls() && !off.implicit_tls());
    }

    #[test]
    fn a_password_never_appears_in_debug_output_or_a_description() {
        let config = SmtpConfig::new("smtp.example.com", 587).credentials("ada", "hunter2");

        let debugged = format!("{config:?}");
        assert!(!debugged.contains("hunter2"), "{debugged}");
        assert!(debugged.contains("<redacted>"));

        let described = config.describe();
        assert!(!described.contains("hunter2"), "{described}");
        assert!(described.contains("smtp.example.com:587 as ada"));
    }

    #[test]
    fn credentials_are_only_sent_over_tls_or_to_this_machine() {
        assert!(may_authenticate(true, "smtp.example.com"));
        assert!(!may_authenticate(false, "smtp.example.com"));
        assert!(may_authenticate(false, "localhost"));
        assert!(may_authenticate(false, "127.0.0.1"));
    }
}
