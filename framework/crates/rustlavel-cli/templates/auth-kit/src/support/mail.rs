//! Sending a message with the From address Settings → Email has.
//!
//! The mailer itself is built from configuration at boot, which is right for
//! the transport — a host and a port are not things to change under a running
//! process. The From address is different: it is on the Email tab, an
//! administrator can edit it, and a message sent without one is refused by the
//! mailer. So every message the kit sends goes through here, which stamps the
//! address on the way past.

use rustlavel::prelude::*;
use rustlavel::mail::{Mailer, Message};

use crate::support::settings::Settings;

/// The mailer Settings → Email describes, or the one the application booted
/// with.
///
/// **The boot-time mailer is built from `Config`, which reads `.env` and
/// `config/mail.json` — it has never seen the settings table.** So a host, a
/// port and a password typed into the Email tab were saved and then ignored:
/// the tab wrote a row and the mailer kept using the environment. Anything the
/// environment decides still wins, because `Settings::get` returns the
/// environment's value for a key with an `env` binding; what changes is that a
/// key the environment leaves alone now reaches the transport.
///
/// Rebuilt per send rather than cached. Building a transport does not connect
/// — that is the point of `Mailer::build` — so the cost is a struct, and the
/// alternative is a mailer that goes stale the moment somebody saves the tab.
async fn mailer_for(req: &Request) -> Option<Mailer> {
    let settings = req.state::<Settings>()?;
    let mut config = rustlavel::mail::MailConfig::from_app_config(req.config()).ok()?;

    if let Ok(transport) = rustlavel::mail::TransportKind::parse(&settings.get("mail.driver").await) {
        config.transport = transport;
    }
    let host = settings.get("mail.host").await;
    if !host.is_empty() {
        config.host = host;
    }
    if let Ok(port) = settings.get("mail.port").await.parse::<u16>()
        && port > 0
    {
        config.port = port;
    }
    if let Ok(encryption) = rustlavel::mail::Encryption::parse(&settings.get("mail.encryption").await) {
        config.encryption = encryption;
    }
    config.username = settings.get("mail.username").await;
    config.password = settings.get("mail.password").await;
    config.from_address = settings.get("mail.from.address").await;
    config.from_name = settings.get("mail.from.name").await;

    Mailer::build(&config).ok()
}

/// Send a message, filling in From from Settings → Email.
///
/// With no mailer registered — the normal case in development — the message is
/// logged instead of sent, and the caller is told nothing failed. A person
/// staring at "check your email" while nothing was ever sent is worse than a
/// line in the log.
pub async fn send(req: &Request, message: Message) -> Result<()> {
    // The settings store first, so what the Email tab holds is what sends.
    // The boot-time mailer is the fallback for an application that registered
    // one but no settings.
    let mailer = match mailer_for(req).await {
        Some(built) => built,
        None => match req.state::<Mailer>() {
            Some(booted) => booted.clone(),
            None => {
                warn!(
                    "no mailer is configured; a message to {} was not sent",
                    message.envelope_recipients().join(", ")
                );
                return Ok(());
            }
        },
    };
    mailer.send(with_from(req, message).await).await
}

/// The message with a From address on it, unless it already carries one.
pub async fn with_from(req: &Request, message: Message) -> Message {
    if message.sender().is_some() {
        return message;
    }
    let (address, name) = from_address(req).await;
    if address.is_empty() {
        return message;
    }
    match name.is_empty() {
        true => message.from(address.as_str()),
        false => message.from((name.as_str(), address.as_str())),
    }
}

/// The configured From, as Settings → Email has it.
pub async fn from_address(req: &Request) -> (String, String) {
    match req.state::<Settings>() {
        Some(settings) => (
            settings.get("mail.from.address").await,
            settings.get("mail.from.name").await,
        ),
        None => (
            req.config().string("mail.from.address", ""),
            req.config().string("mail.from.name", ""),
        ),
    }
}
