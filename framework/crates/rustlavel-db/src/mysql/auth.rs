//! MySQL authentication: `mysql_native_password` and `caching_sha2_password`.
//!
//! Both plugins work the same way in outline. The server sends a 20-byte
//! nonce — the *scramble* — and the client answers with a digest that mixes the
//! password with it. The password itself never crosses the wire, and the answer
//! is useless to anyone who replays it against a different scramble.
//!
//! The hash functions come from crates rather than being hand-written. This is
//! the one place where "from scratch" would be a liability instead of a
//! feature; everything around them, including both message flows, is ours.

use rustlavel_core::{Error, Result};
use sha1::Sha1;
use sha2::{Digest, Sha256};

/// The plugin every MySQL before 8.0 defaulted to, and many still use.
pub const MYSQL_NATIVE_PASSWORD: &str = "mysql_native_password";

/// MySQL 8's default.
pub const CACHING_SHA2_PASSWORD: &str = "caching_sha2_password";

/// The plugin that sends the password in the clear. Refused outright — see
/// [`insecure_plugin_error`].
pub const MYSQL_CLEAR_PASSWORD: &str = "mysql_clear_password";

/// The plugin MySQL names when it does not want to say the user is unknown.
pub const SHA256_PASSWORD: &str = "sha256_password";

/// `mysql_native_password`: `SHA1(password) XOR SHA1(scramble ++ SHA1(SHA1(password)))`.
///
/// The server stores only `SHA1(SHA1(password))`, so it can verify the answer
/// without ever holding anything it could replay elsewhere — the outer XOR is
/// what lets it recover `SHA1(password)` and check it hashes to what it stored.
///
/// An empty password sends an empty response rather than a digest of nothing,
/// which is how the server distinguishes "no password set" from a wrong one.
pub fn native_password(password: &str, scramble: &[u8]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }

    let stage1 = Sha1::digest(password.as_bytes());
    let stage2 = Sha1::digest(stage1);

    let mut hasher = Sha1::new();
    hasher.update(scramble);
    hasher.update(stage2);
    let salted = hasher.finalize();

    xor(&stage1, &salted)
}

/// `caching_sha2_password`, fast path:
/// `SHA256(password) XOR SHA256(SHA256(SHA256(password)) ++ scramble)`.
///
/// The shape is the same as the SHA-1 plugin's with a stronger hash and the
/// scramble on the other side of the concatenation. "Fast" is the path the
/// server can take once it has the account in its in-memory cache; a cold cache
/// forces the full path, which needs a channel nobody can read.
pub fn caching_sha2_password(password: &str, scramble: &[u8]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }

    let stage1 = Sha256::digest(password.as_bytes());
    let stage2 = Sha256::digest(stage1);

    let mut hasher = Sha256::new();
    hasher.update(stage2);
    hasher.update(scramble);
    let salted = hasher.finalize();

    xor(&stage1, &salted)
}

/// What the server decided after seeing a `caching_sha2_password` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastAuth {
    /// The account was in the cache and the digest matched: authentication is
    /// finished, and an OK packet follows.
    Succeeded,
    /// The cache did not hold this account, so the server wants the password
    /// itself over a channel nobody can read.
    FullAuthRequired,
}

/// The two-byte verdict inside the `AuthMoreData` packet.
pub fn fast_auth_status(data: &[u8]) -> Result<FastAuth> {
    match data.first() {
        Some(0x03) => Ok(FastAuth::Succeeded),
        Some(0x04) => Ok(FastAuth::FullAuthRequired),
        Some(other) => Err(Error::Protocol(format!(
            "caching_sha2_password sent status {other:#04x}, which this driver does not understand"
        ))),
        None => Err(Error::Protocol("caching_sha2_password sent an empty status".into())),
    }
}

/// The password itself, NUL-terminated, for the full authentication path.
///
/// Only ever sent over a channel that cannot be read — see
/// [`full_auth_error`], which is what happens when there is no such channel.
pub fn cleartext_password(password: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(password.len() + 1);
    out.extend_from_slice(password.as_bytes());
    out.push(0);
    out
}

