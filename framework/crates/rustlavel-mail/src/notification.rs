//! Notifications: one definition, several channels.
//!
//! A notification says what it is; the channels say how it looks in each
//! place. Adding a webhook to something that already sends mail is one more
//! method, not a second copy of the message.

use crate::message::Message;
use crate::transport::Mailer;
use rustlavel_client::Client;
use rustlavel_core::events::Event;
use rustlavel_core::{Error, Json, Result};
use std::collections::BTreeMap;

/// The channel names the framework knows.
pub mod channel {
    /// Email, through the application's [`crate::Mailer`].
    pub const MAIL: &str = "mail";
    /// A JSON payload the application stores itself.
    pub const DATABASE: &str = "database";
    /// A JSON POST to whatever URL the recipient is routed to.
    pub const WEBHOOK: &str = "webhook";
}

/// Where one recipient can be reached, per channel.
///
/// A route per channel rather than a `User` trait: the address a notification
/// goes to is not always a column on a model — a webhook URL belongs to a
/// team, an email address may come from a form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recipient {
    name: Option<String>,
    routes: BTreeMap<String, String>,
}

impl Recipient {
    pub fn new() -> Recipient {
        Recipient::default()
    }

    pub fn named(name: impl Into<String>) -> Recipient {
        Recipient { name: Some(name.into()), routes: BTreeMap::new() }
    }

    /// Where to email this recipient.
    pub fn mail(self, address: impl Into<String>) -> Recipient {
        self.route(channel::MAIL, address)
    }

    /// Where to POST this recipient's webhooks.
    pub fn webhook(self, url: impl Into<String>) -> Recipient {
        self.route(channel::WEBHOOK, url)
    }

    /// What identifies this recipient in the application's own store.
    pub fn database(self, key: impl Into<String>) -> Recipient {
        self.route(channel::DATABASE, key)
    }

    pub fn route(mut self, channel: &str, value: impl Into<String>) -> Recipient {
        self.routes.insert(channel.to_string(), value.into());
        self
    }

