//! Connection settings, from a URL or from the application's configuration.

use rustlavel_core::{Config, Error, Result};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    pub application_name: String,
    /// Connections kept open by the pool.
    pub max_connections: usize,
    pub connect_timeout: Duration,
    /// How long a query may run before the driver gives up on it.
    pub query_timeout: Duration,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        DatabaseConfig {
            host: "127.0.0.1".into(),
            port: 5432,
            user: "postgres".into(),
            password: String::new(),
            database: "postgres".into(),
            application_name: "rustlavel".into(),
            max_connections: 10,
            connect_timeout: Duration::from_secs(10),
            query_timeout: Duration::from_secs(30),
        }
    }
}

impl DatabaseConfig {
    /// Parse `postgres://user:password@host:port/database`.
    ///
    /// User, password and database are percent-decoded, so a password with an
    /// `@` or `/` in it works without the caller escaping anything twice.
    pub fn from_url(url: &str) -> Result<Self> {
        let rest = url
            .strip_prefix("postgres://")
            .or_else(|| url.strip_prefix("postgresql://"))
            .ok_or_else(|| {
                Error::msg(format!(
                    "`{url}` is not a PostgreSQL URL. Expected postgres://user:password@host:port/database"
                ))
            })?;

        let mut config = DatabaseConfig::default();

        // Split off the query string before anything else, so `?` inside it
        // cannot be mistaken for part of the database name.
        let (rest, query) = match rest.split_once('?') {
            Some((rest, query)) => (rest, Some(query)),
            None => (rest, None),
        };

        // The last `@` separates credentials from the host, which is what makes
        // an `@` inside a password unambiguous.
        let (credentials, host_part) = match rest.rsplit_once('@') {
            Some((credentials, host)) => (Some(credentials), host),
            None => (None, rest),
        };

        if let Some(credentials) = credentials {
            let (user, password) = match credentials.split_once(':') {
                Some((user, password)) => (user, password),
                None => (credentials, ""),
            };
            if !user.is_empty() {
                config.user = decode(user);
            }
            config.password = decode(password);
        }

        let (host, database) = match host_part.split_once('/') {
            Some((host, database)) => (host, database),
            None => (host_part, ""),
        };

        if !database.is_empty() {
            config.database = decode(database);
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
                "application_name" => config.application_name = decode(value),
                "max_connections" => {
                    config.max_connections = value.parse().unwrap_or(config.max_connections)
                }
                "connect_timeout" => {
                    if let Ok(seconds) = value.parse() {
                        config.connect_timeout = Duration::from_secs(seconds);
                    }
                }
                _ => {}
            }
        }

        Ok(config)
    }

    /// Read from the application config, falling back to `DATABASE_URL`.
    pub fn from_app_config(config: &Config) -> Result<Self> {
        if let Some(url) = config.get("database.url").and_then(|v| v.as_str().map(str::to_string))
            && !url.is_empty() {
                return DatabaseConfig::from_url(&url);
            }
        if let Ok(url) = std::env::var("DATABASE_URL")
            && !url.is_empty() {
                return DatabaseConfig::from_url(&url);
            }

        let mut settings = DatabaseConfig {
            host: config.string("database.host", "127.0.0.1"),
            port: config.int("database.port", 5432) as u16,
            user: config.string("database.user", "postgres"),
            password: config.string("database.password", ""),
            database: config.string("database.name", "postgres"),
            ..DatabaseConfig::default()
        };
        settings.max_connections = config.int("database.max_connections", 10).max(1) as usize;
        settings.application_name = config.string("app.name", "rustlavel");
        Ok(settings)
    }

    /// The URL with the password removed, for logs and error messages.
    pub fn redacted_url(&self) -> String {
        let password = if self.password.is_empty() { "" } else { ":***" };
        format!(
            "postgres://{}{password}@{}:{}/{}",
            self.user, self.host, self.port, self.database
        )
    }
}

fn decode(value: &str) -> String {
    if !value.contains('%') {
        return value.to_string();
    }

    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
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
    fn parses_a_full_url() {
        let config = DatabaseConfig::from_url("postgres://ada:hunter2@db.internal:6543/blog").unwrap();

        assert_eq!(config.user, "ada");
        assert_eq!(config.password, "hunter2");
        assert_eq!(config.host, "db.internal");
        assert_eq!(config.port, 6543);
        assert_eq!(config.database, "blog");
    }

    #[test]
    fn falls_back_to_defaults_for_missing_parts() {
        let config = DatabaseConfig::from_url("postgres://localhost/blog").unwrap();

        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 5432);
        assert_eq!(config.user, "postgres");
        assert_eq!(config.database, "blog");
    }

    #[test]
    fn a_password_may_contain_an_at_sign() {
        let config = DatabaseConfig::from_url("postgres://ada:p%40ss@host/blog").unwrap();
        assert_eq!(config.password, "p@ss");
    }

    #[test]
    fn reads_query_parameters() {
        let config =
            DatabaseConfig::from_url("postgres://host/blog?application_name=worker&max_connections=25")
                .unwrap();

        assert_eq!(config.application_name, "worker");
        assert_eq!(config.max_connections, 25);
    }

    #[test]
    fn rejects_a_url_with_the_wrong_scheme() {
        let error = DatabaseConfig::from_url("mysql://host/blog").unwrap_err();
        assert!(error.to_string().contains("not a PostgreSQL URL"));
    }

    #[test]
    fn never_prints_the_password() {
        let config = DatabaseConfig::from_url("postgres://ada:hunter2@host/blog").unwrap();
        let shown = config.redacted_url();

        assert!(!shown.contains("hunter2"));
        assert!(shown.contains("ada"));
    }
}