/// The full `caching_sha2_password` path needs a secure channel, and there is
/// none.
///
/// Reached only when the connection is *not* encrypted — with TLS the driver
/// takes the full path happily, which is what MySQL's own client does. Rather
/// than fail with "authentication failed" — which sends the developer hunting
/// for a typo in a password that is perfectly correct — say exactly what
/// happened and lead with the fix that is now one query parameter away.
pub fn full_auth_error(user: &str, host: &str) -> Error {
    Error::msg(format!(
        "the server wants full caching_sha2_password authentication for `{user}`, which sends the \
         password itself and so needs a channel nobody can read. This connection to {host} is \
         plain TCP, so the driver will not send the password in the clear.\n  \
         Any of these fixes it:\n  \
         1. Encrypt the connection: add `?sslmode=require` to DATABASE_URL — this is the one you \
         want, and it is why sslmode exists.\n  \
         2. Connect once with the `mysql` client (over a socket or with --get-server-public-key); \
         the server then caches the account and this driver's fast path works.\n  \
         3. ALTER USER '{user}'@'%' IDENTIFIED WITH mysql_native_password BY '…' — available up to \
         MySQL 8.0, and removed in 8.4."
    ))
}

/// A plugin this driver refuses to speak.
///
/// `mysql_clear_password` puts the password on the wire verbatim, and
/// `sha256_password` needs the RSA exchange this driver does not implement. A
/// hostile server can *ask* for either; agreeing would hand it the password.
pub fn insecure_plugin_error(plugin: &str) -> Error {
    if plugin == MYSQL_CLEAR_PASSWORD {
        return Error::msg(format!(
            "the server asked for `{plugin}`, which sends the password in the clear. This driver \
             refuses: a server that asks for it can read the password, and a server that has been \
             replaced by someone else can too."
        ));
    }

    // `sha256_password` deserves its own sentence, because the usual reason for
    // seeing it is not that anybody configured it. MySQL answers a login for a
    // user that does not exist with a plugin chosen from the *name*, so that a
    // stranger cannot learn which accounts are real by watching the handshake.
    // Taken at face value the message below sends a developer off to implement
    // an authentication plugin when the account has simply been deleted — which
    // is exactly what happens when a dynamic credential's lease is revoked.
    if plugin == SHA256_PASSWORD {
        return Error::msg(format!(
            "the server asked for the `{plugin}` authentication plugin, which this driver does \
             not implement — but the more likely explanation is that this account does not \
             exist. MySQL answers a login for an unknown user with a plugin picked from the \
             user name, so that watching the handshake cannot reveal which accounts are real. \
             Check the user name first; if the account really is configured for {plugin}, \
             change it to {CACHING_SHA2_PASSWORD}."
        ));
    }

    Error::msg(format!(
        "the server asked for the `{plugin}` authentication plugin, which this driver does not \
         implement. It speaks {MYSQL_NATIVE_PASSWORD} and {CACHING_SHA2_PASSWORD}."
    ))
}

/// Whether this driver can answer a plugin's challenge at all.
pub fn is_supported(plugin: &str) -> bool {
    matches!(plugin, MYSQL_NATIVE_PASSWORD | CACHING_SHA2_PASSWORD)
}

/// Compute a plugin's response to a scramble.
pub fn respond(plugin: &str, password: &str, scramble: &[u8]) -> Result<Vec<u8>> {
    match plugin {
        MYSQL_NATIVE_PASSWORD => Ok(native_password(password, scramble)),
        CACHING_SHA2_PASSWORD => Ok(caching_sha2_password(password, scramble)),
        other => Err(insecure_plugin_error(other)),
    }
}

