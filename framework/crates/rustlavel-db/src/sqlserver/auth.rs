//! SQL Server authentication, and the TLS that has to happen first.
//!
//! # What this module does *not* do
//!
//! Only SQL Server authentication — a username and a password held by the
//! server — is implemented. Windows integrated authentication (NTLM, Kerberos,
//! SSPI) and Microsoft Entra federated authentication are **not**: they need a
//! full GSS-API negotiation and a Windows credential cache, and a half-built
//! one that silently falls back to something weaker would be worse than none.
//! A server that demands SSPI is told so by name rather than being retried.
//!
//! # The part that surprises everyone
//!
//! TDS negotiates TLS *inside* its own pre-login exchange. The sequence is:
//!
//! 1. The client sends a PRELOGIN packet naming the encryption it wants.
//! 2. The server answers with a PRELOGIN packet naming what it will do.
//! 3. If either side asked for encryption, a **complete TLS handshake** now
//!    runs — but every handshake record is wrapped in a TDS packet of type
//!    PRELOGIN, header and all. TLS is being tunnelled through a protocol that
//!    has not started yet.
//! 4. The moment the handshake finishes, the wrapping stops. From the next byte
//!    on, the connection is ordinary TLS carrying ordinary TDS packets.
//!
//! [`TdsHandshakeStream`] is what makes step 3 and step 4 the same object: it
//! frames while `wrapping` is set and passes bytes straight through afterwards,
//! so `tokio-rustls` can drive a normal handshake over it without knowing that
//! anything unusual is happening underneath.
//!
//! ## Why TLS 1.2 and not 1.3
//!
//! Under TLS 1.3 a server considers the handshake finished as soon as it has
//! sent its own Finished, and immediately sends session tickets — which would
//! arrive unwrapped while this side is still reading wrapped packets. TLS 1.2
//! has no post-handshake traffic and both sides stop wrapping at the same
//! byte, so the wrapped handshake is pinned to TLS 1.2. TDS 8.0, which starts
//! TLS before TDS rather than inside it, is what lifts that restriction; this
//! driver speaks TDS 7.4.

use super::protocol::{HEADER_LEN, PacketHeader, packet, split_message};
use rustlavel_core::{Error, Result};
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

/// How much of the session is encrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encryption {
    /// ENCRYPT_NOT_SUP: nothing is encrypted, the password included. Only for a
    /// server that genuinely has no certificate.
    Disabled,
    /// ENCRYPT_OFF: TLS wraps the login packet and is then torn down, so the
    /// credentials are protected but the queries are not.
    LoginOnly,
    /// ENCRYPT_ON: TLS for the whole session. The default, because the cost is
    /// a handshake and the alternative is queries in the clear.
    #[default]
    Required,
}

impl Encryption {
    /// The byte this choice puts in the PRELOGIN ENCRYPTION option.
    pub fn as_byte(self) -> u8 {
        match self {
            Encryption::Disabled => super::protocol::encryption::NOT_SUPPORTED,
            Encryption::LoginOnly => super::protocol::encryption::OFF,
            Encryption::Required => super::protocol::encryption::ON,
        }
    }
}

/// What the two sides settled on, once both have spoken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Negotiated {
    /// No TLS at all.
    None,
    /// TLS for the login packet, then back to the clear.
    LoginOnly,
    /// TLS for everything.
    Session,
}

/// Work out what happens next from the two ENCRYPTION bytes.
///
/// The table is small but every cell matters: the one case that must fail is a
/// server insisting on encryption a client refuses, because continuing would
/// put the password on the wire in something very close to plaintext.
pub fn negotiate(requested: Encryption, server: u8) -> Result<Negotiated> {
    use super::protocol::encryption as level;

    Ok(match (requested, server) {
        (Encryption::Disabled, level::NOT_SUPPORTED) => Negotiated::None,
        (Encryption::Disabled, level::REQUIRED) | (Encryption::Disabled, level::ON) => {
            return Err(Error::msg(
                "the server requires an encrypted connection, but this connection asked for none. \
                 Use the default encryption setting.",
            ));
        }
        (Encryption::Disabled, _) => Negotiated::None,

        // The server cannot encrypt. Login-only was a preference, so continue;
        // full encryption was a requirement, so do not.
        (Encryption::LoginOnly, level::NOT_SUPPORTED) => Negotiated::None,
        (Encryption::Required, level::NOT_SUPPORTED) => {
            return Err(Error::msg(
                "this connection requires encryption, but the server reports it cannot encrypt. \
                 Give SQL Server a certificate, or connect with encryption set to login-only.",
            ));
        }

        // A server that says ON or REQUIRED encrypts everything, whatever the
        // client merely preferred.
        (_, level::ON) | (_, level::REQUIRED) => Negotiated::Session,
        (Encryption::Required, _) => Negotiated::Session,
        (Encryption::LoginOnly, _) => Negotiated::LoginOnly,
    })
}

