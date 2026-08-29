//! TLS against real servers.
//!
//! The unit tests assert what the negotiation *bytes* look like. These assert
//! that a server accepts them and that the connection is genuinely encrypted
//! afterwards — a distinction this project has already paid for once, when a
//! full suite of green unit tests hid eight bugs that only appeared against a
//! live database.
//!
//! They run only when the matching variable is set, so `cargo test` stays green
//! on a machine with no database.
//!
//! ```text
//! # PostgreSQL, with a CA and a certificate that names localhost:
//! docker run -d --name rustlavel-pg-tls -e POSTGRES_PASSWORD=secret \
//!   -e POSTGRES_USER=rustlavel -e POSTGRES_DB=rustlavel_test \
//!   -p 55432:5432 postgres:16
//! docker exec -u postgres rustlavel-pg-tls bash -c '
//!   cd /var/lib/postgresql/data
//!   openssl req -new -x509 -days 3650 -nodes -out ca.crt -keyout ca.key -subj "/CN=rustlavel-test-ca"
//!   openssl req -new -nodes -out server.csr -keyout server.key -subj "/CN=localhost"
//!   openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out server.crt \
//!     -days 3650 -extfile <(printf "subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n")
//!   chmod 600 server.key'
//! docker exec -u postgres rustlavel-pg-tls psql -U rustlavel -d rustlavel_test -c "ALTER SYSTEM SET ssl='on';"
//! docker restart rustlavel-pg-tls
//! docker exec rustlavel-pg-tls cat /var/lib/postgresql/data/ca.crt > /tmp/pg-ca.crt
//! export PG_TLS_URL='postgres://rustlavel:secret@localhost:55432/rustlavel_test'
//! export PG_TLS_CA=/tmp/pg-ca.crt
//!
//! # MySQL generates its own certificate on first start, so it needs no help:
//! docker run -d --name rustlavel-mysql-tls -e MYSQL_ROOT_PASSWORD=secret \
//!   -e MYSQL_DATABASE=rustlavel_test -e MYSQL_USER=rustlavel -e MYSQL_PASSWORD=secret \
//!   -p 33306:3306 mysql:8.4
//! docker exec rustlavel-mysql-tls cat /var/lib/mysql/ca.pem > /tmp/mysql-ca.pem
//! export MYSQL_TLS_URL='mysql://rustlavel:secret@127.0.0.1:33306/rustlavel_test'
//! export MYSQL_TLS_CA=/tmp/mysql-ca.pem
//! ```

use rustlavel_db::prelude::*;

/// The base URL for a server, or a skip.
macro_rules! server {
    ($variable:literal) => {
        match std::env::var($variable) {
            Ok(url) if !url.is_empty() => url,
            _ => {
                eprintln!("skipping: {} is not set", $variable);
                return;
            }
        }
    };
}

fn with(url: &str, query: &str) -> String {
    format!("{url}?{query}")
}

/// The CA path, or a skip — a verification test without one proves nothing.
macro_rules! ca {
    ($variable:literal) => {
        match std::env::var($variable) {
            Ok(path) if !path.is_empty() => path,
            _ => {
                eprintln!("skipping: {} is not set", $variable);
                return;
            }
        }
    };
}

/// Ask the server itself whether it thinks the connection is encrypted.
///
/// Deliberately not `connection.is_encrypted()`: that is this driver's opinion,
/// and the whole point is to confirm the *server* agrees.
async fn postgres_cipher(db: &Database) -> String {
    db.select_one(
        "select coalesce((select version from pg_stat_ssl where pid = pg_backend_pid()), '') as v",
        &[],
    )
    .await
    .expect("the ssl status query should run")
    .expect("one row")
    .get::<String>("v")
    .expect("a version column")
}

async fn mysql_cipher(db: &Database) -> String {
    db.select_one("show status like 'Ssl_cipher'", &[])
        .await
        .expect("the ssl status query should run")
        .expect("one row")
        .get::<String>("Value")
        .unwrap_or_default()
}

#[tokio::test]
async fn postgres_negotiates_tls_when_asked() {
    let url = server!("PG_TLS_URL");
    let db = Database::connect(&with(&url, "sslmode=require")).await.expect("connect");

    let version = postgres_cipher(&db).await;
    assert!(version.starts_with("TLSv"), "the server reported `{version}`, not a TLS version");
}

#[tokio::test]
async fn postgres_stays_in_the_clear_when_told_to() {
    let url = server!("PG_TLS_URL");
    let db = Database::connect(&with(&url, "sslmode=disable")).await.expect("connect");

    assert_eq!(postgres_cipher(&db).await, "", "sslmode=disable must not negotiate TLS");
}

