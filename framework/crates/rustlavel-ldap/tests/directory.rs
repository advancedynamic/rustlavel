//! Authentication against a real directory.
//!
//! The unit tests assert what the *bytes* look like. These assert that a real
//! OpenLDAP accepts them — and, where this package refuses to send something,
//! go and find out what the directory would have done with it. The
//! empty-password test builds the forbidden bind by hand and sends it, which
//! is how the comment there ended up recording that OpenLDAP 2.6 refuses it and
//! Active Directory historically does not, rather than repeating what I assumed
//! before running it.
//!
//! They run only when `LDAP_TEST_URL` is set, so `cargo test` stays green on a
//! machine with no directory.
//!
//! ```text
//! # OpenLDAP. Note the image name: Bitnami moved its catalogue to
//! # `bitnamilegacy` in 2025, and `bitnami/openldap` no longer resolves.
//! # The container listens on 1389, not 389 — it runs as a non-root user.
//! docker run -d --name rustlavel-ldap -p 3389:1389 \
//!   -e LDAP_ROOT=dc=example,dc=test \
//!   -e LDAP_ADMIN_USERNAME=admin \
//!   -e LDAP_ADMIN_PASSWORD=secret \
//!   -e LDAP_USERS=alice,bob \
//!   -e LDAP_PASSWORDS=alicepass,bobpass \
//!   bitnamilegacy/openldap:latest
//!
//! # That builds the tree these tests expect:
//! #   dc=example,dc=test
//! #     ou=users        cn=alice (uid: alice), cn=bob (uid: bob)
//! #     ou=groups       cn=readers
//! # and cn=admin,dc=example,dc=test with the password `secret`.
//!
//! export LDAP_TEST_URL='ldap://127.0.0.1:3389'
//! cargo test -p rustlavel-ldap
//!
//! docker rm -f rustlavel-ldap
//! ```
//!
//! The container has no TLS, so every test here opts into plain text twice
//! over: [`LdapConfig::plaintext`] to skip StartTLS, and
//! [`LdapConfig::allow_plaintext_password`] to permit the bind. That two-step
//! is the point — it is exactly as awkward as it should be, and the one test
//! that leaves the second step out proves the refusal fires against a live
//! server rather than only in a unit test.

use rustlavel_ldap::ber::Encoder;
use rustlavel_ldap::prelude::*;
use rustlavel_ldap::protocol::{LdapMessage, ProtocolOp, tags};

/// The directory URL, or a skip.
macro_rules! directory {
    () => {
        match std::env::var("LDAP_TEST_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => {
                eprintln!("skipping: LDAP_TEST_URL is not set");
                return;
            }
        }
    };
}

const BASE: &str = "dc=example,dc=test";
const ADMIN: &str = "cn=admin,dc=example,dc=test";
const ADMIN_PASSWORD: &str = "secret";
const ALICE: &str = "cn=alice,ou=users,dc=example,dc=test";
const ALICE_PASSWORD: &str = "alicepass";

/// Everything the tests need, with plain text allowed because the container
/// has no certificate.
fn directory(url: &str) -> Directory {
    Directory::from_config(
        LdapConfig::parse(url).unwrap().plaintext().allow_plaintext_password(),
    )
    .service_account(ADMIN, ADMIN_PASSWORD)
    .base_dn(BASE)
    .username_attribute("uid")
    .attributes(["cn", "uid", "sn"])
}

#[tokio::test]
async fn a_correct_password_authenticates() {
    let url = directory!();

    let user = directory(&url).authenticate("alice", ALICE_PASSWORD).await.unwrap();

    assert_eq!(user.dn(), ALICE);
    assert_eq!(user.value("uid"), Some("alice"));
    // The entry has two `cn` values; both come back.
    assert!(user.values("cn").contains(&"alice"), "got {:?}", user.values("cn"));
    // And the identifier a session would remember is the DN, not the username.
    assert_eq!(
        rustlavel_auth::Authenticatable::auth_identifier(&user),
        ALICE
    );
}

