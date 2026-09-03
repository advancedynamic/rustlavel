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

/// Send a message, filling in From from Settings → Email.
///
/// With no mailer registered — the normal case in development — the message is
/// logged instead of sent, and the caller is told nothing failed. A person
/// staring at "check your email" while nothing was ever sent is worse than a
/// line in the log.
pub async fn send(req: &Request, message: Message) -> Result<()> {
    let Some(mailer) = req.state::<Mailer>() else {
        warn!("no mailer is configured; a message to {} was not sent", message.envelope_recipients().join(", "));
        return Ok(());
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
