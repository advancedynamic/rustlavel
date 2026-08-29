//! Mail configuration, read from `config/mail.json` or `.env`.
//!
//! ```json
//! {
//!   "transport": "${MAIL_TRANSPORT:log}",
//!   "host":      "${MAIL_HOST:127.0.0.1}",
//!   "port":      "${MAIL_PORT:587}",
//!   "username":  "${MAIL_USERNAME}",
//!   "password":  "${MAIL_PASSWORD}",
//!   "path":      "storage/mail",
//!   "from": { "address": "${MAIL_FROM_ADDRESS}", "name": "${MAIL_FROM_NAME}" }
//! }
//! ```
//!
//! The default transport is `log`, deliberately: a new application should not
//! be able to send real mail to real people before anyone has configured it.

use crate::address::Address;
use crate::smtp::{Encryption, SmtpConfig};
use rustlavel_core::{Config, Error, Result};
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// Which transport an application uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportKind {
    /// Talk to a real mail server.
    Smtp,
    /// Write the rendered message to the log. The local default.
    #[default]
    Log,
    /// Write one `.eml` per message under `mail.path`.
    File,
}

impl TransportKind {
    /// Parse a transport name, listing the alternatives when it is not one.
    ///
    /// Refuses rather than falling back: silently logging mail that was meant
    /// to be sent is the kind of bug that is noticed a week later.
    pub fn parse(name: &str) -> Result<TransportKind> {
        match name.trim().to_ascii_lowercase().as_str() {
            "smtp" => Ok(TransportKind::Smtp),
            "" | "log" => Ok(TransportKind::Log),
            "file" | "eml" => Ok(TransportKind::File),
            other => Err(Error::msg(format!(
                "`{other}` is not a mail transport. Set mail.transport to one of: smtp, log, file."
            ))),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            TransportKind::Smtp => "smtp",
            TransportKind::Log => "log",
            TransportKind::File => "file",
        }
    }
}

/// Everything `config/mail.json` holds.
///
/// `Debug` is hand-written so a password cannot reach a log line by being
/// printed alongside the rest of the settings.
#[derive(Clone)]
pub struct MailConfig {
    pub transport: TransportKind,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub encryption: Encryption,
    /// Where the file transport writes its `.eml` files.
    pub path: PathBuf,
    /// The default sender, used by any message that does not set its own.
    pub from_address: String,
    pub from_name: String,
    pub timeout: Duration,
}

impl Default for MailConfig {
    fn default() -> Self {
        MailConfig {
            transport: TransportKind::Log,
            host: "127.0.0.1".into(),
            port: 587,
            username: String::new(),
            password: String::new(),
            encryption: Encryption::Auto,
            path: PathBuf::from("storage/mail"),
            from_address: String::new(),
            from_name: String::new(),
            timeout: Duration::from_secs(30),
        }
    }
}

impl MailConfig {
    pub fn from_app_config(config: &Config) -> Result<MailConfig> {
        let defaults = MailConfig::default();

        Ok(MailConfig {
            transport: TransportKind::parse(&config.string("mail.transport", "log"))?,
            host: config.string("mail.host", &defaults.host),
            port: port_of(config)?,
            username: config.string("mail.username", ""),
            password: config.string("mail.password", ""),
            encryption: Encryption::parse(&config.string("mail.encryption", ""))?,
            path: PathBuf::from(config.string("mail.path", "storage/mail")),
            from_address: config.string("mail.from.address", ""),
            from_name: config.string("mail.from.name", ""),
            ..defaults
        })
    }

    /// The configured sender, if there is one and it is valid.
    pub fn from(&self) -> Result<Option<Address>> {
        if self.from_address.is_empty() {
            return Ok(None);
        }
        let address = if self.from_name.is_empty() {
            Address::new(&self.from_address)
        } else {
            Address::named(&self.from_name, &self.from_address)
        }
        .map_err(|error| {
            Error::msg(format!("mail.from.address is not a valid address: {error}"))
        })?;

        Ok(Some(address))
    }

