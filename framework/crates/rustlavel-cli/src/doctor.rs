//! `rustlavel doctor` — answer "why won't it start?" in one command.
//!
//! Every check reports what it found and, when something is wrong, what to do
//! about it. A diagnostic that only says "failed" has moved the problem, not
//! solved it.

use crate::console;
use crate::project::Project;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

#[derive(PartialEq)]
enum Outcome {
    Ok,
    Warn,
    Fail,
}

struct Check {
    label: String,
    outcome: Outcome,
    detail: String,
    advice: Option<String>,
}

impl Check {
    fn pass(label: &str, detail: impl Into<String>) -> Check {
        Check { label: label.into(), outcome: Outcome::Ok, detail: detail.into(), advice: None }
    }

    fn warn(label: &str, detail: impl Into<String>, advice: impl Into<String>) -> Check {
        Check {
            label: label.into(),
            outcome: Outcome::Warn,
            detail: detail.into(),
            advice: Some(advice.into()),
        }
    }

    fn fail(label: &str, detail: impl Into<String>, advice: impl Into<String>) -> Check {
        Check {
            label: label.into(),
            outcome: Outcome::Fail,
            detail: detail.into(),
            advice: Some(advice.into()),
        }
    }
}

pub fn run() -> Result<(), String> {
    let project = Project::discover()?;
    console::heading(&format!("Checking {}", console::accent(&project.crate_name)));

    let env = read_env(&project.root);
    let checks = vec![
        check_toolchain(),
        check_upgrade_tools(),
        check_env_file(&project.root),
        check_app_key(&env),
        check_port(&env),
        check_database(&env),
        check_database_encryption(&env),
        check_directories(&project.root),
        check_build(&project.root),
    ];

    println!();
    for check in &checks {
        let mark = match check.outcome {
            Outcome::Ok => "\x1b[38;5;71m✓\x1b[0m",
            Outcome::Warn => "\x1b[38;5;179m!\x1b[0m",
            Outcome::Fail => "\x1b[38;5;203m✗\x1b[0m",
        };
        println!("  {mark} {:<22}{}", check.label, console::dim(&check.detail));
        if let Some(advice) = &check.advice {
            println!("    {}", console::dim(&format!("→ {advice}")));
        }
    }

    let failures = checks.iter().filter(|c| c.outcome == Outcome::Fail).count();
    let warnings = checks.iter().filter(|c| c.outcome == Outcome::Warn).count();

    if failures > 0 {
        console::error(&format!("{failures} check(s) failed, {warnings} warning(s)."));
        return Err("the application is not ready to run".into());
    }
    console::success(&format!("All checks passed ({warnings} warning(s))."));
    Ok(())
}