#[tokio::test]
async fn a_wrong_password_does_not() {
    let url = directory!();

    let error = directory(&url).authenticate("alice", "not-alicepass").await.unwrap_err();

    assert!(
        matches!(error, AuthenticationError::InvalidPassword),
        "expected a rejected password, got {error}"
    );
    assert!(error.is_credential_failure());
}

/// The bug this package exists to refuse, and what a real directory does with
/// it — which turned out not to be what I assumed.
///
/// RFC 4511 §4.2 defines a simple bind with a zero-length password as an
/// *unauthenticated* bind. A login form that passes a blank password field
/// straight through therefore sends a message the specification says means
/// "log me in as nobody", and whether that comes back as `success` is up to
/// the server.
///
/// So this test has two halves. The client's refusal, which does not depend on
/// the directory at all. And then the same bind, hand-built past every guard
/// and sent to the real server, to find out what the guard is actually
/// standing in front of.
#[tokio::test]
async fn an_empty_password_does_not_authenticate() {
    let url = directory!();

    let error = directory(&url).authenticate("alice", "").await.unwrap_err();
    assert!(
        matches!(error, AuthenticationError::EmptyPassword),
        "expected the empty-password refusal, got {error}"
    );

    // `Operation::simple_bind` will not build this message and
    // `LdapConnection::bind` will not send it, so the bytes are assembled by
    // hand — which is exactly the code path an application would have had if
    // this package did not refuse.
    let mut encoder = Encoder::new();
    encoder.sequence(|message| {
        message.integer(1);
        message.constructed(tags::BIND_REQUEST, |bind| {
            bind.integer(3);
            bind.string(ALICE);
            // simple [0] "" — the empty password.
            bind.tagged_string(0x80, "");
        });
    });

    let response = send_one(&url, &encoder.into_bytes()).await;
    let ProtocolOp::BindResponse { result, .. } = response.op else {
        panic!("expected a bind response");
    };

    // Measured, not assumed. OpenLDAP 2.6.10 refuses it, with
    // `unwillingToPerform` and the diagnostic "unauthenticated bind (DN with
    // no password) disallowed" — RFC 4513 §5.1.2 says a server SHOULD, and
    // this one does.
    //
    // That is not a reason to drop the client-side guard, and it is worth
    // being precise about why. The behaviour is a configuration option
    // (`olcAllows: bind_anon_cred`), so the same server can be told to accept
    // it; Active Directory has historically answered it with `success`, which
    // is where the reported vulnerabilities came from; and the message itself
    // is still, by the specification, a request to be logged in as nobody. A
    // client that is safe only because the directory in front of it happens to
    // be strict is a client that is unsafe against the next directory it meets.
    assert_eq!(
        result.code,
        ResultCode::UnwillingToPerform,
        "this directory answered an empty-password bind with `{}`. If that is `success`, the \
         client-side refusal above is the only thing between a blank password field and a \
         successful login — which is the case this guard exists for.",
        result.code
    );
    assert!(!result.code.is_success());
}

/// A `*` is a username anybody can type. In the string form of a filter it is a
/// wildcard; through [`Filter`] it is one byte compared literally.
#[tokio::test]
async fn an_asterisk_in_the_username_does_not_match_everybody() {
    let url = directory!();
    let directory = directory(&url);

    // First, establish that there really are entries a wildcard would match —
    // otherwise "no match" proves nothing.
    let everybody = directory.search(Filter::present("uid"), &["uid"]).await.unwrap();
    assert!(
        everybody.len() >= 2,
        "the fixture should have at least two people in it, found {}",
        everybody.len()
    );

    // A bare asterisk matches nobody, because it is compared as data.
    let error = directory.authenticate("*", ALICE_PASSWORD).await.unwrap_err();
    assert!(
        matches!(error, AuthenticationError::NoSuchUser),
        "an asterisk must not match every account; got {error}"
    );

    // Nor does a trailing one, which is the version that looks most like a
    // legitimate username.
    let error = directory.authenticate("alice*", ALICE_PASSWORD).await.unwrap_err();
    assert!(matches!(error, AuthenticationError::NoSuchUser), "got {error}");

    // And neither does an attempt to rewrite the filter outright.
    let error = directory.authenticate("*)(uid=bob", ALICE_PASSWORD).await.unwrap_err();
    assert!(matches!(error, AuthenticationError::NoSuchUser), "got {error}");
}

