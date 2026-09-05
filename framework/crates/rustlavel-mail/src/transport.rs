//! Where a message actually goes: a mail server, the log, or a directory.
//!
//! The transport is a boot-time decision read from configuration, so nothing
//! in an application changes between a developer's laptop and production —
//! only `mail.transport` does.

use crate::address::Address;
use crate::config::{MailConfig, TransportKind};
use crate::fake::Fake;
use crate::mailable::Mailable;
use crate::message::Message;
use crate::smtp::{SmtpClient, SmtpConfig};
use rustlavel_core::events::Event;
use rustlavel_core::{Config, Error, Result};
use rustlavel_view::Engine;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

/// A boxed future borrowed from the transport and the message it is sending.
///
/// The trait has to be dyn-compatible — the factory decides at boot which
/// transport an application has — and `async fn` in a trait is not.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Something that can deliver a message.
pub trait Transport: Send + Sync + 'static {
    /// The transport's name, as it appears in the `mail.sent` event.
    fn name(&self) -> &'static str;

    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<()>>;
}

/// Talks to a real SMTP server, one connection per message.
///
/// Connection reuse is not worth the complexity here: mail is not a hot path,
/// and a pooled SMTP connection that has gone stale fails in ways that are far
/// harder to explain than a reconnect.
pub struct SmtpTransport {
    settings: SmtpConfig,
}

impl SmtpTransport {
    pub fn new(settings: SmtpConfig) -> SmtpTransport {
        SmtpTransport { settings }
    }
}

impl Transport for SmtpTransport {
    fn name(&self) -> &'static str {
        "smtp"
    }

    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let from = message
                .sender()
                .ok_or_else(|| Error::msg("this message has no From address"))?
                .email()
                .to_string();
            let recipients = message.envelope_recipients();
            let data = message.to_smtp_data()?;

            let mut client = SmtpClient::connect(&self.settings).await?;
            let result = client.send_message(&from, &recipients, &data).await;
            client.quit().await;
            result
        })
    }
}

/// Writes the rendered message to the log.
///
/// The local development default: mail you can read without a mail server, an
/// inbox, or a network — and without the risk of a test mailing a customer.
pub struct LogTransport {
    /// Off for the fake, which records rather than reports.
    write: bool,
}

impl Default for LogTransport {
    fn default() -> Self {
        LogTransport::new()
    }
}

impl LogTransport {
    pub fn new() -> LogTransport {
        LogTransport { write: true }
    }

    /// A transport that does nothing at all, for [`crate::Mail::fake`].
    pub fn silent() -> LogTransport {
        LogTransport { write: false }
    }
}

impl Transport for LogTransport {
    fn name(&self) -> &'static str {
        "log"
    }

    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // Built even though the body is logged separately: it is the only
            // thing that proves the message would actually encode, and finding
            // that out in development is the point of this transport.
            let _ = message.to_mime()?;

            if self.write {
                // The body as written, not as encoded. `to_mime` produces
                // quoted-printable, which breaks every line over 76 columns
                // with a soft `=` — correct on the wire, and ruinous on a
                // terminal, because the break lands inside a long link and the
                // URL somebody copies is not the URL that was sent. A sign-in
                // or reset link is the main thing this transport exists to
                // deliver, so it has to survive being read.
                let body = message
                    .text_body()
                    .or_else(|| message.html_body())
                    .unwrap_or("(no body)");

                rustlavel_core::info!(
                    "mail to {}\nSubject: {}\n\n{body}",
                    message.envelope_recipients().join(", "),
                    message.subject_text()
                );
            }
            Ok(())
        })
    }
}

/// Writes one `.eml` per message under a directory.
///
/// `.eml` is what every mail client opens, so a designer can double-click the
/// file and see the message exactly as a recipient would — including the
/// attachments, which a log line cannot show.
pub struct FileTransport {
    directory: PathBuf,
}

impl FileTransport {
    pub fn new(directory: impl Into<PathBuf>) -> FileTransport {
        FileTransport { directory: directory.into() }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

impl Transport for FileTransport {
    fn name(&self) -> &'static str {
        "file"
    }

    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mime = message.to_mime()?;

            std::fs::create_dir_all(&self.directory).map_err(|error| {
                Error::msg(format!(
                    "cannot write mail to `{}`: {error}. Check mail.path.",
                    self.directory.display()
                ))
            })?;

            let path = self.directory.join(filename(message));
            std::fs::write(&path, mime).map_err(|error| {
                Error::msg(format!("cannot write `{}`: {error}", path.display()))
            })?;

            rustlavel_core::debug!("mail written to {}", path.display());
            Ok(())
        })
    }
}