/// Encode a password for LOGIN7.
///
/// This is obfuscation, not a hash: MS-TDS specifies that each byte of the
/// UTF-16LE password has its nibbles swapped and is then XORed with 0xA5. It
/// hides nothing from anyone watching the wire, which is exactly why the login
/// packet is sent through TLS.
pub fn obfuscate_password(password: &str) -> Vec<u8> {
    password
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        // `rotate_left(4)` on a byte is exactly the documented nibble swap.
        .map(|byte| byte.rotate_left(4) ^ 0xA5)
        .collect()
}

/// Undo [`obfuscate_password`]. Only the tests need it — but a scheme whose
/// inverse is never written is a scheme nobody has checked.
pub fn deobfuscate_password(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .iter()
        .map(|byte| {
            let plain = byte ^ 0xA5;
            plain.rotate_left(4)
        })
        .collect::<Vec<u8>>()
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();

    String::from_utf16_lossy(&units)
}

/// A socket that frames the TLS handshake into TDS packets, and stops once the
/// handshake is done.
///
/// Written as a plain `AsyncRead`/`AsyncWrite` so `tokio-rustls` can drive an
/// ordinary handshake across it. Writes are gathered and framed at the flush
/// that ends each handshake flight, which keeps one TLS flight to one TDS
/// message — the shape SQL Server expects.
pub struct TdsHandshakeStream {
    socket: TcpStream,
    /// Cleared by [`TdsHandshakeStream::stop_wrapping`] once TLS is up.
    wrapping: bool,
    packet_size: usize,
    /// TLS bytes written but not yet framed.
    outgoing: Vec<u8>,
    /// Framed bytes on their way to the socket, and how far they have got.
    pending: Vec<u8>,
    pending_at: usize,
    /// Bytes read from the socket that are not yet a whole packet.
    incoming: Vec<u8>,
    /// Unwrapped payload waiting to be handed to the TLS engine.
    ready: Vec<u8>,
    ready_at: usize,
}

impl TdsHandshakeStream {
    pub fn new(socket: TcpStream, packet_size: usize) -> Self {
        TdsHandshakeStream {
            socket,
            wrapping: true,
            packet_size,
            outgoing: Vec::new(),
            pending: Vec::new(),
            pending_at: 0,
            incoming: Vec::new(),
            ready: Vec::new(),
            ready_at: 0,
        }
    }

    /// Stop framing. Called the instant the TLS handshake completes, which is
    /// the exact byte at which the server stops framing too.
    pub fn stop_wrapping(&mut self) {
        self.wrapping = false;
    }

    /// Take the socket back, for a login-only session that returns to the clear.
    ///
    /// Fails if anything was read ahead of the handshake, because those bytes
    /// belong to the TLS session and dropping them would corrupt the stream.
    pub fn into_socket(self) -> Result<TcpStream> {
        if self.ready_at < self.ready.len() || !self.incoming.is_empty() {
            return Err(Error::Protocol(
                "the server sent data before encryption was torn down".into(),
            ));
        }
        Ok(self.socket)
    }

    /// Pull one complete packet's payload out of `incoming`, if there is one.
    fn take_packet(&mut self) -> Result<bool> {
        if self.incoming.len() < HEADER_LEN {
            return Ok(false);
        }
        let header = PacketHeader::parse(&self.incoming)?;
        let total = header.length as usize;
        if total < HEADER_LEN {
            return Err(Error::Protocol("packet length is impossibly small".into()));
        }
        if self.incoming.len() < total {
            return Ok(false);
        }

        self.ready = self.incoming[HEADER_LEN..total].to_vec();
        self.ready_at = 0;
        self.incoming.drain(..total);
        Ok(true)
    }
}