#[tokio::test]
async fn postgres_verifies_the_certificate_against_a_named_ca() {
    let url = server!("PG_TLS_URL");
    let ca = ca!("PG_TLS_CA");

    let db = Database::connect(&with(&url, &format!("sslmode=verify-full&sslrootcert={ca}")))
        .await
        .expect("verify-full should succeed against the CA that signed the server");

    assert!(postgres_cipher(&db).await.starts_with("TLSv"));
}

#[tokio::test]
async fn postgres_refuses_a_certificate_it_cannot_chain() {
    let url = server!("PG_TLS_URL");
    // No sslrootcert, so the only trust anchors are the public roots — which
    // did not sign a certificate generated in a container five minutes ago.
    let error = match Database::connect(&with(&url, "sslmode=verify-full")).await {
        Ok(_) => panic!("verify-full must not accept an unknown issuer"),
        Err(error) => error.to_string(),
    };

    assert!(error.contains("UnknownIssuer"), "got {error}");
    // The error has to say how to fix it, or it sends people to sslmode=disable.
    assert!(error.contains("sslrootcert"), "the error should name the way out: {error}");
}

#[tokio::test]
async fn postgres_encrypts_before_the_password_is_sent() {
    // The ordering that makes this worth doing at all. A wrong password over
    // TLS fails at authentication, which proves the handshake completed before
    // the credentials went anywhere.
    let url = server!("PG_TLS_URL");
    let wrong = url.replace(":secret@", ":wrong-password@");

    let error = match Database::connect(&with(&wrong, "sslmode=require")).await {
        Ok(_) => panic!("a wrong password must fail"),
        Err(error) => error.to_string(),
    };

    assert!(
        error.contains("password") || error.contains("authentication"),
        "the failure should be authentication, not transport: {error}"
    );
}

#[tokio::test]
async fn mysql_negotiates_tls_when_asked() {
    let url = server!("MYSQL_TLS_URL");
    let db = Database::connect(&with(&url, "sslmode=require")).await.expect("connect");

    let cipher = mysql_cipher(&db).await;
    assert!(!cipher.is_empty(), "Ssl_cipher was empty, so the connection is not encrypted");
}

#[tokio::test]
async fn mysql_verifies_the_certificate_against_a_named_ca() {
    let url = server!("MYSQL_TLS_URL");
    let ca = ca!("MYSQL_TLS_CA");

    // verify-ca rather than verify-full: MySQL's auto-generated certificate
    // carries no subjectAltName at all, so no hostname can ever match it. That
    // is a property of the server, not a gap here.
    let db = Database::connect(&with(&url, &format!("sslmode=verify-ca&sslrootcert={ca}")))
        .await
        .expect("verify-ca should succeed against the CA MySQL generated");

    assert!(!mysql_cipher(&db).await.is_empty());
}

#[tokio::test]
async fn mysql_full_authentication_works_once_the_channel_is_private() {
    // caching_sha2_password is MySQL 8's default, and its full path sends the
    // password itself. The driver refuses that in the clear and allows it
    // inside the tunnel — so on a fresh server, where nothing is cached, this
    // connecting at all is the proof that TLS came first.
    let url = server!("MYSQL_TLS_URL");
    let db = Database::connect(&with(&url, "sslmode=require")).await.expect("connect");

    assert!(!mysql_cipher(&db).await.is_empty());
}

#[tokio::test]
async fn a_query_actually_runs_over_the_encrypted_connection() {
    // Encryption that breaks the first real query is not encryption anybody can
    // use. Both drivers reframe every packet after the upgrade, so a payload
    // large enough to span several frames is where a mistake would show.
    for (variable, placeholder) in [("PG_TLS_URL", "$1"), ("MYSQL_TLS_URL", "?")] {
        let Ok(url) = std::env::var(variable).map(|u| u.trim().to_string()) else {
            eprintln!("skipping: {variable} is not set");
            continue;
        };
        if url.is_empty() {
            eprintln!("skipping: {variable} is empty");
            continue;
        }

        let db = Database::connect(&with(&url, "sslmode=require")).await.expect("connect");
        let long = "x".repeat(70_000);

        let row = db
            .select_one(&format!("select {placeholder} as echo"), &[Value::Text(long.clone())])
            .await
            .expect("a round trip over TLS")
            .expect("one row");

        assert_eq!(row.get::<String>("echo").unwrap().len(), long.len(), "on {variable}");
    }
}
