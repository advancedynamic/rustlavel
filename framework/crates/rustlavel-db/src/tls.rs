//! TLS for the drivers that negotiate it over a plain socket.
//!
//! PostgreSQL and MySQL both start a connection in the clear and ask to upgrade:
//! PostgreSQL with an `SSLRequest` packet, MySQL by setting `CLIENT_SSL` in its
//! handshake response. The *asking* is protocol-specific and lives in each
//! driver; everything after the server says yes is the same, and lives here.
//!
//! SQL Server is deliberately not part of this. It tunnels its handshake inside
//! TDS pre-login packets rather than over the raw socket, so it keeps its own
//! stream type in [`crate::sqlserver`].

use crate::config::DatabaseConfig;
use rustlavel_core::{Error, Result};
use std::sync::Arc;

/// How hard to insist on TLS, and how much of the certificate to believe.
///
/// The names match PostgreSQL's `sslmode` and MySQL's `--ssl-mode`, because a
/// person configuring this has almost certainly met them before and a third
/// spelling of the same idea helps nobody.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsMode {
    /// Never encrypt. Everything, the password included, is on the wire in the
    /// clear.
    Disable,
    /// Encrypt if the server offers it, otherwise carry on in the clear.
    ///
    /// The default, and it is worth being blunt about what it is worth: it
    /// **guarantees nothing**. An attacker positioned to read the connection is
    /// also positioned to answer "no, I don't do TLS", and the client will
    /// obligingly continue in plain text. It defends against a passive
    /// eavesdropper on a well-behaved network and against nobody else. Anything
    /// facing a real network wants [`TlsMode::VerifyFull`].
    #[default]
    Prefer,
    /// Refuse to connect without encryption, but believe whatever certificate
    /// is presented.
    ///
    /// Stops passive eavesdropping outright. Does not stop an active attacker,
    /// who simply presents a certificate of their own.
    Require,
    /// Encrypt, and check the certificate chains to a trusted root — but do not
    /// check the hostname.
    ///
    /// The mode for a managed database reached through a name that is not on
    /// its certificate, which is common enough that PostgreSQL and MySQL both
    /// name it.
    VerifyCa,
    /// Encrypt, and check both the chain and the hostname. The only mode that
    /// is secure against an active attacker.
    VerifyFull,
}

impl TlsMode {
    pub fn parse(raw: &str) -> Result<TlsMode> {
        Ok(match raw.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "disable" | "disabled" | "off" | "false" => TlsMode::Disable,
            "prefer" | "preferred" => TlsMode::Prefer,
            "require" | "required" | "on" | "true" => TlsMode::Require,
            "verify-ca" => TlsMode::VerifyCa,
            "verify-full" | "verify-identity" => TlsMode::VerifyFull,
            other => {
                return Err(Error::msg(format!(
                    "`{other}` is not an sslmode. Use disable, prefer, require, verify-ca or \
                     verify-full — prefer is the default, and verify-full is the only one that \
                     is safe against an active attacker."
                )));
            }
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TlsMode::Disable => "disable",
            TlsMode::Prefer => "prefer",
            TlsMode::Require => "require",
            TlsMode::VerifyCa => "verify-ca",
            TlsMode::VerifyFull => "verify-full",
        }
    }

    /// Whether the driver should ask the server to encrypt at all.
    pub fn wants_tls(self) -> bool {
        self != TlsMode::Disable
    }

    /// Whether a server that declines to encrypt is a failure.
    pub fn demands_tls(self) -> bool {
        matches!(self, TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull)
    }

    /// Whether the certificate is checked against a trust anchor.
    pub fn verifies_certificate(self) -> bool {
        matches!(self, TlsMode::VerifyCa | TlsMode::VerifyFull)
    }
}

impl std::fmt::Display for TlsMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A socket that may or may not have been upgraded.
///
/// An enum rather than a boxed trait object: there are exactly two cases, both
/// known at compile time, and a virtual call per read on a database connection
/// buys nothing. `Closed` exists for the instant during the handshake when the
/// plain socket has been taken out and the encrypted one is not yet in — a real
/// placeholder, so a bug there reports itself instead of hanging on a dead file
/// descriptor.
pub enum DbStream {
    Plain(tokio::net::TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>),
    Closed,
}

impl DbStream {
    pub fn is_encrypted(&self) -> bool {
        matches!(self, DbStream::Tls(_))
    }

