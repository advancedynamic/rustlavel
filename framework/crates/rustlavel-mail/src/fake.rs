//! Faking outbound mail.
//!
//! This is `Mail::fake()`. A test must never put a message on a real mail
//! server: it is slow, it needs a server nobody has, and the day the address
//! in a fixture is a real one it emails a real person.

use crate::message::Message;
use crate::transport::{BoxFuture, Transport};
use rustlavel_core::Result;
use std::sync::Mutex;

/// A transport that records instead of sending.
#[derive(Default)]
pub struct Fake {
    sent: Mutex<Vec<Message>>,
}

impl Fake {
    pub fn new() -> Fake {
        Fake::default()
    }

    /// Every message that would have been sent, in order.
    pub fn sent(&self) -> Vec<Message> {
        self.messages().clone()
    }

    /// The messages addressed to one recipient — To, Cc or Bcc.
    pub fn sent_to(&self, address: &str) -> Vec<Message> {
        self.messages()
            .iter()
            .filter(|message| {
                message.envelope_recipients().iter().any(|recipient| recipient == address)
            })
            .cloned()
            .collect()
    }

    pub fn count(&self) -> usize {
        self.messages().len()
    }

    /// Forget everything recorded so far.
    pub fn clear(&self) {
        self.messages().clear();
    }

    #[track_caller]
    pub fn assert_sent_to(&self, address: &str) {
        assert!(
            !self.sent_to(address).is_empty(),
            "expected a message to {address}; {}",
            self.summary()
        );
    }

    #[track_caller]
    pub fn assert_not_sent_to(&self, address: &str) {
        assert!(
            self.sent_to(address).is_empty(),
            "did not expect a message to {address}; {}",
            self.summary()
        );
    }

    #[track_caller]
    pub fn assert_sent_times(&self, expected: usize) {
        assert_eq!(self.count(), expected, "unexpected number of messages; {}", self.summary());
    }

    #[track_caller]
    pub fn assert_nothing_sent(&self) {
        assert!(self.count() == 0, "expected no mail at all; {}", self.summary());
    }

    /// What a failed assertion prints: enough to see what actually happened
    /// without printing whole message bodies into a test log.
    fn summary(&self) -> String {
        let messages = self.messages();
        if messages.is_empty() {
            return "no messages were sent".to_string();
        }
        let lines: Vec<String> = messages
            .iter()
            .map(|message| {
                format!(
                    "`{}` to {}",
                    message.subject_text(),
                    message.envelope_recipients().join(", ")
                )
            })
            .collect();
        format!("sent: {}", lines.join("; "))
    }

    fn messages(&self) -> std::sync::MutexGuard<'_, Vec<Message>> {
        self.sent.lock().expect("mail fake lock poisoned")
    }
}

impl Transport for Fake {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // Rendered even though nothing is sent: a message that cannot be
            // turned into MIME must fail in the test, not in production.
            message.to_mime()?;
            self.messages().push(message.clone());
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mail;
    use crate::message::Attachment;

    fn message() -> Message {
        Message::new()
            .from("no-reply@example.com")
            .to("ada@example.com")
            .subject("Your receipt")
            .text("Thank you.")
            .html("<p>Thank you.</p>")
    }

    #[tokio::test]
    async fn nothing_is_sent_and_everything_is_recorded() {
        let mailer = Mail::fake();

        mailer.send(message()).await.unwrap();
        mailer.send(message().to("grace@example.com").subject("Second")).await.unwrap();

        let fake = mailer.fake().expect("a faking mailer");
        fake.assert_sent_to("ada@example.com");
        fake.assert_sent_to("grace@example.com");
        fake.assert_not_sent_to("nobody@example.com");
        fake.assert_sent_times(2);

        assert_eq!(mailer.transport_name(), "fake");
    }

    #[tokio::test]
    async fn a_test_can_read_the_subject_and_the_bodies_it_recorded() {
        let mailer = Mail::fake();
        mailer
            .send(message().attach(Attachment::new("receipt.pdf", b"%PDF".to_vec())))
            .await
            .unwrap();

        let recorded = mailer.fake().unwrap().sent();
        let sent = &recorded[0];

        assert_eq!(sent.subject_text(), "Your receipt");
        assert_eq!(sent.text_body(), Some("Thank you."));
        assert_eq!(sent.html_body(), Some("<p>Thank you.</p>"));
        assert_eq!(sent.attachments()[0].filename, "receipt.pdf");
        // The full MIME is available too, for a test that wants to be sure.
        assert!(sent.to_mime().unwrap().contains("multipart/mixed"));
    }

    #[tokio::test]
    async fn assertions_are_also_reachable_on_the_mailer_itself() {
        let mailer = Mail::fake();
        mailer.assert_nothing_sent();

        mailer.send(message()).await.unwrap();

        mailer.assert_sent_to("ada@example.com");
        mailer.assert_sent_times(1);
    }

    #[tokio::test]
    async fn a_message_that_cannot_be_rendered_fails_in_the_test() {
        let mailer = Mail::fake();

        let error = mailer
            .send(Message::new().from("no-reply@example.com").to("nonsense"))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("nonsense"), "{error}");
        mailer.assert_nothing_sent();
    }

    #[tokio::test]
    async fn a_failed_assertion_says_what_was_actually_sent() {
        let fake = Fake::new();
        fake.send(&message()).await.unwrap();

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fake.assert_sent_to("nobody@example.com")
        }))
        .unwrap_err();

        let text = panicked
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| "not a string".to_string());

        assert!(text.contains("nobody@example.com"), "{text}");
        assert!(text.contains("`Your receipt` to ada@example.com"), "{text}");
    }

    #[tokio::test]
    async fn recorded_messages_can_be_cleared_between_phases_of_a_test() {
        let fake = Fake::new();
        fake.send(&message()).await.unwrap();
        fake.assert_sent_times(1);

        fake.clear();
        fake.assert_nothing_sent();
    }
}