fn xor(left: &[u8], right: &[u8]) -> Vec<u8> {
    left.iter().zip(right.iter()).map(|(a, b)| a ^ b).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Twenty bytes, the length every MySQL scramble has.
    const SCRAMBLE: &[u8] = b"01234567890123456789";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn native_password_matches_a_constructed_vector() {
        // SHA1("secret") XOR SHA1(scramble ++ SHA1(SHA1("secret"))), computed
        // independently from the plugin's documented definition.
        assert_eq!(
            hex(&native_password("secret", SCRAMBLE)),
            "7abe1a8776b59e931059451f81e596a60dbbf7a8"
        );
    }

    #[test]
    fn native_password_is_the_documented_xor_of_two_sha1s() {
        let response = native_password("secret", SCRAMBLE);
        assert_eq!(response.len(), 20, "SHA-1 is 20 bytes wide");

        // Undo the XOR with the half the server can recompute, and what is left
        // must be SHA1(password) — which is exactly the check the server makes.
        let stage1 = Sha1::digest(b"secret");
        let stage2 = Sha1::digest(stage1);
        let mut hasher = Sha1::new();
        hasher.update(SCRAMBLE);
        hasher.update(stage2);
        let recovered = xor(&response, &hasher.finalize());

        assert_eq!(recovered, stage1.to_vec());
    }

    #[test]
    fn the_server_stores_the_double_sha1_this_response_is_built_from() {
        // What MySQL keeps in `mysql.user.authentication_string` for a
        // mysql_native_password account with the password "secret".
        let stored = format!("*{}", hex(&Sha1::digest(Sha1::digest(b"secret"))).to_uppercase());

        assert_eq!(stored, "*14E65567ABDB5135D0CFD9A70B3032C179A49EE7");
    }

    #[test]
    fn caching_sha2_matches_a_constructed_vector() {
        assert_eq!(
            hex(&caching_sha2_password("secret", SCRAMBLE)),
            "1a2da2573c2faa367e2afddb54cdfd11a95ed22eef0167151196a6fc8e3d3813"
        );
    }

    #[test]
    fn caching_sha2_is_the_documented_xor_of_two_sha256s() {
        let response = caching_sha2_password("secret", SCRAMBLE);
        assert_eq!(response.len(), 32, "SHA-256 is 32 bytes wide");

        // The scramble sits after the digest here, not before it as in the
        // SHA-1 plugin — a difference that is invisible until it is wrong.
        let stage1 = Sha256::digest(b"secret");
        let stage2 = Sha256::digest(stage1);
        let mut hasher = Sha256::new();
        hasher.update(stage2);
        hasher.update(SCRAMBLE);
        let recovered = xor(&response, &hasher.finalize());

        assert_eq!(recovered, stage1.to_vec());
    }

    #[test]
    fn a_different_scramble_gives_a_different_response() {
        // What makes the answer worthless to replay.
        let first = caching_sha2_password("secret", SCRAMBLE);
        let second = caching_sha2_password("secret", b"98765432109876543210");

        assert_ne!(first, second);
    }

    #[test]
    fn an_empty_password_sends_an_empty_response() {
        // Not a digest of the empty string: the server tells the two apart.
        assert!(native_password("", SCRAMBLE).is_empty());
        assert!(caching_sha2_password("", SCRAMBLE).is_empty());
    }

    #[test]
    fn reads_the_caching_sha2_verdict() {
        assert_eq!(fast_auth_status(&[0x03]).unwrap(), FastAuth::Succeeded);
        assert_eq!(fast_auth_status(&[0x04]).unwrap(), FastAuth::FullAuthRequired);
        assert!(fast_auth_status(&[0x09]).is_err());
        assert!(fast_auth_status(&[]).is_err());
    }

    #[test]
    fn a_cleartext_password_is_nul_terminated() {
        assert_eq!(cleartext_password("secret"), b"secret\0");
        assert_eq!(cleartext_password(""), b"\0");
    }

    #[test]
    fn full_authentication_without_a_secure_channel_says_what_to_do_instead() {
        let error = full_auth_error("ada", "127.0.0.1:3306").to_string();

        assert!(error.contains("caching_sha2_password"), "{error}");
        assert!(error.contains("ada"), "{error}");
        assert!(error.contains("127.0.0.1:3306"), "{error}");
        // The three ways out are all named, and none of them is "give up".
        assert!(error.contains("mysql_native_password"), "{error}");
        assert!(error.contains("--get-server-public-key"), "{error}");
        assert!(error.contains("DATABASE_URL"), "{error}");
    }

    #[test]
    fn refuses_a_plugin_that_would_hand_over_the_password() {
        let error = insecure_plugin_error(MYSQL_CLEAR_PASSWORD).to_string();
        assert!(error.contains("in the clear"), "{error}");

        let error = insecure_plugin_error("some_other_plugin").to_string();
        assert!(error.contains("does not implement"), "{error}");
        assert!(error.contains(MYSQL_NATIVE_PASSWORD), "{error}");
    }

    #[test]
    fn sha256_password_leads_with_the_reason_it_is_usually_seen() {
        // Measured against MySQL 8.4: connecting as a user whose account has
        // been deleted — a dynamic credential whose lease was revoked — is
        // answered with `sha256_password`, because MySQL picks a plugin from
        // the user name rather than admit the account is unknown. The literal
        // reading of the old message sent you off to implement a plugin.
        let error = insecure_plugin_error(SHA256_PASSWORD).to_string();

        assert!(error.contains("does not exist"), "{error}");
        assert!(error.contains("unknown user"), "{error}");
        assert!(error.contains("Check the user name first"), "{error}");
    }

    #[test]
    fn only_the_two_implemented_plugins_are_answered() {
        assert!(is_supported(MYSQL_NATIVE_PASSWORD));
        assert!(is_supported(CACHING_SHA2_PASSWORD));
        assert!(!is_supported(MYSQL_CLEAR_PASSWORD));
        assert!(!is_supported("sha256_password"));

        assert_eq!(
            respond(MYSQL_NATIVE_PASSWORD, "secret", SCRAMBLE).unwrap(),
            native_password("secret", SCRAMBLE)
        );
        assert!(respond("sha256_password", "secret", SCRAMBLE).is_err());
    }
}
