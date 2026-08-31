//! TLS against a real directory.
//!
//! `tests/directory.rs` runs against a container with no certificate, which
//! means it proves the *refusals* work and nothing about the encryption. These
//! prove the other half: that a directory accepts what this client sends in a
//! handshake, that StartTLS actually upgrades a socket that started in the
//! clear, and that certificate verification is a real check rather than a flag
//! nobody exercised.
//!
//! That last one is the reason this file exists separately. A TLS client whose
//! verification has never been tested against a certificate it should reject is
//! a TLS client that might be accepting everything.
//!
//! They run only when the matching variable is set, so `cargo test` stays green
//! on a machine with no directory.
//!
//! ```text
//! # A self-signed certificate that names localhost and 127.0.0.1:
//! mkdir -p /tmp/rustlavel-ldap-certs && cd /tmp/rustlavel-ldap-certs
//! openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
//!   -keyout server.key -out server.crt -subj "/CN=localhost" \
//!   -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
//!   -addext "basicConstraints=CA:FALSE" \
//!   -addext "extendedKeyUsage=serverAuth"
//! cp server.crt ca.crt && chmod 644 *
//!
//! # The same image as tests/directory.rs, with TLS turned on. It listens on
//! # 1636 for LDAPS and 1389 for StartTLS, both as a non-root user.
//! docker run -d --name rustlavel-ldaps -p 3636:1636 -p 3390:1389 \
//!   -v /tmp/rustlavel-ldap-certs:/certs:ro \
//!   -e LDAP_ROOT=dc=example,dc=test \
//!   -e LDAP_ADMIN_USERNAME=admin \
//!   -e LDAP_ADMIN_PASSWORD=secret \
//!   -e LDAP_USERS=alice,bob \
//!   -e LDAP_PASSWORDS=alicepass,bobpass \
//!   -e LDAP_ENABLE_TLS=yes \
//!   -e LDAP_TLS_CERT_FILE=/certs/server.crt \
//!   -e LDAP_TLS_KEY_FILE=/certs/server.key \
//!   -e LDAP_TLS_CA_FILE=/certs/ca.crt \
//!   bitnamilegacy/openldap:latest
//!
//! export LDAPS_TEST_URL='ldaps://localhost:3636'
//! export LDAP_STARTTLS_TEST_URL='ldap://localhost:3390'
//! cargo test -p rustlavel-ldap
//!
//! docker rm -f rustlavel-ldaps
//! ```
//!
//! The certificate is self-signed, so it does not chain to anything
//! webpki-roots knows. Every test that expects a *successful* handshake asks
//! for [`LdapConfig::dangerously_accept_any_certificate`] — which is precisely
//! what the last test proves is necessary.

use rustlavel_ldap::prelude::*;

macro_rules! url {
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

const BASE: &str = "dc=example,dc=test";
const ADMIN: &str = "cn=admin,dc=example,dc=test";
const ADMIN_PASSWORD: &str = "secret";

/// LDAPS: encrypted from the first byte, and no plaintext opt-in anywhere.
#[tokio::test]
async fn ldaps_encrypts_before_the_password_and_needs_no_opt_in() {
    let url = url!("LDAPS_TEST_URL");

    let config = LdapConfig::parse(&url).unwrap().dangerously_accept_any_certificate();
    assert_eq!(config.encryption, Encryption::Ldaps);
    assert!(!config.allow_plaintext_password, "and it must not need to be");

    let mut connection = LdapConnection::connect(&config).await.unwrap();
    assert!(connection.is_encrypted(), "ldaps:// must be encrypted before anything is sent");

    // The bind goes through with no opt-in: an encrypted connection satisfies
    // the transport rule by itself, which is the point of the rule.
    let result = connection.bind(ADMIN, ADMIN_PASSWORD).await.unwrap();
    assert!(result.is_success(), "{}", result.code);

    let outcome = connection
        .search(&SearchRequest::new(BASE, Filter::equals("uid", "alice")).attributes(["uid"]))
        .await
        .unwrap();
    assert_eq!(outcome.entries.len(), 1);
    assert_eq!(outcome.entries[0].value("uid"), Some("alice"));

    connection.unbind().await.unwrap();
}

/// StartTLS: the connection begins in the clear and is upgraded before a
/// password exists on it.
#[tokio::test]
async fn start_tls_upgrades_a_connection_that_began_in_the_clear() {
    let url = url!("LDAP_STARTTLS_TEST_URL");

    let config = LdapConfig::parse(&url).unwrap().dangerously_accept_any_certificate();
    assert_eq!(config.encryption, Encryption::StartTls, "`ldap://` defaults to the upgrade");

    let mut connection = LdapConnection::connect(&config).await.unwrap();
    assert!(
        connection.is_encrypted(),
        "the socket started as plain TCP; if it is still plain, the extended request went out \
         and the handshake did not happen"
    );

    let result = connection.bind(ADMIN, ADMIN_PASSWORD).await.unwrap();
    assert!(result.is_success(), "{}", result.code);

    connection.unbind().await.unwrap();
}

/// Authentication end to end over StartTLS, which is the shape a real
/// deployment has.
#[tokio::test]
async fn a_user_authenticates_over_an_upgraded_connection() {
    let url = url!("LDAP_STARTTLS_TEST_URL");

    let directory =
        Directory::from_config(LdapConfig::parse(&url).unwrap().dangerously_accept_any_certificate())
            .service_account(ADMIN, ADMIN_PASSWORD)
            .base_dn(BASE)
            .username_attribute("uid")
            .attributes(["cn", "uid"]);

    let user = directory.authenticate("alice", "alicepass").await.unwrap();
    assert_eq!(user.dn(), "cn=alice,ou=users,dc=example,dc=test");

    let error = directory.authenticate("alice", "wrong").await.unwrap_err();
    assert!(matches!(error, AuthenticationError::InvalidPassword), "got {error}");
}

/// The test that makes the other two mean something.
///
/// The certificate above is self-signed, so it chains to nothing webpki-roots
/// knows. With verification left on — the default — the handshake has to fail.
/// If it does not, then `dangerously_accept_any_certificate` is not the switch
/// it claims to be, and every "encrypted" connection in this package is
/// encrypted to whoever answered.
#[tokio::test]
async fn a_self_signed_certificate_is_refused_unless_it_is_explicitly_accepted() {
    let url = url!("LDAPS_TEST_URL");

    let config = LdapConfig::parse(&url).unwrap();
    assert!(config.verify_certificate, "verification is the default");

    let error = match LdapConnection::connect(&config).await {
        Ok(connection) => panic!(
            "a self-signed certificate was accepted with verification on (encrypted: {})",
            connection.is_encrypted()
        ),
        Err(error) => error.to_string(),
    };

    assert!(error.contains("TLS handshake"), "got {error}");
    assert!(
        error.contains("dangerously_accept_any_certificate"),
        "the error should name the deliberate way out, not suggest turning encryption off: \
         {error}"
    );

    // And the same for StartTLS, where the failure happens after the directory
    // has already agreed to upgrade — a different code path with the same rule.
    let url = url!("LDAP_STARTTLS_TEST_URL");
    let error = match LdapConnection::connect(&LdapConfig::parse(&url).unwrap()).await {
        Ok(_) => panic!("a self-signed certificate was accepted over StartTLS"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("TLS handshake"), "got {error}");
}