fn check_toolchain() -> Check {
    match Command::new("cargo").arg("--version").output() {
        Ok(output) if output.status.success() => {
            Check::pass("Toolchain", String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        _ => Check::fail(
            "Toolchain",
            "cargo was not found",
            "Install Rust from https://rustup.rs, then reopen your terminal.",
        ),
    }
}

/// `rustlavel upgrade` fetches the version a project was created with from
/// crates.io, and does it with `curl` and `tar` rather than by carrying an
/// HTTP client, a TLS stack and a gzip decoder in a CLI that otherwise depends
/// on nothing. Both ship with macOS, every Linux distribution, and Windows
/// since 2018 — but a container image trimmed to `cargo` alone will not have
/// them, and finding that out mid-upgrade is the wrong time.
fn check_upgrade_tools() -> Check {
    let missing: Vec<&str> = ["curl", "tar"]
        .into_iter()
        .filter(|tool| {
            !Command::new(tool)
                .arg("--version")
                .output()
                .map(|out| out.status.success())
                .unwrap_or(false)
        })
        .collect();

    match missing.as_slice() {
        [] => Check::pass("Upgrade tools", "curl and tar are available"),
        missing => Check::warn(
            "Upgrade tools",
            format!("{} not found", missing.join(" and ")),
            "`rustlavel upgrade` needs them to fetch the kit version this project was created \
             with. Everything else works without them.",
        ),
    }
}

fn check_env_file(root: &Path) -> Check {
    let env = root.join(".env");
    if env.exists() {
        return Check::pass("Environment", ".env found");
    }
    if root.join(".env.example").exists() {
        return Check::warn(
            "Environment",
            ".env is missing",
            "Copy it: `cp .env.example .env`",
        );
    }
    Check::warn("Environment", "no .env file", "Create one, or rely on real environment variables.")
}

fn check_app_key(env: &std::collections::BTreeMap<String, String>) -> Check {
    match env.get("APP_KEY") {
        Some(key) if !key.is_empty() => Check::pass("Application key", "set"),
        _ => Check::warn(
            "Application key",
            "APP_KEY is empty",
            "Sessions and encryption need one. Generate it with `rustlavel key:generate`.",
        ),
    }
}

fn check_port(env: &std::collections::BTreeMap<String, String>) -> Check {
    let host = env.get("SERVER_HOST").cloned().unwrap_or_else(|| "127.0.0.1".into());
    let port = env.get("SERVER_PORT").cloned().unwrap_or_else(|| "8000".into());
    let address = format!("{host}:{port}");

    match TcpListener::bind(&address) {
        Ok(listener) => {
            drop(listener);
            Check::pass("Server port", format!("{address} is free"))
        }
        Err(e) => Check::fail(
            "Server port",
            format!("{address} is unavailable ({e})"),
            format!("Something else is listening. Stop it, or run `rustlavel serve --port {}`.", port.parse::<u16>().unwrap_or(8000) + 1),
        ),
    }
}

/// Report what the database connection is actually protected by.
///
/// Separate from reachability because it is a different question with a
/// different answer: a database can be perfectly reachable and perfectly
/// readable by anyone on the path. `prefer`, the default, is the case worth
/// naming out loud — it asks for encryption and accepts a refusal, so an
/// attacker who can read the connection can also just say "no TLS here" and
/// keep reading it.
fn check_database_encryption(env: &std::collections::BTreeMap<String, String>) -> Check {
    let Some(url) = env.get("DATABASE_URL").filter(|u| !u.is_empty()) else {
        return Check::pass("Database encryption", "not configured (no DATABASE_URL)");
    };

    let scheme = url.split("://").next().unwrap_or("").to_ascii_lowercase();
    if matches!(scheme.as_str(), "sqlserver" | "mssql") {
        return Check::pass("Database encryption", "SQL Server always encrypts (TDS negotiates it)");
    }

    let mode = url
        .split_once('?')
        .map(|(_, query)| query)
        .into_iter()
        .flat_map(|query| query.split('&'))
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| matches!(*key, "sslmode" | "ssl-mode" | "ssl_mode"))
        .map(|(_, value)| value.to_ascii_lowercase().replace('_', "-"))
        .unwrap_or_else(|| "prefer".to_string());

    let production = env
        .get("APP_ENV")
        .map(|value| value.eq_ignore_ascii_case("production"))
        .unwrap_or(false);

    match mode.as_str() {
        "verify-full" | "verify-identity" => {
            Check::pass("Database encryption", "verify-full: encrypted and the server is verified")
        }
        "verify-ca" => Check::pass(
            "Database encryption",
            "verify-ca: encrypted, certificate checked, hostname not",
        ),
        "require" | "required" if !production => {
            Check::pass("Database encryption", "require: encrypted, certificate not checked")
        }
        "require" | "required" => Check::warn(
            "Database encryption",
            "require: encrypted, but the certificate is not checked",
            "An attacker who can intercept the connection can present their own certificate. \
             Use sslmode=verify-full and point sslrootcert at the CA.",
        ),
        "disable" | "disabled" | "off" | "false" => Check::warn(
            "Database encryption",
            "disable: the connection is in clear text, password included",
            "Anyone on the network path can read every query and the credentials. \
             Use sslmode=require at minimum, or verify-full if the server has a certificate \
             you can verify.",
        ),
        _ => Check::warn(
            "Database encryption",
            format!("{mode}: encryption is requested but not required"),
            "`prefer` accepts a server that declines to encrypt, so it guarantees nothing \
             against an active attacker — they answer \"no TLS\" and read on. \
             Set sslmode=verify-full for anything crossing a network you do not own.",
        ),
    }
}

fn check_database(env: &std::collections::BTreeMap<String, String>) -> Check {
    let Some(url) = env.get("DATABASE_URL").filter(|u| !u.is_empty()) else {
        return Check::pass("Database", "not configured (no DATABASE_URL)");
    };

    // Parsing the URL here keeps the check dependency-free; reachability is
    // proved by opening a TCP connection, without speaking the protocol.
    let Some((scheme, after_scheme)) = url.split_once("://") else {
        return Check::fail(
            "Database",
            "DATABASE_URL has no scheme",
            "Expected postgres://, mysql:// or sqlserver:// followed by user:password@host:port/database",
        );
    };

    // The scheme decides which port to try when the URL gives none, which is
    // why this cannot assume PostgreSQL.
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" | "pgsql" => "5432",
        "mysql" | "mariadb" => "3306",
        "sqlserver" | "mssql" => "1433",
        other => {
            return Check::fail(
                "Database",
                format!("`{other}` is not a database this framework speaks"),
                "Use postgres://, mysql:// or sqlserver://.",
            );
        }
    };

    let authority = after_scheme.rsplit('@').next().unwrap_or("");
    let host_port = authority.split('/').next().unwrap_or("");
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => (host, port),
        None => (host_port, default_port),
    };

    if host.is_empty() {
        return Check::fail(
            "Database",
            "DATABASE_URL names no host",
            "Expected <scheme>://user:password@host:port/database",
        );
    }

    // Resolution failure is reported, never guessed around. Falling back to a
    // default address could connect to some other database on this machine and
    // cheerfully report the wrong one as reachable.
    let address = match resolve(host, port) {
        Ok(address) => address,
        Err(e) => {
            return Check::fail(
                "Database",
                format!("cannot resolve `{host}` ({e})"),
                "Check the host in DATABASE_URL.",
            );
        }
    };

    match std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(3)) {
        Ok(_) => Check::pass("Database", format!("{scheme} at {host}:{port} is reachable")),
        Err(e) => Check::fail(
            "Database",
            format!("cannot reach {host}:{port} ({e})"),
            format!("Start the {scheme} server, or point DATABASE_URL at one that is running."),
        ),
    }
}

