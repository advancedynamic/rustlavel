//! The socket an SMTP conversation runs over, plain or wrapped in TLS.
//!
//! An enum rather than a boxed trait object, for the same reason the HTTP
//! client uses one: there are exactly two cases, and the read path is hot
//! enough that dynamic dispatch would be a needless cost.
//!
//! The client itself is generic over any `AsyncRead + AsyncWrite`, which is
//! what lets the whole protocol be tested against an in-memory duplex pipe
//! with no socket, no port, and no server.

use rustlavel_core::{Error, Result};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

pub enum SmtpStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl SmtpStream {
    pub fn is_encrypted(&self) -> bool {
        matches!(self, SmtpStream::Tls(_))
    }

    /// Negotiate TLS over an already-open connection, for STARTTLS.
    pub async fn upgrade(self, host: &str) -> Result<SmtpStream> {
        let tcp = match self {
            SmtpStream::Plain(tcp) => tcp,
            already => return Ok(already),
        };

        let name = rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|_| Error::msg(format!("`{host}` is not a valid TLS server name")))?;

        let tls = connector()
            .connect(name, tcp)
            .await
            .map_err(|e| Error::msg(format!("TLS handshake with {host} failed: {e}")))?;

        Ok(SmtpStream::Tls(Box::new(tls)))
    }
}

/// Open a TCP connection, wrapping it in TLS straight away when asked.
///
/// Implicit TLS is what port 465 means: the server expects a handshake before
/// it says anything at all.
pub async fn connect(
    host: &str,
    port: u16,
    implicit_tls: bool,
    timeout: std::time::Duration,
) -> Result<SmtpStream> {
    let address = format!("{host}:{port}");

    let tcp = tokio::time::timeout(timeout, TcpStream::connect(&address))
        .await
        .map_err(|_| {
            Error::msg(format!(
                "timed out connecting to {address}. Is the mail server reachable, and is \
                 mail.port right?"
            ))
        })?
        .map_err(|e| Error::msg(format!("cannot connect to {address}: {e}")))?;

    let _ = tcp.set_nodelay(true);

    let stream = SmtpStream::Plain(tcp);
    if implicit_tls { stream.upgrade(host).await } else { Ok(stream) }
}

/// The TLS configuration, built once and shared.
///
/// Trust anchors come from webpki-roots rather than the OS store, so behaviour
/// is identical on a laptop and in a scratch container.
fn connector() -> tokio_rustls::TlsConnector {
    use std::sync::OnceLock;
    static CONNECTOR: OnceLock<tokio_rustls::TlsConnector> = OnceLock::new();

    CONNECTOR
        .get_or_init(|| {
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            let config =
                rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
            tokio_rustls::TlsConnector::from(Arc::new(config))
        })
        .clone()
}

impl AsyncRead for SmtpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            SmtpStream::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            SmtpStream::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for SmtpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            SmtpStream::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            SmtpStream::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            SmtpStream::Plain(stream) => Pin::new(stream).poll_flush(cx),
            SmtpStream::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            SmtpStream::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            SmtpStream::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_refused_connection_names_the_host_and_the_port_setting() {
        let message = match connect("127.0.0.1", 1, false, Duration::from_secs(2)).await {
            Err(error) => error.to_string(),
            Ok(_) => panic!("nothing should be listening on port 1"),
        };

        assert!(message.contains("127.0.0.1:1"), "{message}");
    }
}
