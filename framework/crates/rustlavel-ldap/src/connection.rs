//! One connection to a directory: TCP, optionally encrypted, and the loop that
//! matches an answer to the question that asked it.
//!
//! # There is no connection pool here
//!
//! Deliberately, and it is worth saying out loud rather than leaving as an
//! omission. An LDAP connection is not stateless the way an HTTP one is: it
//! carries an *authorization identity*, set by the last successful bind. Two
//! callers sharing a pooled connection would be sharing whoever bound last,
//! and a failed bind leaves a connection anonymous rather than closed — so a
//! pool that handed that connection back out would hand out an anonymous
//! session that used to be an administrator's.
//!
//! Authentication is also not a hot path. One connection per attempt, closed
//! afterwards, is what [`crate::directory`] does, and it is both simpler and
//! the only version that is obviously correct. A pool for *search* traffic
//! bound as a single service account is a reasonable thing to want later; it
//! would key on the bound DN, and it is not this.
//!
//! # Encryption
//!
//! Three ways to get there, matching what deployments actually look like:
//!
//! * [`Encryption::Ldaps`] — TLS from the first byte, port 636. Never
//!   standardised, universally deployed.
//! * [`Encryption::StartTls`] — connect in the clear on port 389, then send
//!   the extended request from RFC 4511 §4.14 and upgrade. If the directory
//!   says no, the connection fails; it never continues in the clear.
//! * [`Encryption::None`] — no encryption. A simple bind over this is refused
//!   unless the caller has said, in as many words, that it is acceptable.