fn protocol_io(error: Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

impl AsyncRead for TdsHandshakeStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if self.ready_at < self.ready.len() {
                let take = buf.remaining().min(self.ready.len() - self.ready_at);
                let at = self.ready_at;
                buf.put_slice(&self.ready[at..at + take]);
                self.ready_at += take;
                return Poll::Ready(Ok(()));
            }

            if !self.wrapping {
                // Anything already unwrapped has been delivered; from here the
                // socket is the TLS session's own.
                if !self.incoming.is_empty() {
                    self.ready = std::mem::take(&mut self.incoming);
                    self.ready_at = 0;
                    continue;
                }
                return Pin::new(&mut self.socket).poll_read(cx, buf);
            }

            if self.take_packet().map_err(protocol_io)? {
                continue;
            }

            let mut chunk = [0u8; 8192];
            let mut incoming = ReadBuf::new(&mut chunk);
            ready!(Pin::new(&mut self.socket).poll_read(cx, &mut incoming))?;

            let filled = incoming.filled().len();
            if filled == 0 {
                // End of file: let the TLS engine report the truncated
                // handshake, which says far more than an I/O error would.
                return Poll::Ready(Ok(()));
            }
            let bytes = incoming.filled().to_vec();
            self.incoming.extend_from_slice(&bytes);
        }
    }
}

impl AsyncWrite for TdsHandshakeStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if !self.wrapping {
            return Pin::new(&mut self.socket).poll_write(cx, buf);
        }
        // Gathered rather than sent: the framing happens at the flush that ends
        // the flight, so one flight becomes one TDS message.
        self.outgoing.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.wrapping && !self.outgoing.is_empty() {
            let flight = std::mem::take(&mut self.outgoing);
            let packet_size = self.packet_size;
            for packet in split_message(packet::PRE_LOGIN, &flight, packet_size) {
                self.pending.extend_from_slice(&packet);
            }
        }

        while self.pending_at < self.pending.len() {
            let this = &mut *self;
            let written =
                ready!(Pin::new(&mut this.socket).poll_write(cx, &this.pending[this.pending_at..]))?;
            if written == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.pending_at += written;
        }
        self.pending.clear();
        self.pending_at = 0;

        Pin::new(&mut self.socket).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_shutdown(cx)
    }
}

/// A TDS connection, before or after encryption.
///
/// An enum rather than a boxed trait object for the same reason the HTTP client
/// uses one: there are exactly two cases, they are known at compile time, and a
/// virtual call per read on a database connection buys nothing.
pub enum TdsStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TdsHandshakeStream>>),
    /// No transport, only ever seen for the instant it takes to swap one kind
    /// for another mid-handshake. A real placeholder rather than a dummy socket
    /// so that a bug here reports itself instead of hanging on a dead file
    /// descriptor.
    Closed,
}

impl TdsStream {
    pub async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            TdsStream::Plain(stream) => stream.write_all(bytes).await,
            TdsStream::Tls(stream) => stream.write_all(bytes).await,
            TdsStream::Closed => Err(closed()),
        }
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        match self {
            TdsStream::Plain(stream) => stream.flush().await,
            TdsStream::Tls(stream) => stream.flush().await,
            TdsStream::Closed => Err(closed()),
        }
    }

    pub async fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        use tokio::io::AsyncReadExt;
        match self {
            TdsStream::Plain(stream) => stream.read(buffer).await,
            TdsStream::Tls(stream) => stream.read(buffer).await,
            TdsStream::Closed => Err(closed()),
        }
    }

    pub async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            TdsStream::Plain(stream) => stream.shutdown().await,
            TdsStream::Tls(stream) => stream.shutdown().await,
            TdsStream::Closed => Ok(()),
        }
    }

    /// Take the transport, leaving [`TdsStream::Closed`] behind.
    pub fn take(&mut self) -> TdsStream {
        std::mem::replace(self, TdsStream::Closed)
    }

    /// Tear the TLS session down and go back to the bare socket, which is what
    /// ENCRYPT_OFF asks for once the login packet has been sent.
    pub fn into_plain(self) -> Result<TdsStream> {
        match self {
            TdsStream::Tls(stream) => {
                let (wrapper, _session) = stream.into_inner();
                Ok(TdsStream::Plain(wrapper.into_socket()?))
            }
            other => Ok(other),
        }
    }
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "the TDS connection has no transport")
}