    /// Take the plain socket out, leaving `Closed` behind.
    ///
    /// Only for the upgrade: a driver calls this, hands the socket to
    /// [`upgrade`], and puts the result back.
    pub fn take_plain(&mut self) -> Result<tokio::net::TcpStream> {
        match std::mem::replace(self, DbStream::Closed) {
            DbStream::Plain(stream) => Ok(stream),
            DbStream::Tls(stream) => {
                *self = DbStream::Tls(stream);
                Err(Error::msg("this connection is already encrypted"))
            }
            DbStream::Closed => Err(closed()),
        }
    }

    pub async fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        match self {
            DbStream::Plain(stream) => stream.write_all(bytes).await,
            DbStream::Tls(stream) => stream.write_all(bytes).await,
            DbStream::Closed => Err(closed_io()),
        }
    }

    pub async fn flush(&mut self) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        match self {
            DbStream::Plain(stream) => stream.flush().await,
            DbStream::Tls(stream) => stream.flush().await,
            DbStream::Closed => Err(closed_io()),
        }
    }

    pub async fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        use tokio::io::AsyncReadExt;
        match self {
            DbStream::Plain(stream) => stream.read(buffer).await,
            DbStream::Tls(stream) => stream.read(buffer).await,
            DbStream::Closed => Err(closed_io()),
        }
    }

    pub async fn shutdown(&mut self) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        match self {
            DbStream::Plain(stream) => stream.shutdown().await,
            DbStream::Tls(stream) => stream.shutdown().await,
            DbStream::Closed => Ok(()),
        }
    }
}

fn closed() -> Error {
    Error::msg("the connection was left mid-upgrade; this is a bug in the driver")
}

fn closed_io() -> std::io::Error {
    std::io::Error::other("the connection was left mid-upgrade; this is a bug in the driver")
}

/// Run the TLS handshake on a socket the server has agreed to encrypt.
pub async fn upgrade(
    stream: tokio::net::TcpStream,
    host: &str,
    config: &DatabaseConfig,
) -> Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let connector = tokio_rustls::TlsConnector::from(client_config(config)?);

    // An IP address is a valid server name for rustls, and one that no public
    // certificate carries — which is why connecting to 127.0.0.1 under
    // verify-full fails, correctly, and the error below says so.
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| Error::msg(format!("`{host}` is not a valid TLS server name")))?;

    connector.connect(name, stream).await.map_err(|error| {
        Error::msg(format!(
            "the TLS handshake with {host} failed: {error}. sslmode is `{}`; if this is a \
             development server with a self-signed certificate, either point `sslrootcert` at \
             its certificate or drop to sslmode=require.",
            config.tls_mode
        ))
    })
}

/// Build (and cache) the rustls configuration a mode implies.
fn client_config(config: &DatabaseConfig) -> Result<Arc<rustls::ClientConfig>> {
    let roots = match &config.tls_root_certificate {
        Some(path) => root_store_from_pem(path)?,
        None => rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() },
    };

    let builder = rustls::ClientConfig::builder();

    let config = match config.tls_mode {
        // Encryption without authentication. Named `dangerous` by rustls, and
        // the name is right: this stops a passive listener and nothing else.
        TlsMode::Require | TlsMode::Disable | TlsMode::Prefer => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TrustAnyCertificate::new()?))
            .with_no_client_auth(),
        TlsMode::VerifyCa => {
            let verifier = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
                .build()
                .map_err(|error| Error::msg(format!("could not build a certificate verifier: {error}")))?;
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(ChainOnly(verifier)))
                .with_no_client_auth()
        }
        TlsMode::VerifyFull => builder.with_root_certificates(roots).with_no_client_auth(),
    };

    Ok(Arc::new(config))
}

/// Read a PEM bundle into a root store.
///
/// The framing is unwrapped here rather than pulled in as a dependency — PEM is
/// a base64 body between two marker lines, not cryptography, and rule one
/// applies. What the bytes then *mean* is still webpki's problem, not ours.
fn root_store_from_pem(path: &str) -> Result<rustls::RootCertStore> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        Error::msg(format!("could not read the certificate at `{path}`: {error}"))
    })?;

    let mut store = rustls::RootCertStore::empty();
    let mut found = 0;

    for block in text.split("-----BEGIN CERTIFICATE-----").skip(1) {
        let body = block.split("-----END CERTIFICATE-----").next().ok_or_else(|| {
            Error::msg(format!("`{path}` has a BEGIN CERTIFICATE line with no END"))
        })?;

        let der = crate::base64::decode(&body.replace([' ', '\t'], "")).ok_or_else(|| {
            Error::msg(format!("`{path}` contains a certificate that is not valid base64"))
        })?;

        store.add(rustls::pki_types::CertificateDer::from(der)).map_err(|error| {
            Error::msg(format!("`{path}` contains a certificate rustls will not accept: {error}"))
        })?;
        found += 1;
    }

    if found == 0 {
        return Err(Error::msg(format!(
            "`{path}` contains no `-----BEGIN CERTIFICATE-----` block. It should be a PEM \
             file; a DER file has to be converted first."
        )));
    }

    Ok(store)
}