use crate::ber;
use crate::protocol::{
    LdapMessage, LdapResult, Operation, ProtocolOp, SearchOutcome, SearchRequest,
};
use rustlavel_core::{Error, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How a connection gets encrypted, if it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encryption {
    /// Plain TCP. Everything, the password included, is on the wire in the
    /// clear — see [`LdapConfig::allow_plaintext_password`] for the one way to
    /// send a bind over it.
    None,
    /// Connect in the clear, then upgrade with the StartTLS extended operation.
    /// The default for port 389.
    StartTls,
    /// TLS from the first byte. The default for port 636.
    #[default]
    Ldaps,
}

/// Where the directory is and how much to insist on before talking to it.
///
/// Holds no credentials, which is why it can derive `Debug`: the bind DN and
/// password live in [`crate::directory::Directory`], which cannot.
#[derive(Debug, Clone)]
pub struct LdapConfig {
    pub host: String,
    pub port: u16,
    pub encryption: Encryption,
    /// Whether the server's certificate has to chain to a trusted root and
    /// name the host. Off is for a laboratory and nowhere else.
    pub verify_certificate: bool,
    /// Whether a simple bind may go over an unencrypted connection.
    ///
    /// False by default, and the error when it fires says why. A simple bind
    /// puts the password on the wire verbatim; this is the most common LDAP
    /// deployment mistake there is, and it is invisible from the outside
    /// because everything works.
    pub allow_plaintext_password: bool,
    pub connect_timeout: Duration,
    /// How long to wait for one response.
    pub timeout: Duration,
    /// The largest message this client will buffer.
    ///
    /// Enforced from the length header, before anything is read, so a directory
    /// cannot make a client allocate by announcing a size it never sends.
    pub max_message_size: usize,
}

impl LdapConfig {
    /// Parse `ldap://host:port` or `ldaps://host:port`.
    ///
    /// `ldaps://` implies port 636 and TLS from the first byte. `ldap://`
    /// implies port 389 and, by default, StartTLS — because a URL is not a
    /// security decision and defaulting to the encrypted form of the same port
    /// is the version that fails loudly rather than leaking quietly. A caller
    /// who genuinely wants plain text asks with [`LdapConfig::plaintext`].
    pub fn parse(url: &str) -> Result<LdapConfig> {
        let (scheme, rest) = url.split_once("://").ok_or_else(|| {
            Error::msg(format!(
                "`{url}` is not an LDAP URL. It should start with `ldap://` or `ldaps://` — the \
                 scheme is what decides whether the password is encrypted, so it is not optional."
            ))
        })?;

        let (encryption, default_port) = match scheme.to_ascii_lowercase().as_str() {
            "ldap" => (Encryption::StartTls, 389),
            "ldaps" => (Encryption::Ldaps, 636),
            other => {
                return Err(Error::msg(format!(
                    "`{other}` is not an LDAP scheme. Use `ldaps://` for TLS from the first byte, \
                     or `ldap://` for port 389 with StartTLS."
                )));
            }
        };

        // Anything after the authority is a search base and a filter in
        // RFC 4516 terms. This package takes those as arguments rather than
        // buried in a URL, so they are refused here instead of ignored.
        let authority = rest.trim_end_matches('/');
        if authority.contains('/') {
            return Err(Error::msg(format!(
                "`{url}` has a path. This package takes the search base and filter as arguments, \
                 not as part of the URL — give it just `{scheme}://host:port`."
            )));
        }
        if authority.is_empty() {
            return Err(Error::msg(format!("`{url}` names no host")));
        }

        let (host, port) = split_authority(authority, default_port)?;

        Ok(LdapConfig {
            host,
            port,
            encryption,
            verify_certificate: true,
            allow_plaintext_password: false,
            connect_timeout: Duration::from_secs(10),
            timeout: Duration::from_secs(30),
            max_message_size: 8 * 1024 * 1024,
        })
    }

    /// Do not encrypt at all.
    ///
    /// Separate from [`LdapConfig::allow_plaintext_password`] on purpose: this
    /// says "no TLS", that one says "and send the password anyway". A read-only
    /// anonymous search over plain TCP is a perfectly ordinary thing to do, and
    /// needs only the first.
    pub fn plaintext(mut self) -> LdapConfig {
        self.encryption = Encryption::None;
        self
    }

    pub fn start_tls(mut self) -> LdapConfig {
        self.encryption = Encryption::StartTls;
        self
    }

    pub fn ldaps(mut self) -> LdapConfig {
        self.encryption = Encryption::Ldaps;
        self
    }

    pub fn port(mut self, port: u16) -> LdapConfig {
        self.port = port;
        self
    }

    /// Send a simple bind even though the connection is not encrypted.
    ///
    /// The named-in-full opt-in the security rule asks for. It exists because a
    /// test container on loopback is a real case; it should not appear in
    /// anything that talks to a directory over a network.
    pub fn allow_plaintext_password(mut self) -> LdapConfig {
        self.allow_plaintext_password = true;
        self
    }

    /// Believe whatever certificate the directory presents.
    ///
    /// This turns TLS into protection against a passive listener and nothing
    /// else: an active attacker simply presents a certificate of their own. The
    /// name is long because the call site is where somebody will read it.
    pub fn dangerously_accept_any_certificate(mut self) -> LdapConfig {
        self.verify_certificate = false;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> LdapConfig {
        self.timeout = timeout;
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> LdapConfig {
        self.connect_timeout = timeout;
        self
    }

    pub fn max_message_size(mut self, bytes: usize) -> LdapConfig {
        self.max_message_size = bytes;
        self
    }

    pub fn address(&self) -> String {
        if self.host.contains(':') {
            return format!("[{}]:{}", self.host, self.port);
        }
        format!("{}:{}", self.host, self.port)
    }
}

/// `host`, `host:port`, `[v6]` or `[v6]:port`.
fn split_authority(authority: &str, default_port: u16) -> Result<(String, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(|| {
            Error::msg(format!("`{authority}` opens a bracketed IPv6 address but never closes it"))
        })?;
        let port = match tail.strip_prefix(':') {
            Some(port) => parse_port(port)?,
            None if tail.is_empty() => default_port,
            None => return Err(Error::msg(format!("`{authority}` has junk after the address"))),
        };
        return Ok((host.to_string(), port));
    }

    match authority.rsplit_once(':') {
        Some((host, port)) => Ok((host.to_string(), parse_port(port)?)),
        None => Ok((authority.to_string(), default_port)),
    }
}

fn parse_port(port: &str) -> Result<u16> {
    port.parse().map_err(|_| Error::msg(format!("`{port}` is not a port number")))
}

/// A socket that may or may not have been upgraded.
///
/// An enum rather than a boxed trait object: there are exactly two cases, both
/// known at compile time. `Upgrading` exists for the instant during StartTLS
/// when the plain socket has been taken out and the encrypted one is not yet
/// in — a real placeholder, so a bug there reports itself instead of hanging on
/// a dead file descriptor.
enum LdapStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
    Upgrading,
}

impl LdapStream {
    fn is_encrypted(&self) -> bool {
        matches!(self, LdapStream::Tls(_))
    }

    async fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            LdapStream::Plain(stream) => stream.write_all(bytes).await,
            LdapStream::Tls(stream) => stream.write_all(bytes).await,
            LdapStream::Upgrading => Err(mid_upgrade()),
        }
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        match self {
            LdapStream::Plain(stream) => stream.flush().await,
            LdapStream::Tls(stream) => stream.flush().await,
            LdapStream::Upgrading => Err(mid_upgrade()),
        }
    }

    async fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            LdapStream::Plain(stream) => stream.read(buffer).await,
            LdapStream::Tls(stream) => stream.read(buffer).await,
            LdapStream::Upgrading => Err(mid_upgrade()),
        }
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        match self {
            LdapStream::Plain(stream) => stream.shutdown().await,
            LdapStream::Tls(stream) => stream.shutdown().await,
            LdapStream::Upgrading => Ok(()),
        }
    }
}