/// A name a human can scan in a directory listing: when, and what about.
fn filename(message: &Message) -> String {
    let slug: String = message
        .subject_text()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    let slug = if slug.is_empty() { "message".to_string() } else { slug.chars().take(60).collect() };
    format!("{}-{slug}.eml", crate::encode::unique_token())
}

/// The application's handle on mail.
///
/// Cloning is cheap and shares one transport, so a handler can hold a `Mailer`
/// in context the same way it holds a database pool.
#[derive(Clone)]
pub struct Mailer {
    transport: Arc<dyn Transport>,
    from: Option<Address>,
    fake: Option<Arc<Fake>>,
}

impl Mailer {
    pub fn new(transport: impl Transport) -> Mailer {
        Mailer { transport: Arc::new(transport), from: None, fake: None }
    }

    /// Build the transport named in `mail.transport`.
    ///
    /// Deliberately does not connect: an application must boot even while the
    /// mail server is restarting.
    pub fn from_config(config: &Config) -> Result<Mailer> {
        Mailer::build(&MailConfig::from_app_config(config)?)
    }

    pub fn build(settings: &MailConfig) -> Result<Mailer> {
        let transport: Arc<dyn Transport> = match settings.transport {
            TransportKind::Smtp => Arc::new(SmtpTransport::new(settings.smtp())),
            TransportKind::Log => Arc::new(LogTransport::new()),
            TransportKind::File => Arc::new(FileTransport::new(settings.path.clone())),
        };

        Ok(Mailer { transport, from: settings.from()?, fake: None })
    }

    /// The sender used by any message that does not set its own.
    pub fn from(mut self, address: Address) -> Mailer {
        self.from = Some(address);
        self
    }

    /// Record instead of sending. This is `Mail::fake()`.
    pub fn faking(mut self, fake: Fake) -> Mailer {
        let fake = Arc::new(fake);
        self.fake = Some(Arc::clone(&fake));
        self.transport = fake;
        self
    }

    /// The recorder, when this mailer is faking.
    pub fn fake(&self) -> Option<&Arc<Fake>> {
        self.fake.as_ref()
    }

    pub fn transport_name(&self) -> &'static str {
        self.transport.name()
    }

    /// Send one message.
    pub async fn send(&self, message: Message) -> Result<()> {
        let message = match &self.from {
            Some(address) => message.default_from(address.clone()),
            None => message,
        };
        message.validate()?;

        let started = Instant::now();
        let result = self.transport.send(&message).await;
        let elapsed = started.elapsed();

        result.map_err(|error| {
            Error::msg(format!(
                "could not send `{}` to {}: {error}",
                message.subject_text(),
                message.envelope_recipients().join(", ")
            ))
        })?;

        // Recipients and subject, never the body and never a credential: this
        // event is written to a Telescope database an operator can read.
        Event::new("mail.sent")
            .with("to", message.envelope_recipients().join(", "))
            .with("subject", message.subject_text())
            .with("transport", self.transport.name())
            .took(elapsed)
            .dispatch();

        Ok(())
    }

    /// Render a [`Mailable`] and send it to one recipient.
    pub async fn send_mailable<M: Mailable + ?Sized>(
        &self,
        engine: &Engine,
        to: impl crate::address::IntoAddress,
        mailable: &M,
    ) -> Result<()> {
        let message = mailable.build(engine)?.to(to);
        self.send(message).await
    }

    /// Assert a message went to this address. Only meaningful while faking.
    #[track_caller]
    pub fn assert_sent_to(&self, address: &str) {
        self.recorder().assert_sent_to(address);
    }

    #[track_caller]
    pub fn assert_sent_times(&self, expected: usize) {
        self.recorder().assert_sent_times(expected);
    }

    #[track_caller]
    pub fn assert_nothing_sent(&self) {
        self.recorder().assert_nothing_sent();
    }

    #[track_caller]
    fn recorder(&self) -> &Fake {
        self.fake
            .as_deref()
            .expect("this mailer is not faking — build it with `Mail::fake()` before asserting")
    }
}

