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
        check_env_file(&project.root),
        check_app_key(&env),
        check_port(&env),
        check_database(&env),
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

fn check_database(env: &std::collections::BTreeMap<String, String>) -> Check {
    let Some(url) = env.get("DATABASE_URL").filter(|u| !u.is_empty()) else {
        return Check::pass("Database", "not configured (no DATABASE_URL)");
    };

    // Parsing the URL here keeps the check dependency-free; reachability is
    // proved by opening a TCP connection, without speaking the protocol.
    let after_scheme = url.split("://").nth(1).unwrap_or("");
    let authority = after_scheme.rsplit('@').next().unwrap_or("");
    let host_port = authority.split('/').next().unwrap_or("");
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => (host, port),
        None => (host_port, "5432"),
    };

    if host.is_empty() {
        return Check::fail("Database", "DATABASE_URL is malformed", "Expected postgres://user:password@host:port/database");
    }

    match std::net::TcpStream::connect_timeout(
        &format!("{host}:{port}")
            .parse()
            .or_else(|_| resolve(host, port))
            .map_err(|_| ())
            .unwrap_or_else(|_| ([127, 0, 0, 1], 5432).into()),
        std::time::Duration::from_secs(3),
    ) {
        Ok(_) => Check::pass("Database", format!("{host}:{port} is reachable")),
        Err(e) => Check::fail(
            "Database",
            format!("cannot reach {host}:{port} ({e})"),
            "Start PostgreSQL, or point DATABASE_URL at a server that is running.",
        ),
    }
}

fn resolve(host: &str, port: &str) -> Result<std::net::SocketAddr, std::io::Error> {
    use std::net::ToSocketAddrs;
    (host, port.parse::<u16>().unwrap_or(5432))
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("no address"))
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
        let dir = std::env::temp_dir().join("rustlavel-doctor-env");
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
        let dir = std::env::temp_dir().join("rustlavel-doctor-missing");
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
    fn a_free_port_passes() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("SERVER_PORT".to_string(), "0".to_string());

        // Port 0 always binds, which is what makes this test deterministic.
        assert!(check_port(&env).outcome == Outcome::Ok);
    }
}