fn mid_upgrade() -> std::io::Error {
    std::io::Error::other("the connection was left mid-upgrade; this is a bug in this client")
}

/// A live connection to a directory.
///
/// **No `Debug`.** The bytes it writes hold a password whenever a simple bind
/// goes out, and a type that can be printed is a type that ends up in a log.
pub struct LdapConnection {
    stream: LdapStream,
    config: LdapConfig,
    /// Bytes read but not yet framed into a message. LDAP messages do not align
    /// with TCP reads in either direction.
    buffer: Vec<u8>,
    next_id: i64,
    bound_as: Option<String>,
}

impl LdapConnection {
    /// Open a connection and get it encrypted, if the configuration says so.
    pub async fn connect(config: &LdapConfig) -> Result<LdapConnection> {
        let address = config.address();

        let tcp = tokio::time::timeout(config.connect_timeout, TcpStream::connect(&address))
            .await
            .map_err(|_| {
                Error::msg(format!(
                    "connecting to the directory at {address} timed out after {:?}",
                    config.connect_timeout
                ))
            })?
            .map_err(|error| {
                Error::msg(format!("cannot reach the directory at {address}: {error}"))
            })?;

        // Every message is a request followed by a wait, so Nagle only ever
        // adds latency here.
        let _ = tcp.set_nodelay(true);

        let mut connection = LdapConnection {
            stream: LdapStream::Plain(tcp),
            config: config.clone(),
            buffer: Vec::with_capacity(4096),
            next_id: 1,
            bound_as: None,
        };

        match config.encryption {
            Encryption::None => {}
            Encryption::Ldaps => connection.upgrade().await?,
            Encryption::StartTls => connection.start_tls().await?,
        }

        Ok(connection)
    }

    /// Whether this connection is encrypted right now.
    pub fn is_encrypted(&self) -> bool {
        self.stream.is_encrypted()
    }

    /// The DN of the last successful non-anonymous bind, if there was one.
    pub fn bound_as(&self) -> Option<&str> {
        self.bound_as.as_deref()
    }

    pub fn config(&self) -> &LdapConfig {
        &self.config
    }

    /// A simple bind: name plus password.
    ///
    /// Refuses two things before writing a byte. An empty password, because
    /// LDAP would answer it with `success` — see [`Operation::simple_bind`].
    /// And an unencrypted connection, because a simple bind puts the password
    /// on the wire verbatim and there is no way to un-send it.
    pub async fn bind(&mut self, dn: &str, password: &str) -> Result<LdapResult> {
        let operation = Operation::simple_bind(dn, password)?;
        let result = self.bind_with(operation).await?;
        if result.is_success() {
            self.bound_as = Some(dn.to_string());
        }
        Ok(result)
    }

    /// An anonymous bind — no name and no password, and no authentication.
    ///
    /// Spelled out so it cannot be arrived at by leaving a field empty.
    pub async fn bind_anonymous(&mut self) -> Result<LdapResult> {
        let result = self.bind_with(Operation::anonymous_bind()).await?;
        if result.is_success() {
            self.bound_as = None;
        }
        Ok(result)
    }

    async fn bind_with(&mut self, operation: Operation) -> Result<LdapResult> {
        let id = self.send(&operation).await?;
        let message = self.receive(id).await?;

        match message.op {
            ProtocolOp::BindResponse { result, .. } => Ok(result),
            other => Err(Error::msg(format!(
                "asked the directory to bind and it answered with {}",
                other.name()
            ))),
        }
    }