#[cfg(test)]
mod tests {

    /// The bug this exists to stop: a reset link logged as quoted-printable
    /// carries a soft `=` line break inside the token, so the URL somebody
    /// copies out of the terminal is not the URL that was issued — and the
    /// application, seeing a token that matches nothing, reports it as expired.
    #[tokio::test]
    async fn a_long_link_survives_being_logged() {
        let token = "436c02b608c95a2f59e2476756b44c7a9da324a72fbeb0785ee7bca3d8429b26";
        let url = format!("http://localhost:9001/reset-password/{token}");
        let message = Message::new()
            .from("a@example.com")
            .to("b@example.com")
            .subject("Reset your password")
            .text(format!("Use this link:\n\n{url}\n"));

        // What the wire carries: correct, and broken across lines.
        let mime = message.to_mime().unwrap();
        assert!(!mime.contains(&url), "the encoder no longer wraps; this test is moot");

        // What a person reads: the link, whole.
        let body = message.text_body().unwrap();
        assert!(body.contains(&url), "the logged body must carry the link unbroken");
        assert!(!body.contains("=\r\n"), "a soft break reached the readable body: {body}");
    }
    use super::*;
    use rustlavel_core::events;

    fn message() -> Message {
        Message::new()
            .from(("Rustlavel", "no-reply@example.com"))
            .to("ada@example.com")
            .subject("Your receipt")
            .text("Thank you.")
    }