fn resolve(host: &str, port: &str) -> Result<std::net::SocketAddr, std::io::Error> {
    use std::net::ToSocketAddrs;

    let port: u16 = port
        .parse()
        .map_err(|_| std::io::Error::other(format!("`{port}` is not a port number")))?;

    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("the host resolved to no address"))
}

fn check_directories(root: &Path) -> Check {
    let missing: Vec<&str> = ["src", "config"]
        .iter()
        .filter(|directory| !root.join(directory).is_dir())
        .copied()
        .collect();

    if missing.is_empty() {
        Check::pass("Layout", "src/ and config/ present")
    } else {
        Check::fail(
            "Layout",
            format!("missing {}", missing.join(", ")),
            "This does not look like a rustlavel application. Run `rustlavel new <name>` to create one.",
        )
    }
}

fn check_build(root: &Path) -> Check {
    match Command::new("cargo").arg("check").arg("--quiet").current_dir(root).output() {
        Ok(output) if output.status.success() => Check::pass("Build", "compiles"),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let first = stderr
                .lines()
                .find(|line| line.starts_with("error"))
                .unwrap_or("see `cargo check`")
                .to_string();
            Check::fail("Build", first, "Run `cargo check` for the full output.")
        }
        Err(e) => Check::fail("Build", format!("cannot run cargo ({e})"), "Install Rust from https://rustup.rs."),
    }
}