    /// Run a search to completion, collecting every entry.
    ///
    /// Entries are collected rather than streamed because the searches this
    /// package exists for return one entry. A search that could return
    /// thousands wants a paged control and an iterator, and should not pretend
    /// this is that.
    pub async fn search(&mut self, request: &SearchRequest) -> Result<SearchOutcome> {
        let operation = Operation::search(request);
        let id = self.send(&operation).await?;

        let mut entries = Vec::new();
        let mut referrals = Vec::new();

        loop {
            let message = self.receive(id).await?;
            match message.op {
                ProtocolOp::SearchResultEntry(entry) => entries.push(entry),
                ProtocolOp::SearchResultReference(uris) => referrals.push(uris),
                ProtocolOp::SearchResultDone(result) => {
                    return Ok(SearchOutcome { entries, referrals, result });
                }
                // A directory may send these in the middle of a search when a
                // control asks for them. Nothing here asked, so ignore rather
                // than fail: an unexpected extra message is not a wrong answer.
                ProtocolOp::IntermediateResponse => {}
                other => {
                    return Err(Error::msg(format!(
                        "the directory sent {} in the middle of a search",
                        other.name()
                    )));
                }
            }
        }
    }

    /// Say goodbye and close.
    ///
    /// `UnbindRequest` has no response — it means "I am closing this", not
    /// "please reply" — so this writes and shuts down without waiting. Consumes
    /// the connection, because there is nothing left to do with it.
    pub async fn unbind(mut self) -> Result<()> {
        let operation = Operation::unbind();
        let id = self.next_message_id()?;
        let bytes = operation.encode(id);

        // Best effort: the point of an unbind is politeness to the directory's
        // connection table, and a socket that has already gone is not a failure
        // the caller can do anything about.
        let _ = self.stream.write_all(&bytes).await;
        let _ = self.stream.flush().await;
        let _ = self.stream.shutdown().await;
        Ok(())
    }

    /// Write one operation and return the message id it went out under.
    ///
    /// This is where the transport rule is enforced, because it is the single
    /// place every request passes through.
    pub async fn send(&mut self, operation: &Operation) -> Result<i64> {
        if operation.carries_password()
            && !self.is_encrypted()
            && !self.config.allow_plaintext_password
        {
            return Err(Error::msg(format!(
                "refusing to send a simple bind to {} over an unencrypted connection: the \
                 password would go out in the clear, readable by anything on the path. Use \
                 `ldaps://` (TLS from the first byte) or StartTLS. If this really is a local test \
                 directory, say so with `LdapConfig::allow_plaintext_password`.",
                self.config.address()
            )));
        }

        let id = self.next_message_id()?;
        let bytes = operation.encode(id);

        tokio::time::timeout(self.config.timeout, self.stream.write_all(&bytes))
            .await
            .map_err(|_| self.timed_out(operation.kind()))?
            .map_err(|error| {
                Error::msg(format!(
                    "writing the {} to {} failed: {error}",
                    operation.kind(),
                    self.config.address()
                ))
            })?;

        self.stream.flush().await.map_err(Error::Io)?;
        Ok(id)
    }

    /// Read the response to a particular message id.
    ///
    /// LDAP lets several operations be outstanding at once, and the id is the
    /// only thing that says which answer belongs to which question. This client
    /// asks one thing at a time, so anything else arriving means either the
    /// directory is confused or something is injecting messages — and in both
    /// cases carrying on with the wrong answer is worse than stopping.
    pub async fn receive(&mut self, expected_id: i64) -> Result<LdapMessage> {
        let message = self.read_message().await?;

        if message.id == expected_id {
            return Ok(message);
        }

        // Message id zero is an unsolicited notification (RFC 4511 §4.4): the
        // directory speaking without being asked, which in practice is the
        // Notice of Disconnection immediately before it hangs up.
        if message.id == 0 {
            let reason = message
                .op
                .result()
                .map(|result| format!("{}: {}", result.code, result.diagnostic))
                .unwrap_or_else(|| message.op.name().to_string());
            return Err(Error::msg(format!(
                "the directory at {} sent an unsolicited notification and is closing the \
                 connection — {reason}",
                self.config.address()
            )));
        }

        Err(Error::msg(format!(
            "the directory answered message {} while message {expected_id} was the one \
             outstanding. Refusing to treat one operation's answer as another's.",
            message.id
        )))
    }

