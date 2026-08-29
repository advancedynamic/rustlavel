//! Redis connection settings, from a URL or from the application config.

use rustlavel_core::{Config, Error, Result};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RedisConfig {
    pub host: String,
    pub port: u16,
    /// Redis 6 ACL username. Empty means the legacy single-password mode.
    pub username: String,
    pub password: String,
    /// The numbered database `SELECT`ed after connecting.
    pub database: u32,
    /// Connections the pool will open at once.
    pub max_connections: usize,
    pub connect_timeout: Duration,
    /// How long one command may take before the connection is given up on.
    pub command_timeout: Duration,
}

impl Default for RedisConfig {
    fn default() -> Self {
        RedisConfig {
            host: "127.0.0.1".into(),
            port: 6379,
            username: String::new(),
            password: String::new(),
            database: 0,
            max_connections: 10,
            connect_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_secs(10),
        }
    }
}

impl RedisConfig {
    /// Parse `redis://[[user]:password@]host[:port][/db]`.
    ///
    /// The password-only form `redis://:secret@host` is the one almost every
    /// deployment uses, so it is handled first-class rather than as a special
    /// case of a username.
    pub fn from_url(url: &str) -> Result<Self> {
        let rest = url
            .strip_prefix("redis://")
            .or_else(|| url.strip_prefix("rediss://"))
            .ok_or_else(|| {
                Error::msg(format!(
                    "`{url}` is not a Redis URL. Expected redis://[:password@]host:port[/db]"
                ))
            })?;

        if url.starts_with("rediss://") {
            return Err(Error::msg(
                "rustlavel-cache speaks plain RESP over TCP; `rediss://` (TLS) is not supported yet. \
                 Terminate TLS with stunnel or a sidecar, or use redis://.",
            ));
        }

        let mut config = RedisConfig::default();

        // Strip the query string first so a `?` inside it is never read as part
        // of the database number.
        let (rest, query) = match rest.split_once('?') {
            Some((rest, query)) => (rest, Some(query)),
            None => (rest, None),
        };

        // The *last* `@` separates credentials from the host, which is what
        // makes an `@` inside a password unambiguous.
        let (credentials, host_part) = match rest.rsplit_once('@') {
            Some((credentials, host)) => (Some(credentials), host),
            None => (None, rest),
        };

        if let Some(credentials) = credentials {
            let (username, password) = match credentials.split_once(':') {
                Some((username, password)) => (username, password),
                // `redis://secret@host` — a bare credential is a password.
                None => ("", credentials),
            };
            config.username = decode(username);
            config.password = decode(password);
        }

        let (host, database) = match host_part.split_once('/') {
            Some((host, database)) => (host, database),
            None => (host_part, ""),
        };

        if !database.is_empty() {
            config.database = database.parse().map_err(|_| {
                Error::msg(format!("`{database}` is not a Redis database number"))
            })?;
        }

        if !host.is_empty() {
            let (name, port) = match host.rsplit_once(':') {
                Some((name, port)) => (name, Some(port)),
                None => (host, None),
            };
            if !name.is_empty() {
                config.host = name.to_string();
            }
            if let Some(port) = port {
                config.port = port
                    .parse()
                    .map_err(|_| Error::msg(format!("`{port}` is not a valid port number")))?;
            }
        }

        for (key, value) in query.into_iter().flat_map(|q| q.split('&')).filter_map(|p| p.split_once('='))
        {
            match key {
                "max_connections" => {
                    config.max_connections = value.parse().unwrap_or(config.max_connections).max(1);
                }
                "connect_timeout" => {
                    if let Ok(seconds) = value.parse() {
                        config.connect_timeout = Duration::from_secs(seconds);
                    }
                }
                "command_timeout" => {
                    if let Ok(seconds) = value.parse() {
                        config.command_timeout = Duration::from_secs(seconds);
                    }
                }
                _ => {}
            }
        }

        Ok(config)
    }