    /// The settings the SMTP transport needs.
    pub fn smtp(&self) -> SmtpConfig {
        SmtpConfig {
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
            password: self.password.clone(),
            encryption: self.encryption,
            timeout: self.timeout,
            // The EHLO name identifies this client to the server. The sender's
            // domain is the honest answer, and some servers check it.
            hello: self
                .from_address
                .split_once('@')
                .map(|(_, domain)| domain.to_string())
                .unwrap_or_else(|| "localhost".to_string()),
        }
    }

    /// A one-line summary that is safe to log.
    pub fn describe(&self) -> String {
        match self.transport {
            TransportKind::Smtp => format!("smtp {}", self.smtp().describe()),
            TransportKind::Log => "log".to_string(),
            TransportKind::File => format!("file {}", self.path.display()),
        }
    }
}

impl fmt::Debug for MailConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailConfig")
            .field("transport", &self.transport)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("encryption", &self.encryption)
            .field("path", &self.path)
            .field("from_address", &self.from_address)
            .field("from_name", &self.from_name)
            .finish()
    }
}

/// `mail.port` may arrive as a number or as a string out of `.env`.
fn port_of(config: &Config) -> Result<u16> {
    let port = config.int("mail.port", 587);
    u16::try_from(port).map_err(|_| {
        Error::msg(format!("mail.port is {port}, which is not a TCP port number"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unconfigured_application_gets_the_log_transport() {
        let settings = MailConfig::from_app_config(&Config::new()).unwrap();

        assert_eq!(settings.transport, TransportKind::Log);
        assert_eq!(settings.from().unwrap(), None);
    }

    #[test]
    fn a_transport_typo_names_the_valid_choices() {
        let config = Config::new();
        config.set("mail.transport", "smpt");

        let error = MailConfig::from_app_config(&config).unwrap_err().to_string();
        assert!(error.contains("smtp, log, file"), "{error}");
    }

    #[test]
    fn every_documented_key_is_read() {
        let config = Config::new();
        config.set("mail.transport", "smtp");
        config.set("mail.host", "smtp.example.com");
        config.set("mail.port", "2525");
        config.set("mail.username", "ada");
        config.set("mail.password", "hunter2");
        config.set("mail.from.address", "no-reply@example.com");
        config.set("mail.from.name", "Rustlavel");
        config.set("mail.path", "/tmp/some-mail");

        let settings = MailConfig::from_app_config(&config).unwrap();

        assert_eq!(settings.transport, TransportKind::Smtp);
        assert_eq!(settings.host, "smtp.example.com");
        assert_eq!(settings.port, 2525);
        assert_eq!(settings.username, "ada");
        assert_eq!(settings.password, "hunter2");
        assert_eq!(settings.path, PathBuf::from("/tmp/some-mail"));
        assert_eq!(
            settings.from().unwrap().unwrap().to_header(),
            "Rustlavel <no-reply@example.com>"
        );
        // The EHLO name comes from the sender's domain.
        assert_eq!(settings.smtp().hello, "example.com");
    }

    #[test]
    fn the_password_never_reaches_debug_output_or_a_description() {
        let config = Config::new();
        config.set("mail.transport", "smtp");
        config.set("mail.username", "ada");
        config.set("mail.password", "hunter2");
        config.set("mail.host", "smtp.example.com");

        let settings = MailConfig::from_app_config(&config).unwrap();

        for rendered in [format!("{settings:?}"), settings.describe(), format!("{:?}", settings.smtp())] {
            assert!(!rendered.contains("hunter2"), "a password leaked: {rendered}");
        }
        assert!(format!("{settings:?}").contains("<redacted>"));
    }

    #[test]
    fn a_from_address_that_cannot_be_parsed_is_refused_at_boot() {
        let config = Config::new();
        config.set("mail.from.address", "not an address");

        let error = MailConfig::from_app_config(&config).unwrap().from().unwrap_err().to_string();
        assert!(error.contains("mail.from.address"), "{error}");
    }

    #[test]
    fn an_impossible_port_is_refused_rather_than_truncated() {
        let config = Config::new();
        config.set("mail.port", 99_999);

        let error = MailConfig::from_app_config(&config).unwrap_err().to_string();
        assert!(error.contains("not a TCP port"), "{error}");
    }
}