    async fn read_message(&mut self) -> Result<LdapMessage> {
        loop {
            if let Some(size) = ber::element_size(&self.buffer, self.config.max_message_size)? {
                let bytes: Vec<u8> = self.buffer.drain(..size).collect();
                return LdapMessage::parse(&bytes);
            }

            let mut chunk = [0u8; 8192];
            let read = tokio::time::timeout(self.config.timeout, self.stream.read(&mut chunk))
                .await
                .map_err(|_| self.timed_out("a response"))?
                .map_err(|error| {
                    Error::msg(format!(
                        "reading from the directory at {} failed: {error}",
                        self.config.address()
                    ))
                })?;

            if read == 0 {
                return Err(Error::msg(format!(
                    "the directory at {} closed the connection with {} bytes of a message \
                     outstanding",
                    self.config.address(),
                    self.buffer.len()
                )));
            }

            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }

    fn timed_out(&self, what: &str) -> Error {
        Error::msg(format!(
            "the directory at {} did not finish {what} within {:?}",
            self.config.address(),
            self.config.timeout
        ))
    }

    fn next_message_id(&mut self) -> Result<i64> {
        // Zero is reserved for unsolicited notifications, so ids start at one.
        // Running out is not realistic on one connection, but silently wrapping
        // would make two operations share an id, which is the one thing the id
        // exists to prevent.
        if self.next_id > i64::from(i32::MAX) {
            return Err(Error::msg(
                "this connection has used every message id; open a new one",
            ));
        }
        let id = self.next_id;
        self.next_id += 1;
        Ok(id)
    }

    /// Ask the directory to upgrade the connection, then do it.
    async fn start_tls(&mut self) -> Result<()> {
        let operation = Operation::start_tls();
        let id = self.send(&operation).await?;
        let message = self.receive(id).await?;

        let result = match message.op {
            ProtocolOp::ExtendedResponse { result, .. } => result,
            other => {
                return Err(Error::msg(format!(
                    "asked for StartTLS and the directory answered with {}",
                    other.name()
                )));
            }
        };

        if !result.is_success() {
            return Err(Error::msg(format!(
                "the directory at {} refused StartTLS: {}{}. It is not configured for TLS on \
                 this port. Connecting anyway would send the password in the clear, so this \
                 stops here rather than downgrading.",
                self.config.address(),
                result.code,
                match result.diagnostic.as_str() {
                    "" => String::new(),
                    diagnostic => format!(" — {diagnostic}"),
                }
            )));
        }

        // RFC 4511 §4.14.3.1: nothing may be pending across the upgrade. There
        // is nothing pending — this client asks one question at a time — but if
        // bytes did arrive between the response and here, they arrived before
        // the handshake and are therefore unauthenticated. Refuse them.
        if !self.buffer.is_empty() {
            return Err(Error::msg(
                "the directory sent data after agreeing to StartTLS but before the handshake. \
                 Those bytes are unauthenticated, so the connection is abandoned rather than \
                 trusted.",
            ));
        }

        self.upgrade().await
    }

    /// Run the TLS handshake over whatever socket is there now.
    async fn upgrade(&mut self) -> Result<()> {
        let plain = match std::mem::replace(&mut self.stream, LdapStream::Upgrading) {
            LdapStream::Plain(stream) => stream,
            LdapStream::Tls(stream) => {
                self.stream = LdapStream::Tls(stream);
                return Err(Error::msg("this connection is already encrypted"));
            }
            LdapStream::Upgrading => return Err(Error::msg(mid_upgrade().to_string())),
        };

        let connector = tokio_rustls::TlsConnector::from(client_config(&self.config));

        // An IP address is a valid server name for rustls, and one that no
        // public certificate carries — which is why connecting to 127.0.0.1
        // with verification on fails, correctly, and the error says so.
        let name = rustls::pki_types::ServerName::try_from(self.config.host.clone())
            .map_err(|_| {
                Error::msg(format!("`{}` is not a valid TLS server name", self.config.host))
            })?;

        let tls = tokio::time::timeout(self.config.connect_timeout, connector.connect(name, plain))
            .await
            .map_err(|_| {
                Error::msg(format!(
                    "the TLS handshake with {} timed out",
                    self.config.address()
                ))
            })?
            .map_err(|error| {
                Error::msg(format!(
                    "the TLS handshake with {} failed: {error}. If this is a development \
                     directory with a self-signed certificate, say so explicitly with \
                     `dangerously_accept_any_certificate` rather than turning encryption off.",
                    self.config.address()
                ))
            })?;

        self.stream = LdapStream::Tls(Box::new(tls));
        Ok(())
    }
}

/// The rustls configuration a connection implies.
///
/// Trust anchors come from webpki-roots rather than the OS store, so behaviour
/// is identical on a laptop and in a scratch container.
///
/// The key exchange groups come from the provider chosen in `Cargo.toml`, and
/// that is the one security decision in this function: with
/// `prefer-post-quantum`, X25519MLKEM768 leads the list, so its key share goes
/// out in the first ClientHello rather than costing a HelloRetryRequest. It
/// matters more here than almost anywhere else in this framework — a recorded
/// LDAP handshake, decrypted years from now, yields a directory password.
fn client_config(config: &LdapConfig) -> Arc<rustls::ClientConfig> {
    use std::sync::OnceLock;
    static VERIFYING: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    static TRUSTING: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();

    if config.verify_certificate {
        return VERIFYING
            .get_or_init(|| {
                let roots =
                    rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
                Arc::new(
                    rustls::ClientConfig::builder()
                        .with_root_certificates(roots)
                        .with_no_client_auth(),
                )
            })
            .clone();
    }

    TRUSTING
        .get_or_init(|| {
            Arc::new(
                rustls::ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(TrustAnyCertificate::new()))
                    .with_no_client_auth(),
            )
        })
        .clone()
}