#[tokio::test]
async fn a_missing_user_is_told_apart_from_a_wrong_password() {
    let url = directory!();
    let directory = directory(&url);

    let error = directory.authenticate("nobody-by-that-name", ALICE_PASSWORD).await.unwrap_err();
    assert!(matches!(error, AuthenticationError::NoSuchUser), "got {error}");

    // Distinguishable in the log; both must look the same on the login form,
    // which is what `is_credential_failure` is for.
    assert!(error.is_credential_failure());
}

/// A bind over an unencrypted connection, against a directory that is right
/// there and would happily accept it.
#[tokio::test]
async fn a_simple_bind_over_plain_tcp_is_refused_without_the_opt_in() {
    let url = directory!();

    // The same configuration as everywhere else in this file, minus the second
    // opt-in. The directory is reachable; the refusal is this client's.
    let directory = Directory::from_config(LdapConfig::parse(&url).unwrap().plaintext())
        .service_account(ADMIN, ADMIN_PASSWORD)
        .base_dn(BASE)
        .username_attribute("uid");

    let error = directory.authenticate("alice", ALICE_PASSWORD).await.unwrap_err();
    let text = error.to_string();

    assert!(text.contains("unencrypted"), "got {text}");
    assert!(text.contains("ldaps://"), "the error should name the fix: {text}");
    assert!(!error.is_credential_failure(), "this is not the user's fault");
}

/// `ldap://` defaults to StartTLS, and this container has no TLS at all.
///
/// The interesting property is not that it fails — it is that it fails
/// *instead* of quietly carrying on in the clear, which is the whole reason a
/// StartTLS client is worth writing carefully.
#[tokio::test]
async fn a_directory_without_tls_fails_start_tls_rather_than_downgrading() {
    let url = directory!();

    let config = LdapConfig::parse(&url).unwrap();
    assert_eq!(config.encryption, Encryption::StartTls, "`ldap://` must not mean plain text");

    let error = match LdapConnection::connect(&config).await {
        Ok(connection) => panic!(
            "StartTLS appeared to succeed against a container with no certificate (encrypted: {})",
            connection.is_encrypted()
        ),
        Err(error) => error.to_string(),
    };

    assert!(error.contains("StartTLS"), "got {error}");
}

#[tokio::test]
async fn a_bad_service_account_is_its_own_failure_and_not_the_user_s() {
    let url = directory!();

    let directory = Directory::from_config(
        LdapConfig::parse(&url).unwrap().plaintext().allow_plaintext_password(),
    )
    .service_account(ADMIN, "not-the-admin-password")
    .base_dn(BASE)
    .username_attribute("uid");

    let error = directory.authenticate("alice", ALICE_PASSWORD).await.unwrap_err();

    assert!(
        matches!(error, AuthenticationError::ServiceAccount(_)),
        "a broken service account must not look like a wrong user password; got {error}"
    );
    assert!(!error.is_credential_failure());
    assert!(error.is_directory_failure());
    assert!(error.to_string().contains("service account"), "got {error}");
}