/// How the certificate the server presents is checked.
#[derive(Debug, Clone, Copy)]
pub struct TlsOptions {
    /// Accept whatever certificate the server presents.
    ///
    /// On by default, and that is a deliberate, documented compromise: SQL
    /// Server generates a self-signed certificate at startup unless an
    /// administrator installs one, so verification would fail against every
    /// stock installation. Encryption still protects against passive
    /// eavesdropping; it does not protect against an active attacker until this
    /// is turned off. Every SQL Server driver makes the same trade under the
    /// name `TrustServerCertificate`.
    pub trust_server_certificate: bool,
}

impl Default for TlsOptions {
    fn default() -> Self {
        TlsOptions { trust_server_certificate: true }
    }
}

/// Run the TLS handshake through the pre-login tunnel.
pub async fn start_tls(
    socket: TcpStream,
    host: &str,
    options: TlsOptions,
    packet_size: usize,
) -> Result<tokio_rustls::client::TlsStream<TdsHandshakeStream>> {
    let connector = tokio_rustls::TlsConnector::from(client_config(options));

    // A bare IP address is a valid TLS server name; `try_from` picks the right
    // representation for it, which matters because a developer's connection
    // string is nearly always an IP.
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| Error::msg(format!("`{host}` is not a valid TLS server name")))?;

    let wrapper = TdsHandshakeStream::new(socket, packet_size);
    let mut stream = connector.connect(name, wrapper).await.map_err(|e| {
        Error::msg(format!(
            "the TLS handshake inside SQL Server's pre-login exchange failed: {e}"
        ))
    })?;

    // The handshake is over, so both sides stop framing at exactly this byte.
    stream.get_mut().0.stop_wrapping();
    Ok(stream)
}

/// Build the TLS configuration, once per process.
///
/// The provider is named rather than left to rustls to infer: a build that also
/// pulls in `aws-lc-rs` — which `tokio-rustls` does by default — has two
/// candidates and rustls refuses to guess between them. Saying `ring` here makes
/// the choice a property of this driver rather than of whatever else happens to
/// be in the dependency graph.
fn client_config(options: TlsOptions) -> Arc<rustls::ClientConfig> {
    use std::sync::OnceLock;
    static TRUSTING: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    static VERIFYING: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();

    if options.trust_server_certificate {
        Arc::clone(TRUSTING.get_or_init(|| {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let verifier = Arc::new(TrustAnyCertificate(Arc::clone(&provider)));
            Arc::new(
                builder(provider)
                    .dangerous()
                    .with_custom_certificate_verifier(verifier)
                    .with_no_client_auth(),
            )
        }))
    } else {
        Arc::clone(VERIFYING.get_or_init(|| {
            // Trust anchors from webpki-roots rather than the OS store, so
            // behaviour is identical on a laptop and in a scratch container.
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            Arc::new(
                builder(Arc::new(rustls::crypto::ring::default_provider()))
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        }))
    }
}

/// A config builder pinned to TLS 1.2 — see the module documentation. A wrapped
/// handshake and TLS 1.3's post-handshake tickets cannot both be right.
fn builder(
    provider: Arc<rustls::crypto::CryptoProvider>,
) -> rustls::ConfigBuilder<rustls::ClientConfig, rustls::WantsVerifier> {
    rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS12])
        .expect("TLS 1.2 is enabled by this crate's rustls features")
}