/// Believes any certificate.
///
/// Encryption without authentication: this stops a passive listener and nothing
/// else, because an active attacker presents a certificate of their own. It
/// exists for a test container, and the builder method that reaches it is named
/// so that nobody can claim they were not told.
#[derive(Debug)]
struct TrustAnyCertificate(Arc<rustls::crypto::CryptoProvider>);

impl TrustAnyCertificate {
    fn new() -> TrustAnyCertificate {
        let provider = rustls::crypto::CryptoProvider::get_default()
            .cloned()
            .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
        TrustAnyCertificate(provider)
    }
}

impl rustls::client::danger::ServerCertVerifier for TrustAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_carry_the_security_decision_in_their_scheme() {
        let ldaps = LdapConfig::parse("ldaps://dc.example.test").unwrap();
        assert_eq!(ldaps.host, "dc.example.test");
        assert_eq!(ldaps.port, 636);
        assert_eq!(ldaps.encryption, Encryption::Ldaps);

        // Plain `ldap://` gets StartTLS, not plain text. A URL is not a
        // security decision, and the failure mode of guessing wrong here is a
        // password in the clear.
        let ldap = LdapConfig::parse("ldap://dc.example.test").unwrap();
        assert_eq!(ldap.port, 389);
        assert_eq!(ldap.encryption, Encryption::StartTls);