    /// Read from the application config, falling back to `REDIS_URL`.
    pub fn from_app_config(config: &Config) -> Result<Self> {
        let url = config.string("cache.url", "");
        if !url.is_empty() {
            return RedisConfig::from_url(&url);
        }
        if let Ok(url) = std::env::var("REDIS_URL")
            && !url.is_empty()
        {
            return RedisConfig::from_url(&url);
        }
        Ok(RedisConfig::default())
    }

    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The URL with the password removed, for logs and error messages.
    ///
    /// Every message this crate produces about a connection uses this, so a
    /// stack trace on a shared screen never leaks a credential.
    pub fn redacted_url(&self) -> String {
        let credentials = match (self.username.is_empty(), self.password.is_empty()) {
            (true, true) => String::new(),
            (true, false) => ":***@".to_string(),
            (false, _) => format!("{}:***@", self.username),
        };
        format!("redis://{credentials}{}:{}/{}", self.host, self.port, self.database)
    }
}

/// Percent-decode a credential, so a password containing `@`, `/` or `:` can
/// be written into a URL at all.
fn decode(value: &str) -> String {
    if !value.contains('%') {
        return value.to_string();
    }

    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_password_only_form_every_deployment_uses() {
        let config = RedisConfig::from_url("redis://:hunter2@cache.internal:6380/3").unwrap();

        assert_eq!(config.username, "");
        assert_eq!(config.password, "hunter2");
        assert_eq!(config.host, "cache.internal");
        assert_eq!(config.port, 6380);
        assert_eq!(config.database, 3);
    }

    #[test]
    fn parses_an_acl_username_and_password() {
        let config = RedisConfig::from_url("redis://ada:hunter2@localhost").unwrap();

        assert_eq!(config.username, "ada");
        assert_eq!(config.password, "hunter2");
        assert_eq!(config.port, 6379);
        assert_eq!(config.database, 0);
    }

    #[test]
    fn falls_back_to_defaults_for_every_missing_part() {
        let config = RedisConfig::from_url("redis://127.0.0.1:6379").unwrap();

        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 6379);
        assert_eq!(config.database, 0);
        assert!(config.password.is_empty());
    }

    #[test]
    fn a_bare_credential_is_read_as_a_password_not_a_username() {
        let config = RedisConfig::from_url("redis://hunter2@host").unwrap();

        assert!(config.username.is_empty());
        assert_eq!(config.password, "hunter2");
    }

    #[test]
    fn a_password_may_contain_an_at_sign_or_a_slash() {
        let config = RedisConfig::from_url("redis://:p%40ss%2Fword@host/1").unwrap();

        assert_eq!(config.password, "p@ss/word");
        assert_eq!(config.host, "host");
        assert_eq!(config.database, 1);
    }

    #[test]
    fn reads_pool_settings_from_the_query_string() {
        let config =
            RedisConfig::from_url("redis://host/0?max_connections=25&connect_timeout=2").unwrap();

        assert_eq!(config.max_connections, 25);
        assert_eq!(config.connect_timeout, Duration::from_secs(2));
    }

    #[test]
    fn rejects_a_url_with_the_wrong_scheme() {
        let error = RedisConfig::from_url("memcached://host").unwrap_err();
        assert!(error.to_string().contains("not a Redis URL"));
    }

    #[test]
    fn refuses_tls_urls_out_loud_rather_than_connecting_in_the_clear() {
        let error = RedisConfig::from_url("rediss://host").unwrap_err();
        assert!(error.to_string().contains("TLS"), "got: {error}");
    }

    #[test]
    fn rejects_a_database_that_is_not_a_number() {
        assert!(RedisConfig::from_url("redis://host/not-a-db").is_err());
        assert!(RedisConfig::from_url("redis://host:not-a-port").is_err());
    }

    #[test]
    fn never_prints_the_password() {
        let config = RedisConfig::from_url("redis://ada:hunter2@host:6380/2").unwrap();
        let shown = config.redacted_url();

        assert!(!shown.contains("hunter2"));
        assert!(shown.contains("ada"));
        assert!(shown.contains("host:6380/2"));
    }
}
