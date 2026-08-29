//! Connection settings, from a URL or from the application's configuration.

use rustlavel_core::{Config, Error, Result};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    /// Which database this points at: `postgres`, `mysql`, `sqlserver`.
    pub driver: String,
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
    /// Whether to encrypt the connection, and how much of the certificate to
    /// believe. See [`crate::tls::TlsMode`] — the default asks for encryption
    /// but accepts a server that declines, which guarantees nothing.
    pub tls_mode: crate::tls::TlsMode,
    /// A PEM file of trust anchors, for `verify-ca` and `verify-full` against a
    /// private CA. `None` uses the public roots.
    pub tls_root_certificate: Option<String>,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        DatabaseConfig {
            driver: "postgres".into(),
            host: "127.0.0.1".into(),
            port: 5432,
            user: "postgres".into(),
            password: String::new(),
            database: "postgres".into(),
            application_name: "rustlavel".into(),
            max_connections: 10,
            connect_timeout: Duration::from_secs(10),
            query_timeout: Duration::from_secs(30),
            tls_mode: crate::tls::TlsMode::default(),
            tls_root_certificate: None,
        }
    }
}

impl DatabaseConfig {
    /// Parse a database URL.
    ///
    /// The scheme chooses the driver: `postgres://`, `mysql://` or
    /// `sqlserver://`. User, password and database are percent-decoded, so a
    /// password with an `@` or `/` in it works without escaping anything twice.
    pub fn from_url(url: &str) -> Result<Self> {
        let (scheme, rest) = url.split_once("://").ok_or_else(|| {
            Error::msg(format!(
                "`{url}` has no scheme. Expected postgres://, mysql:// or sqlserver:// \
                 followed by user:password@host:port/database"
            ))
        })?;

        // The scheme names the database, and the default port follows from it,
        // because nobody remembers 1433.
        let (driver, default_port) = match scheme.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "pgsql" => ("postgres", 5432),
            "mysql" | "mariadb" => ("mysql", 3306),
            "sqlserver" | "mssql" => ("sqlserver", 1433),
            other => {
                return Err(Error::msg(format!(
                    "`{other}` is not a database this framework speaks. \
                     Available schemes: postgres, mysql, sqlserver."
                )));
            }
        };

        let mut config = DatabaseConfig {
            driver: driver.to_string(),
            port: default_port,
            ..DatabaseConfig::default()
        };

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
                // An unreadable sslmode is an error rather than a shrug: the
                // silent fallback would be a weaker mode than the one asked
                // for, which is the wrong way round for a security setting.
                "sslmode" | "ssl-mode" | "ssl_mode" => {
                    config.tls_mode = crate::tls::TlsMode::parse(&decode(value))?
                }
                "sslrootcert" | "ssl-ca" | "ssl_ca" => {
                    config.tls_root_certificate = Some(decode(value))
                }
                _ => {}
            }
        }

        Ok(config)
    }

    /// The dialect this configuration implies.
    pub fn dialect(&self) -> Result<Box<dyn crate::dialect::Dialect>> {
        crate::dialect::by_name(&self.driver)
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
            driver: config.string("database.driver", "postgres"),
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
            "{}://{}{password}@{}:{}/{}",
            self.driver, self.user, self.host, self.port, self.database
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
    fn the_scheme_chooses_the_driver_and_its_default_port() {
        for (url, driver, port) in [
            ("postgres://host/blog", "postgres", 5432),
            ("postgresql://host/blog", "postgres", 5432),
            ("mysql://host/blog", "mysql", 3306),
            ("mariadb://host/blog", "mysql", 3306),
            ("sqlserver://host/blog", "sqlserver", 1433),
            ("mssql://host/blog", "sqlserver", 1433),
        ] {
            let config = DatabaseConfig::from_url(url).unwrap();
            assert_eq!(config.driver, driver, "for {url}");
            assert_eq!(config.port, port, "for {url}");
        }
    }

    #[test]
    fn an_explicit_port_still_wins_over_the_default() {
        assert_eq!(DatabaseConfig::from_url("mysql://host:3307/blog").unwrap().port, 3307);
    }

    #[test]
    fn a_configuration_knows_its_dialect() {
        for (url, dialect) in [
            ("postgres://host/b", "postgres"),
            ("mysql://host/b", "mysql"),
            ("sqlserver://host/b", "sqlserver"),
        ] {
            assert_eq!(
                DatabaseConfig::from_url(url).unwrap().dialect().unwrap().name(),
                dialect
            );
        }
    }

    #[test]
    fn an_unsupported_database_lists_the_ones_that_work() {
        let error = DatabaseConfig::from_url("oracle://host/blog").unwrap_err().to_string();
        assert!(error.contains("postgres, mysql, sqlserver"), "{error}");

        let missing = DatabaseConfig::from_url("just-a-host/blog").unwrap_err().to_string();
        assert!(missing.contains("has no scheme"), "{missing}");
    }

    #[test]
    fn never_prints_the_password() {
        for url in [
            "postgres://ada:hunter2@host/blog",
            "mysql://ada:hunter2@host/blog",
            "sqlserver://ada:hunter2@host/blog",
        ] {
            let shown = DatabaseConfig::from_url(url).unwrap().redacted_url();

            assert!(!shown.contains("hunter2"), "the password leaked into {shown}");
            assert!(shown.contains("ada"));
        }
    }
}
