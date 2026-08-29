//! URL parsing for outbound requests.

use rustlavel_core::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub secure: bool,
    pub host: String,
    pub port: u16,
    /// Path and query together, which is exactly what goes on the request line.
    pub target: String,
}

impl Url {
    pub fn parse(input: &str) -> Result<Url> {
        let (scheme, rest) = input
            .split_once("://")
            .ok_or_else(|| Error::msg(format!("`{input}` has no scheme; expected http:// or https://")))?;

        let secure = match scheme.to_ascii_lowercase().as_str() {
            "https" => true,
            "http" => false,
            other => {
                return Err(Error::msg(format!(
                    "`{other}` is not a scheme this client speaks; use http or https"
                )));
            }
        };

        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, "/"),
        };

        // Credentials in a URL are rejected rather than silently dropped: a
        // caller who put them there expects them to be sent.
        if authority.contains('@') {
            return Err(Error::msg(
                "credentials in a URL are not supported; send an Authorization header instead"
                    .to_string(),
            ));
        }

        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => {
                let port = port
                    .parse()
                    .map_err(|_| Error::msg(format!("`{port}` is not a valid port")))?;
                (host, port)
            }
            None => (authority, if secure { 443 } else { 80 }),
        };

        if host.is_empty() {
            return Err(Error::msg(format!("`{input}` has no host")));
        }

        Ok(Url {
            secure,
            host: host.to_string(),
            port,
            target: if path.is_empty() { "/".to_string() } else { path.to_string() },
        })
    }

    /// `host:port`, for connecting and for the `Host` header.
    pub fn authority(&self) -> String {
        let default = if self.secure { 443 } else { 80 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn socket_address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_common_shapes() {
        let url = Url::parse("https://api.anthropic.com/v1/messages").unwrap();
        assert!(url.secure);
        assert_eq!(url.host, "api.anthropic.com");
        assert_eq!(url.port, 443);
        assert_eq!(url.target, "/v1/messages");
        assert_eq!(url.authority(), "api.anthropic.com");

        let local = Url::parse("http://127.0.0.1:11434/api/chat?stream=true").unwrap();
        assert!(!local.secure);
        assert_eq!(local.port, 11434);
        assert_eq!(local.target, "/api/chat?stream=true");
        assert_eq!(local.authority(), "127.0.0.1:11434");
    }

    #[test]
    fn a_bare_host_gets_a_root_path() {
        assert_eq!(Url::parse("https://example.com").unwrap().target, "/");
    }

    #[test]
    fn rejects_what_it_cannot_send_correctly() {
        assert!(Url::parse("example.com/path").is_err());
        assert!(Url::parse("ftp://example.com").is_err());
        assert!(Url::parse("https://user:pass@example.com").is_err());
        assert!(Url::parse("https://example.com:notaport/").is_err());
    }
}