    pub fn route_for(&self, channel: &str) -> Option<&str> {
        self.routes.get(channel).map(String::as_str)
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// Something a recipient should be told about.
///
/// Only [`Notification::channels`] is required; a notification implements the
/// `to_*` method for each channel it declares, and the default implementation
/// of the others explains what is missing rather than sending something empty.
pub trait Notification: Send + Sync {
    /// A stable name for this notification, used in events and errors.
    fn name(&self) -> &str {
        "notification"
    }

    /// Which channels this notification goes out on.
    fn channels(&self) -> Vec<&'static str>;

    /// The email, with no recipient: [`Notifier`] addresses it.
    fn to_mail(&self, _recipient: &Recipient) -> Result<Message> {
        Err(Error::msg(format!(
            "`{}` lists the mail channel but does not implement `to_mail`",
            self.name()
        )))
    }

    /// The payload for the application to store.
    fn to_database(&self, _recipient: &Recipient) -> Result<Json> {
        Err(Error::msg(format!(
            "`{}` lists the database channel but does not implement `to_database`",
            self.name()
        )))
    }

    /// The JSON body posted to the recipient's webhook.
    fn to_webhook(&self, _recipient: &Recipient) -> Result<Json> {
        Err(Error::msg(format!(
            "`{}` lists the webhook channel but does not implement `to_webhook`",
            self.name()
        )))
    }
}

/// What happened on one channel.
#[derive(Debug)]
pub struct Delivery {
    pub channel: &'static str,
    /// The payload, for a channel that produces one — `database` returns the
    /// document the application is expected to insert.
    pub payload: Option<Json>,
    pub error: Option<Error>,
}

impl Delivery {
    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

/// The result of notifying one recipient over every declared channel.
#[derive(Debug)]
pub struct Notified {
    pub deliveries: Vec<Delivery>,
}

impl Notified {
    /// True when every channel succeeded.
    pub fn is_ok(&self) -> bool {
        self.deliveries.iter().all(Delivery::is_ok)
    }

    pub fn failures(&self) -> Vec<&Delivery> {
        self.deliveries.iter().filter(|delivery| !delivery.is_ok()).collect()
    }

    pub fn delivered(&self, channel: &str) -> bool {
        self.deliveries.iter().any(|d| d.channel == channel && d.is_ok())
    }

    /// The payload a channel produced, for the caller to store.
    pub fn payload(&self, channel: &str) -> Option<&Json> {
        self.deliveries
            .iter()
            .find(|delivery| delivery.channel == channel)
            .and_then(|delivery| delivery.payload.as_ref())
    }

    /// Collapse into a `Result`, naming every channel that failed.
    ///
    /// Optional on purpose: a caller usually wants the mail to have gone out
    /// even though the webhook endpoint is down, and only then decides whether
    /// that is worth failing the request over.
    pub fn into_result(self) -> Result<Notified> {
        let described: Vec<String> = self
            .deliveries
            .iter()
            .filter_map(|delivery| {
                delivery.error.as_ref().map(|error| format!("{}: {error}", delivery.channel))
            })
            .collect();

        if described.is_empty() {
            return Ok(self);
        }
        Err(Error::msg(format!("some channels failed — {}", described.join("; "))))
    }
}

/// Sends notifications over the channels they declare.
#[derive(Clone)]
pub struct Notifier {
    mailer: Mailer,
    client: Client,
}

impl Notifier {
    pub fn new(mailer: Mailer) -> Notifier {
        Notifier { mailer, client: Client::new() }
    }

    /// Use a particular HTTP client for the webhook channel — a faked one in
    /// tests, or one with retries configured in production.
    pub fn with_client(mut self, client: Client) -> Notifier {
        self.client = client;
        self
    }

    pub fn mailer(&self) -> &Mailer {
        &self.mailer
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Deliver a notification to one recipient over every channel it declares.
    ///
    /// One channel failing does not stop the others: a webhook endpoint being
    /// down is no reason for the recipient not to get their email. Every
    /// outcome comes back, and the caller decides what a failure is worth.
    pub async fn notify<N: Notification + ?Sized>(
        &self,
        recipient: &Recipient,
        notification: &N,
    ) -> Notified {
        let mut deliveries = Vec::new();

        for channel in notification.channels() {
            let outcome = match channel {
                channel::MAIL => self.deliver_mail(recipient, notification).await.map(|_| None),
                channel::DATABASE => notification.to_database(recipient).map(Some),
                channel::WEBHOOK => self.deliver_webhook(recipient, notification).await.map(Some),
                unknown => Err(Error::msg(format!(
                    "`{unknown}` is not a notification channel. Use one of: mail, database, \
                     webhook."
                ))),
            };

            let delivery = match outcome {
                Ok(payload) => Delivery { channel, payload, error: None },
                Err(error) => {
                    rustlavel_core::warn!(
                        "notification `{}` failed on the {channel} channel: {error}",
                        notification.name()
                    );
                    Delivery { channel, payload: None, error: Some(error) }
                }
            };

            Event::new("notification.sent")
                .with("notification", notification.name())
                .with("channel", channel)
                .with("ok", delivery.is_ok())
                .dispatch();

            deliveries.push(delivery);
        }

        Notified { deliveries }
    }

    async fn deliver_mail<N: Notification + ?Sized>(
        &self,
        recipient: &Recipient,
        notification: &N,
    ) -> Result<()> {
        let address = recipient.route_for(channel::MAIL).ok_or_else(|| {
            Error::msg(format!(
                "`{}` goes out over mail, but this recipient has no mail route. Add one with \
                 `Recipient::mail(...)`.",
                notification.name()
            ))
        })?;

        let message = match recipient.name() {
            Some(name) => notification.to_mail(recipient)?.to((name, address)),
            None => notification.to_mail(recipient)?.to(address),
        };

        self.mailer.send(message).await
    }

    async fn deliver_webhook<N: Notification + ?Sized>(
        &self,
        recipient: &Recipient,
        notification: &N,
    ) -> Result<Json> {
        let url = recipient.route_for(channel::WEBHOOK).ok_or_else(|| {
            Error::msg(format!(
                "`{}` goes out over webhook, but this recipient has no webhook route. Add one \
                 with `Recipient::webhook(...)`.",
                notification.name()
            ))
        })?;

        let payload = notification.to_webhook(recipient)?;
        self.client
            .post(url)
            .json(payload.clone())
            .send()
            .await?
            .error_for_status()
            .map_err(|error| Error::msg(format!("the webhook at {url} failed: {error}")))?;

        Ok(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mail;
    use rustlavel_client::{Fake as HttpFake, FakeResponse};

    struct InvoicePaid {
        amount: i64,
    }

    impl Notification for InvoicePaid {
        fn name(&self) -> &str {
            "invoice-paid"
        }

        fn channels(&self) -> Vec<&'static str> {
            vec![channel::MAIL, channel::DATABASE, channel::WEBHOOK]
        }

        fn to_mail(&self, _recipient: &Recipient) -> Result<Message> {
            Ok(Message::new()
                .from("billing@example.com")
                .subject("Invoice paid")
                .text(format!("We received {} cents. Thank you.", self.amount)))
        }

        fn to_database(&self, _recipient: &Recipient) -> Result<Json> {
            Ok(Json::object([
                ("type", Json::from("invoice-paid")),
                ("amount", Json::from(self.amount)),
            ]))
        }

        fn to_webhook(&self, _recipient: &Recipient) -> Result<Json> {
            Ok(Json::object([("event", Json::from("invoice.paid"))]))
        }
    }

    fn recipient() -> Recipient {
        Recipient::named("Ada Lovelace")
            .mail("ada@example.com")
            .webhook("https://hooks.example.com/ada")
    }

    fn notifier(http: HttpFake) -> Notifier {
        Notifier::new(Mail::fake()).with_client(Client::new().faking(http))
    }

    #[tokio::test]
    async fn one_definition_fans_out_to_every_channel_it_declares() {
        let notifier = notifier(HttpFake::new().fallback(FakeResponse::text("ok")));

        let result = notifier.notify(&recipient(), &InvoicePaid { amount: 4_200 }).await;

        assert!(result.is_ok(), "{:?}", result.failures());
        assert_eq!(result.deliveries.len(), 3);
        assert!(result.delivered(channel::MAIL));

        // The database channel hands back the document to store.
        let stored = result.payload(channel::DATABASE).unwrap();
        assert_eq!(stored.get("amount").unwrap().as_i64(), Some(4_200));

        notifier.mailer().assert_sent_to("ada@example.com");
        let sent = &notifier.mailer().fake().unwrap().sent()[0];
        assert_eq!(sent.subject_text(), "Invoice paid");
        assert_eq!(sent.to_addresses()[0].to_header(), "Ada Lovelace <ada@example.com>");

        notifier.client().fake().unwrap().assert_sent("hooks.example.com/ada");
        let posted = notifier.client().fake().unwrap().recorded()[0].json().unwrap();
        assert_eq!(posted.get("event").unwrap().as_str(), Some("invoice.paid"));

        assert!(result.into_result().is_ok());
    }

    #[tokio::test]
    async fn one_channel_failing_does_not_stop_the_others() {
        // The webhook endpoint is down; the email must still go out.
        let notifier = notifier(HttpFake::new().fallback(FakeResponse::text("boom").status(500)));

        let result = notifier.notify(&recipient(), &InvoicePaid { amount: 100 }).await;

        assert!(!result.is_ok());
        assert!(result.delivered(channel::MAIL), "the mail should have been sent anyway");
        assert!(result.delivered(channel::DATABASE));
        assert!(!result.delivered(channel::WEBHOOK));

        let failures = result.failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].channel, channel::WEBHOOK);

        notifier.mailer().assert_sent_times(1);

        let error = result.into_result().unwrap_err().to_string();
        assert!(error.contains("webhook"), "{error}");
        assert!(error.contains("500"), "{error}");
    }

    #[tokio::test]
    async fn a_recipient_with_no_route_for_a_channel_is_told_which_one_is_missing() {
        let notifier = notifier(HttpFake::new().fallback(FakeResponse::text("ok")));
        let unreachable = Recipient::named("Nobody");

        let result = notifier.notify(&unreachable, &InvoicePaid { amount: 1 }).await;

        let error = result.into_result().unwrap_err().to_string();
        assert!(error.contains("Recipient::mail"), "{error}");
        assert!(error.contains("Recipient::webhook"), "{error}");
    }

    #[tokio::test]
    async fn a_channel_a_notification_forgot_to_implement_says_so() {
        struct Half;
        impl Notification for Half {
            fn name(&self) -> &str {
                "half-written"
            }
            fn channels(&self) -> Vec<&'static str> {
                vec![channel::DATABASE]
            }
        }

        let result = notifier(HttpFake::new()).notify(&recipient(), &Half).await;
        let error = result.into_result().unwrap_err().to_string();

        assert!(error.contains("does not implement `to_database`"), "{error}");
    }

    #[tokio::test]
    async fn an_unknown_channel_lists_the_ones_that_exist() {
        struct Odd;
        impl Notification for Odd {
            fn channels(&self) -> Vec<&'static str> {
                vec!["carrier-pigeon"]
            }
        }

        let result = notifier(HttpFake::new()).notify(&recipient(), &Odd).await;
        let error = result.into_result().unwrap_err().to_string();

        assert!(error.contains("mail, database, webhook"), "{error}");
    }

    #[test]
    fn routes_are_per_channel() {
        let recipient = recipient().database("user:7");

        assert_eq!(recipient.route_for(channel::MAIL), Some("ada@example.com"));
        assert_eq!(recipient.route_for(channel::DATABASE), Some("user:7"));
        assert_eq!(recipient.route_for("sms"), None);
        assert_eq!(recipient.name(), Some("Ada Lovelace"));
    }
}