    /// Each test writes its own directory: tests run concurrently, and a shared
    /// one would be read while another test was writing into it.
    fn fixture(test: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("rustlavel-mail-{test}"));
        let _ = std::fs::remove_dir_all(&directory);
        directory
    }

    #[tokio::test]
    async fn the_log_transport_renders_the_message_and_reports_success() {
        let mailer = Mailer::new(LogTransport::new());

        assert_eq!(mailer.transport_name(), "log");
        mailer.send(message()).await.unwrap();
    }

    #[tokio::test]
    async fn the_file_transport_writes_one_eml_per_message() {
        let directory = fixture("file-transport");
        let mailer = Mailer::new(FileTransport::new(&directory));

        mailer.send(message()).await.unwrap();
        mailer.send(message().subject("Second")).await.unwrap();

        let mut written: Vec<PathBuf> =
            std::fs::read_dir(&directory).unwrap().map(|e| e.unwrap().path()).collect();
        written.sort();

        assert_eq!(written.len(), 2);
        assert!(written.iter().all(|path| path.extension().is_some_and(|e| e == "eml")));
        assert!(
            written.iter().any(|path| path.to_string_lossy().contains("your-receipt")),
            "the subject should be in the filename: {written:?}"
        );

        let body = std::fs::read_to_string(&written[0]).unwrap();
        assert!(body.contains("From: Rustlavel <no-reply@example.com>\r\n"), "{body}");
        assert!(body.contains("Thank you."));

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn the_file_transport_explains_a_directory_it_cannot_create() {
        // A path under a regular file cannot become a directory.
        let blocker = fixture("file-blocked");
        std::fs::create_dir_all(&blocker).unwrap();
        let file = blocker.join("not-a-directory");
        std::fs::write(&file, b"x").unwrap();

        let mailer = Mailer::new(FileTransport::new(file.join("mail")));
        let error = mailer.send(message()).await.unwrap_err().to_string();

        assert!(error.contains("mail.path"), "{error}");
        let _ = std::fs::remove_dir_all(&blocker);
    }

    #[tokio::test]
    async fn the_configured_sender_fills_in_a_message_that_has_none() {
        let config = Config::new();
        config.set("mail.transport", "log");
        config.set("mail.from.address", "no-reply@example.com");
        config.set("mail.from.name", "Rustlavel");

        let mailer = Mailer::from_config(&config).unwrap();
        let sent = Message::new().to("ada@example.com").subject("Hi").text("hi");

        mailer.send(sent).await.unwrap();
    }

    #[tokio::test]
    async fn a_message_with_no_sender_at_all_is_refused_before_any_transport_runs() {
        let mailer = Mailer::new(LogTransport::new());

        let error = mailer
            .send(Message::new().to("ada@example.com").subject("Hi"))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("mail.from.address"), "{error}");
    }

    #[tokio::test]
    async fn a_sent_message_dispatches_an_event_without_the_body() {
        // The event bus is process-wide and other tests in this crate send mail
        // at the same time, so the subscriber keeps only the message this test
        // sent, identified by a subject nothing else uses.
        const SUBJECT: &str = "event-probe-2c1f";

        events::clear_subscribers();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Event>::new()));
        let sink = Arc::clone(&seen);
        events::subscribe(move |event: &Event| {
            let mine = event.field("subject").and_then(rustlavel_core::Json::as_str)
                == Some(SUBJECT);
            if event.kind == "mail.sent" && mine {
                sink.lock().unwrap().push(event.clone());
            }
        });

        Mailer::new(LogTransport::silent())
            .send(message().subject(SUBJECT).text("a secret body nobody should record"))
            .await
            .unwrap();

        let recorded = seen.lock().unwrap();
        let event = recorded.first().expect("a mail.sent event");

        assert_eq!(
            event.field("to").and_then(rustlavel_core::Json::as_str),
            Some("ada@example.com")
        );
        assert_eq!(
            event.field("transport").and_then(rustlavel_core::Json::as_str),
            Some("log")
        );
        assert!(event.duration.is_some());

        let rendered = format!("{:?}", event.fields);
        assert!(!rendered.contains("secret body"), "the body reached the event: {rendered}");

        drop(recorded);
        events::clear_subscribers();
    }

    #[tokio::test]
    async fn a_transport_failure_names_the_subject_and_the_recipients() {
        struct Broken;
        impl Transport for Broken {
            fn name(&self) -> &'static str {
                "broken"
            }
            fn send<'a>(&'a self, _message: &'a Message) -> BoxFuture<'a, Result<()>> {
                Box::pin(async { Err(Error::msg("the server hung up")) })
            }
        }

        let error = Mailer::new(Broken).send(message()).await.unwrap_err().to_string();

        assert!(error.contains("Your receipt"), "{error}");
        assert!(error.contains("ada@example.com"), "{error}");
        assert!(error.contains("the server hung up"), "{error}");
    }

    #[tokio::test]
    async fn the_smtp_transport_reports_a_server_it_cannot_reach() {
        // Nothing listens on port 1. The protocol itself is covered against a
        // scripted duplex server in `smtp::client`; what matters here is that
        // the transport's failure says where it was trying to go.
        let settings = SmtpConfig::new("127.0.0.1", 1);
        let mailer = Mailer::new(SmtpTransport::new(settings));

        assert_eq!(mailer.transport_name(), "smtp");

        let error = mailer.send(message()).await.unwrap_err().to_string();
        assert!(error.contains("127.0.0.1:1"), "{error}");
        assert!(error.contains("Your receipt"), "{error}");
    }

    #[tokio::test]
    async fn a_mailable_can_be_rendered_and_sent_in_one_call() {
        use rustlavel_view::EXTENSION;

        let root = fixture("send-mailable");
        let path = root.join(format!("mail/hello.{EXTENSION}"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "<p>Hello {{ name }}</p>").unwrap();

        struct Hello;
        impl Mailable for Hello {
            fn subject(&self) -> String {
                "Hello".into()
            }
            fn view(&self) -> &str {
                "mail.hello"
            }
            fn context(&self) -> rustlavel_view::Context {
                rustlavel_view::Context::new().with("name", "Ada")
            }
        }

        let mailer = crate::Mail::fake().from(Address::new("no-reply@example.com").unwrap());
        mailer.send_mailable(&Engine::new(&root), "ada@example.com", &Hello).await.unwrap();

        mailer.assert_sent_to("ada@example.com");
        let sent = &mailer.fake().unwrap().sent()[0];
        assert_eq!(sent.subject_text(), "Hello");
        assert_eq!(sent.html_body(), Some("<p>Hello Ada</p>"));
        assert_eq!(sent.text_body(), Some("Hello Ada"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_filename_survives_a_subject_that_is_not_a_filename() {
        let awkward = Message::new().subject("Re: your / order #7!!");
        let name = filename(&awkward);

        assert!(name.ends_with("-re-your-order-7.eml"), "{name}");
        assert!(!filename(&Message::new()).is_empty());
        assert!(filename(&Message::new()).contains("message"));
    }
}