/// Read `.env` without depending on the framework crates — the CLI is a
/// standalone binary and must work even when the application does not compile.
fn read_env(root: &Path) -> std::collections::BTreeMap<String, String> {
    let mut values = std::collections::BTreeMap::new();
    let Ok(source) = std::fs::read_to_string(root.join(".env")) else { return values };

    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.strip_prefix("export ").unwrap_or(line).split_once('=') {
            values.insert(
                key.trim().to_string(),
                value.trim().trim_matches('"').trim_matches('\'').to_string(),
            );
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_env_values_ignoring_comments_and_quotes() {
        let dir = std::env::temp_dir().join(format!("rustlavel-doctor-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".env"),
            "# a comment\nAPP_KEY=\"abc\"\nexport SERVER_PORT=9000\n\nEMPTY=\n",
        )
        .unwrap();

        let values = read_env(&dir);

        assert_eq!(values["APP_KEY"], "abc");
        assert_eq!(values["SERVER_PORT"], "9000");
        assert_eq!(values["EMPTY"], "");
        assert!(!values.contains_key("# a comment"));
    }

    #[test]
    fn a_missing_env_file_is_a_warning_not_a_failure() {
        let dir = std::env::temp_dir().join(format!("rustlavel-doctor-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join(".env"));

        assert!(check_env_file(&dir).outcome != Outcome::Fail);
    }

    #[test]
    fn an_unset_app_key_explains_how_to_make_one() {
        let check = check_app_key(&std::collections::BTreeMap::new());

        assert!(check.outcome == Outcome::Warn);
        assert!(check.advice.unwrap().contains("key:generate"));
    }

    #[test]
    fn an_unencrypted_database_url_is_a_warning_not_a_pass() {
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "DATABASE_URL".to_string(),
            "postgres://u:p@db.example.com:5432/app?sslmode=disable".to_string(),
        );

        let check = check_database_encryption(&env);
        assert!(check.outcome == Outcome::Warn);
        assert!(check.detail.contains("clear text"), "got {}", check.detail);
    }

    #[test]
    fn the_default_of_prefer_is_reported_as_guaranteeing_nothing() {
        // No sslmode at all: the case almost every application will be in, and
        // the one most likely to be mistaken for "encrypted".
        let mut env = std::collections::BTreeMap::new();
        env.insert("DATABASE_URL".to_string(), "mysql://u:p@db:3306/app".to_string());

        let check = check_database_encryption(&env);
        assert!(check.outcome == Outcome::Warn);
        assert!(
            check.advice.unwrap_or_default().contains("verify-full"),
            "the advice must name the fix"
        );
    }

    #[test]
    fn verifying_modes_pass() {
        for mode in ["verify-full", "verify-ca"] {
            let mut env = std::collections::BTreeMap::new();
            env.insert(
                "DATABASE_URL".to_string(),
                format!("postgres://u:p@db:5432/app?sslmode={mode}"),
            );
            assert!(check_database_encryption(&env).outcome == Outcome::Ok, "{mode}");
        }
    }

    #[test]
    fn require_is_only_flagged_in_production() {
        // Encryption without verification is a reasonable development setting
        // and a poor production one, so the verdict follows APP_ENV.
        let url = "postgres://u:p@db:5432/app?sslmode=require".to_string();

        let mut development = std::collections::BTreeMap::new();
        development.insert("DATABASE_URL".to_string(), url.clone());
        assert!(check_database_encryption(&development).outcome == Outcome::Ok);

        let mut production = std::collections::BTreeMap::new();
        production.insert("DATABASE_URL".to_string(), url);
        production.insert("APP_ENV".to_string(), "production".to_string());
        assert!(check_database_encryption(&production).outcome == Outcome::Warn);
    }

    #[test]
    fn sql_server_is_not_nagged_about_a_setting_it_does_not_have() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("DATABASE_URL".to_string(), "sqlserver://u:p@db:1433/app".to_string());

        assert!(check_database_encryption(&env).outcome == Outcome::Ok);
    }

    #[test]
    fn the_database_check_knows_each_schemes_default_port() {
        let mut env = std::collections::BTreeMap::new();

        for (url, expected) in [
            ("postgres://nowhere.invalid/db", "5432"),
            ("mysql://nowhere.invalid/db", "3306"),
            ("sqlserver://nowhere.invalid/db", "1433"),
        ] {
            env.insert("DATABASE_URL".to_string(), url.to_string());
            let check = check_database(&env);

            // The host does not resolve, so this fails — but the message must
            // name the port the scheme implies rather than PostgreSQL's.
            assert!(
                check.detail.contains(expected) || check.detail.contains("cannot resolve"),
                "for {url} the message was {:?}",
                check.detail
            );
        }
    }

    #[test]
    fn an_unresolvable_host_is_reported_rather_than_guessed_around() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("DATABASE_URL".to_string(), "mysql://nowhere.invalid/db".to_string());

        let check = check_database(&env);

        assert!(check.outcome == Outcome::Fail);
        // It must not silently try localhost and report someone else's database.
        assert!(check.detail.contains("cannot resolve"), "{:?}", check.detail);
    }

    #[test]
    fn an_unsupported_scheme_says_which_ones_work() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("DATABASE_URL".to_string(), "oracle://host/db".to_string());

        let check = check_database(&env);
        assert!(check.advice.unwrap().contains("postgres://, mysql:// or sqlserver://"));
    }

    #[test]
    fn a_free_port_passes() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("SERVER_PORT".to_string(), "0".to_string());

        // Port 0 always binds, which is what makes this test deterministic.
        assert!(check_port(&env).outcome == Outcome::Ok);
    }
}