        assert_eq!(LdapConfig::parse("ldap://dc.example.test:3389").unwrap().port, 3389);
        assert_eq!(LdapConfig::parse("LDAPS://Host").unwrap().encryption, Encryption::Ldaps);
        assert_eq!(LdapConfig::parse("ldaps://[::1]:1636").unwrap().host, "::1");
        assert_eq!(LdapConfig::parse("ldaps://[::1]").unwrap().port, 636);
        assert_eq!(LdapConfig::parse("ldaps://[::1]:1636").unwrap().address(), "[::1]:1636");
    }

    #[test]
    fn the_defaults_are_the_careful_ones() {
        let config = LdapConfig::parse("ldaps://dc.example.test").unwrap();
        assert!(config.verify_certificate, "certificates are checked unless told otherwise");
        assert!(!config.allow_plaintext_password, "and passwords do not go out in the clear");
    }

    #[test]
    fn a_url_without_a_scheme_says_why_the_scheme_matters() {
        let error = LdapConfig::parse("dc.example.test").unwrap_err().to_string();
        assert!(error.contains("ldaps://"), "got {error}");
        assert!(error.contains("encrypted"), "the error should say what is at stake: {error}");

        assert!(LdapConfig::parse("http://dc.example.test").is_err());
        assert!(LdapConfig::parse("ldap://dc.example.test/dc=example").is_err());
        assert!(LdapConfig::parse("ldap://").is_err());
        assert!(LdapConfig::parse("ldap://host:not-a-port").is_err());
        assert!(LdapConfig::parse("ldaps://[::1").is_err());
    }

    #[tokio::test]
    async fn a_simple_bind_over_plain_tcp_is_refused_and_the_error_says_why() {
        // The check has to fire before the socket is touched, which is what
        // makes it testable without a directory: this configuration points at
        // a port nothing is listening on, and the refusal still comes from the
        // transport rule rather than from a connection failure.
        //
        // Built by hand rather than through `connect`, because the point is
        // that no byte reaches a socket.
        let config = LdapConfig::parse("ldap://127.0.0.1:1").unwrap().plaintext();
        let mut connection = LdapConnection {
            stream: LdapStream::Upgrading,
            config,
            buffer: Vec::new(),
            next_id: 1,
            bound_as: None,
        };

        let error = connection.bind("cn=admin", "secret").await.unwrap_err().to_string();
        assert!(error.contains("unencrypted"), "got {error}");
        assert!(error.contains("ldaps://"), "the error should name the fix: {error}");
        assert!(error.contains("allow_plaintext_password"), "and the opt-in: {error}");
    }

    #[tokio::test]
    async fn an_empty_password_never_reaches_the_transport_check_or_the_socket() {
        // Two guards, and the empty-password one is first: an empty password is
        // refused even where plaintext has been explicitly allowed, because it
        // is not a transport problem — a directory answers it with `success`
        // over TLS just as happily.
        let config = LdapConfig::parse("ldap://127.0.0.1:1")
            .unwrap()
            .plaintext()
            .allow_plaintext_password();
        let mut connection = LdapConnection {
            stream: LdapStream::Upgrading,
            config,
            buffer: Vec::new(),
            next_id: 1,
            bound_as: None,
        };

        let error = connection.bind("cn=admin", "").await.unwrap_err().to_string();
        assert!(error.contains("anonymous"), "got {error}");
    }

    #[tokio::test]
    async fn an_unreachable_directory_names_the_address() {
        let config = LdapConfig::parse("ldaps://127.0.0.1:1")
            .unwrap()
            .connect_timeout(Duration::from_secs(2));
        // `LdapConnection` has no `Debug` — it writes passwords — so the error
        // comes out of a match rather than `unwrap_err`.
        let error = match LdapConnection::connect(&config).await {
            Ok(_) => panic!("something is listening on port 1"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("127.0.0.1:1"), "got {error}");
    }

    #[test]
    fn message_ids_start_at_one_and_never_repeat() {
        // Zero means "unsolicited notification", so it can never be a request's
        // id, and a repeated id would let one operation's answer be read as
        // another's.
        let mut connection = LdapConnection {
            stream: LdapStream::Upgrading,
            config: LdapConfig::parse("ldaps://host").unwrap(),
            buffer: Vec::new(),
            next_id: 1,
            bound_as: None,
        };

        assert_eq!(connection.next_message_id().unwrap(), 1);
        assert_eq!(connection.next_message_id().unwrap(), 2);
        assert_eq!(connection.next_message_id().unwrap(), 3);

        connection.next_id = i64::from(i32::MAX) + 1;
        assert!(connection.next_message_id().is_err(), "exhaustion is an error, not a wrap");
    }

    /// The one property of the TLS setup worth a test.
    ///
    /// Only the key exchange is at risk from a quantum computer, and it is at
    /// risk *today*: an observer can record a handshake now and decrypt it once
    /// the machine exists. For an LDAP bind that means a directory password
    /// with a very long shelf life.
    ///
    /// So this asserts the hybrid group is offered, and offered *first*:
    /// rustls sends a key share only for the leading groups, and a hybrid
    /// listed last is one a server can reach only through a second round trip
    /// most will not bother with. Switching the provider back to `ring`
    /// silently loses this.
    #[test]
    fn the_key_exchange_leads_with_a_post_quantum_hybrid() {
        let config = client_config(&LdapConfig::parse("ldaps://host").unwrap());

        let offered: Vec<String> = config
            .crypto_provider()
            .kx_groups
            .iter()
            .map(|group| format!("{:?}", group.name()))
            .collect();

        assert_eq!(
            offered.first().map(String::as_str),
            Some("X25519MLKEM768"),
            "the post-quantum hybrid must lead the ClientHello; offered: {offered:?}"
        );
        assert!(
            offered.iter().any(|name| name == "X25519"),
            "a classical group must remain, for directories that have not caught up: {offered:?}"
        );
    }
}