/// A verifier that accepts any certificate, for [`TlsOptions::trust_server_certificate`].
///
/// It accepts the handshake signatures unchecked as well as the certificate,
/// and that is not laziness. SQL Server's auto-generated certificate is an
/// X.509 **version 1** certificate, which rustls refuses even to parse
/// (`UnsupportedCertVersion`) — so the signature check cannot run against a
/// stock installation at all, because it never gets as far as a public key.
///
/// What this mode buys is confidentiality against someone watching the wire,
/// which is what keeps the login packet's barely-obfuscated password safe. It
/// buys nothing against an active attacker who can sit in the middle. Turn
/// [`TlsOptions::trust_server_certificate`] off — after installing a real
/// certificate on the server — and the ordinary webpki verifier does the whole
/// job, chain and signatures both.
#[derive(Debug)]
struct TrustAnyCertificate(Arc<rustls::crypto::CryptoProvider>);

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
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::protocol::encryption as level;

    #[test]
    fn the_password_scheme_swaps_nibbles_then_xors_with_a5() {
        // MS-TDS 2.2.6.4, worked through one character: 'a' is U+0061, so the
        // UTF-16LE bytes are 0x61 0x00. Swapping nibbles gives 0x16 0x00, and
        // XOR 0xA5 gives 0xB3 0xA5.
        assert_eq!(obfuscate_password("a"), vec![0xB3, 0xA5]);
        assert_eq!(obfuscate_password("abc"), vec![0xB3, 0xA5, 0x83, 0xA5, 0x93, 0xA5]);

        // A high code point still goes through as two UTF-16 code units.
        assert_eq!(obfuscate_password("é").len(), 2);
        assert_eq!(obfuscate_password(""), Vec::<u8>::new());
    }

    #[test]
    fn obfuscation_is_reversible_which_is_the_whole_point_of_calling_it_that() {
        for password in ["", "a", "Rustlavel!2026", "pässwörd", "日本語"] {
            assert_eq!(deobfuscate_password(&obfuscate_password(password)), password);
        }
    }

    #[test]
    fn encryption_choices_map_onto_the_prelogin_option_bytes() {
        assert_eq!(Encryption::Disabled.as_byte(), level::NOT_SUPPORTED);
        assert_eq!(Encryption::LoginOnly.as_byte(), level::OFF);
        assert_eq!(Encryption::Required.as_byte(), level::ON);
        assert_eq!(Encryption::default(), Encryption::Required);
    }

    #[test]
    fn a_server_that_offers_only_login_encryption_gets_login_encryption() {
        assert_eq!(negotiate(Encryption::LoginOnly, level::OFF).unwrap(), Negotiated::LoginOnly);
    }

    #[test]
    fn a_server_that_wants_full_encryption_gets_it_whatever_the_client_preferred() {
        for server in [level::ON, level::REQUIRED] {
            assert_eq!(negotiate(Encryption::LoginOnly, server).unwrap(), Negotiated::Session);
            assert_eq!(negotiate(Encryption::Required, server).unwrap(), Negotiated::Session);
        }
    }

    #[test]
    fn a_server_that_cannot_encrypt_fails_a_connection_that_requires_it() {
        let error = negotiate(Encryption::Required, level::NOT_SUPPORTED).unwrap_err().to_string();
        assert!(error.contains("cannot encrypt"), "{error}");

        // Login-only was a preference, so it continues in the clear instead.
        assert_eq!(negotiate(Encryption::LoginOnly, level::NOT_SUPPORTED).unwrap(), Negotiated::None);
    }

    #[test]
    fn refusing_encryption_a_server_requires_is_an_error_not_a_downgrade() {
        // Silently continuing here would put the password on the wire behind
        // nothing but a nibble swap.
        let error = negotiate(Encryption::Disabled, level::REQUIRED).unwrap_err().to_string();
        assert!(error.contains("requires an encrypted connection"), "{error}");

        assert_eq!(negotiate(Encryption::Disabled, level::NOT_SUPPORTED).unwrap(), Negotiated::None);
    }

    #[tokio::test]
    async fn the_handshake_wrapper_frames_a_flight_and_then_gets_out_of_the_way() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            socket.read_buf(&mut received).await.unwrap();
            // Give the second, unwrapped write time to arrive too.
            let mut more = Vec::new();
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                socket.read_buf(&mut more),
            )
            .await;
            received.extend_from_slice(&more);
            received
        });

        let mut wrapper =
            TdsHandshakeStream::new(TcpStream::connect(address).await.unwrap(), 4096);
        wrapper.write_all(b"handshake").await.unwrap();
        wrapper.flush().await.unwrap();
        wrapper.stop_wrapping();
        wrapper.write_all(b"raw").await.unwrap();
        wrapper.flush().await.unwrap();

        let received = server.await.unwrap();

        // The first write arrived inside a PRELOGIN packet.
        let header = PacketHeader::parse(&received).unwrap();
        assert_eq!(header.kind, packet::PRE_LOGIN);
        assert!(header.is_end_of_message());
        assert_eq!(&received[HEADER_LEN..header.length as usize], b"handshake");

        // The second went out untouched, because the handshake was over.
        assert_eq!(&received[header.length as usize..], b"raw");
    }
}