/// The other deployment shape: no search, bind straight against a built DN.
#[tokio::test]
async fn a_dn_template_binds_without_a_service_account() {
    let url = directory!();

    let directory = Directory::from_config(
        LdapConfig::parse(&url).unwrap().plaintext().allow_plaintext_password(),
    )
    .user_dn_template("cn={username},ou=users,dc=example,dc=test");

    let user = directory.authenticate("alice", ALICE_PASSWORD).await.unwrap();
    assert_eq!(user.dn(), ALICE);
    // No search, so no attributes — the trade this mode makes.
    assert!(user.attributes.is_empty());

    let error = directory.authenticate("alice", "wrong").await.unwrap_err();
    assert!(matches!(error, AuthenticationError::InvalidPassword), "got {error}");

    // And the trade's other half: a name that does not exist is answered with
    // `invalidCredentials` too, so this mode cannot tell them apart.
    let error = directory.authenticate("nobody", "wrong").await.unwrap_err();
    assert!(matches!(error, AuthenticationError::InvalidPassword), "got {error}");
}

#[tokio::test]
async fn a_search_reads_entries_attributes_and_the_result_together() {
    let url = directory!();

    let mut connection = LdapConnection::connect(
        &LdapConfig::parse(&url).unwrap().plaintext().allow_plaintext_password(),
    )
    .await
    .unwrap();

    assert!(!connection.is_encrypted(), "the fixture container has no TLS");

    let result = connection.bind(ADMIN, ADMIN_PASSWORD).await.unwrap();
    assert!(result.is_success(), "{}", result.code);
    assert_eq!(connection.bound_as(), Some(ADMIN));

    let request = SearchRequest::new(BASE, Filter::equals("uid", "alice"))
        .scope(Scope::Subtree)
        .attributes(["cn", "uid", "objectClass"]);

    let outcome = connection.search(&request).await.unwrap();

    assert!(outcome.result.is_success(), "{}", outcome.result.code);
    assert_eq!(outcome.entries.len(), 1);
    assert_eq!(outcome.entries[0].dn, ALICE);
    assert_eq!(outcome.entries[0].value("uid"), Some("alice"));
    assert!(outcome.entries[0].values("objectClass").contains(&"inetOrgPerson"));

    // An `and` of a presence and an equality, to exercise the nested tags
    // against a server rather than against my own encoder.
    let request = SearchRequest::new(
        BASE,
        Filter::and([Filter::present("uid"), Filter::equals("objectClass", "inetOrgPerson")]),
    );
    let outcome = connection.search(&request).await.unwrap();
    assert!(outcome.entries.len() >= 2, "found {}", outcome.entries.len());

    // A filter that matches nothing is a successful search with no entries,
    // which is a different thing from an error.
    let request = SearchRequest::new(BASE, Filter::equals("uid", "*"));
    let outcome = connection.search(&request).await.unwrap();
    assert!(outcome.result.is_success());
    assert!(outcome.entries.is_empty(), "a literal asterisk matched {}", outcome.entries.len());

    connection.unbind().await.unwrap();
}

#[tokio::test]
async fn an_anonymous_bind_is_available_when_it_is_asked_for_by_name() {
    let url = directory!();

    let mut connection =
        LdapConnection::connect(&LdapConfig::parse(&url).unwrap().plaintext()).await.unwrap();

    // No `allow_plaintext_password` on this configuration, and it is not
    // needed: an anonymous bind carries no password to protect.
    let result = connection.bind_anonymous().await.unwrap();
    assert!(result.is_success(), "{}", result.code);
    assert_eq!(connection.bound_as(), None);

    connection.unbind().await.unwrap();
}

/// Write one pre-encoded message and read one back.
///
/// Deliberately not going through `LdapConnection`: the point of the caller is
/// to send something this package refuses to build.
async fn send_one(url: &str, message: &[u8]) -> LdapMessage {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let config = LdapConfig::parse(url).unwrap();
    let mut stream = tokio::net::TcpStream::connect(config.address()).await.unwrap();
    stream.write_all(message).await.unwrap();
    stream.flush().await.unwrap();

    let mut buffer = Vec::new();
    loop {
        match rustlavel_ldap::ber::element_size(&buffer, 1 << 20).unwrap() {
            Some(size) => return LdapMessage::parse(&buffer[..size]).unwrap(),
            None => {
                let mut chunk = [0u8; 1024];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0, "the directory closed without answering");
                buffer.extend_from_slice(&chunk[..read]);
            }
        }
    }
}