/// Chain checked, hostname not — `verify-ca`.
///
/// It delegates to the real verifier and forgives exactly one error: the name
/// mismatch. Doing it this way rather than validating the chain by hand means
/// expiry, signatures, key usage and path length are all still webpki's
/// answer, and only the single check the mode is defined to skip is skipped.
#[derive(Debug)]
struct ChainOnly(Arc<rustls::client::WebPkiServerVerifier>);

impl rustls::client::danger::ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        use rustls::CertificateError::{NotValidForName, NotValidForNameContext};

        match self.0.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Err(rustls::Error::InvalidCertificate(
                NotValidForName | NotValidForNameContext { .. },
            )) => Ok(rustls::client::danger::ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// Believes any certificate — `require` and `prefer`.
#[derive(Debug)]
struct TrustAnyCertificate(Arc<rustls::crypto::CryptoProvider>);

impl TrustAnyCertificate {
    fn new() -> Result<TrustAnyCertificate> {
        let provider = rustls::crypto::CryptoProvider::get_default()
            .cloned()
            .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
        Ok(TrustAnyCertificate(provider))
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
    fn modes_parse_under_the_spellings_people_actually_write() {
        assert_eq!(TlsMode::parse("disable").unwrap(), TlsMode::Disable);
        assert_eq!(TlsMode::parse("PREFER").unwrap(), TlsMode::Prefer);
        assert_eq!(TlsMode::parse(" require ").unwrap(), TlsMode::Require);
        assert_eq!(TlsMode::parse("verify_ca").unwrap(), TlsMode::VerifyCa);
        assert_eq!(TlsMode::parse("verify-full").unwrap(), TlsMode::VerifyFull);
        // MySQL's spelling of verify-full.
        assert_eq!(TlsMode::parse("VERIFY_IDENTITY").unwrap(), TlsMode::VerifyFull);
    }

    #[test]
    fn an_unknown_mode_lists_the_real_ones_and_says_which_is_safe() {
        let error = TlsMode::parse("yes-please").unwrap_err().to_string();

        assert!(error.contains("verify-full"), "got {error}");
        assert!(error.contains("active attacker"), "the error should say what is at stake");
    }

    #[test]
    fn the_default_is_prefer() {
        assert_eq!(TlsMode::default(), TlsMode::Prefer);
    }

    #[test]
    fn what_each_mode_insists_on() {
        // prefer asks but does not insist — the property that makes it worth
        // nothing against an active attacker.
        assert!(TlsMode::Prefer.wants_tls());
        assert!(!TlsMode::Prefer.demands_tls());
        assert!(!TlsMode::Prefer.verifies_certificate());

        assert!(!TlsMode::Disable.wants_tls());

        assert!(TlsMode::Require.demands_tls());
        assert!(!TlsMode::Require.verifies_certificate());

        for mode in [TlsMode::VerifyCa, TlsMode::VerifyFull] {
            assert!(mode.demands_tls());
            assert!(mode.verifies_certificate());
        }
    }

    #[test]
    fn modes_round_trip_through_their_written_form() {
        for mode in [
            TlsMode::Disable,
            TlsMode::Prefer,
            TlsMode::Require,
            TlsMode::VerifyCa,
            TlsMode::VerifyFull,
        ] {
            assert_eq!(TlsMode::parse(mode.as_str()).unwrap(), mode);
        }
    }

    #[test]
    fn a_pem_file_that_is_not_a_certificate_says_so() {
        let path = std::env::temp_dir().join(format!("rustlavel-tls-not-a-cert-{}.pem", std::process::id()));
        std::fs::write(&path, "just some text\n").unwrap();

        let error = root_store_from_pem(path.to_str().unwrap()).unwrap_err().to_string();
        assert!(error.contains("BEGIN CERTIFICATE"), "got {error}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_certificate_file_names_the_path() {
        let error = root_store_from_pem("/nope/does-not-exist.pem").unwrap_err().to_string();
        assert!(error.contains("/nope/does-not-exist.pem"), "got {error}");
    }

    #[test]
    fn a_stream_left_mid_upgrade_errors_rather_than_hanging() {
        let mut stream = DbStream::Closed;
        assert!(stream.take_plain().is_err());
        assert!(!stream.is_encrypted());
    }
}
