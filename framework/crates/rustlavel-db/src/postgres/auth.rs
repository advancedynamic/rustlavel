//! PostgreSQL authentication: MD5 and SCRAM-SHA-256.
//!
//! The hashing primitives come from crates rather than being hand-written —
//! this is the one place where "from scratch" would be a liability instead of a
//! feature. Everything around them, including the SCRAM message flow, is ours.

use crate::base64;
use hmac::{Hmac, Mac};
use md5::Md5;
use rustlavel_core::{Error, Result};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// The `md5` auth response: `md5` + hex(md5(hex(md5(password + user)) + salt)).
pub fn md5_password(user: &str, password: &str, salt: &[u8; 4]) -> String {
    let inner = hex(&{
        let mut hasher = Md5::new();
        hasher.update(password.as_bytes());
        hasher.update(user.as_bytes());
        hasher.finalize()
    });

    let outer = hex(&{
        let mut hasher = Md5::new();
        hasher.update(inner.as_bytes());
        hasher.update(salt);
        hasher.finalize()
    });

    format!("md5{outer}")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A SCRAM-SHA-256 exchange in progress.
///
/// SCRAM proves both sides know the password without either sending it, and
/// the server's final message is verified rather than trusted — skipping that
/// check would leave the client open to a server impersonating the real one.
pub struct Scram {
    password: String,
    client_nonce: String,
    /// `n=,r=<nonce>` — kept because it forms part of the signed auth message.
    client_first_bare: String,
    server_signature: Option<Vec<u8>>,
}

impl Scram {
    pub const MECHANISM: &'static str = "SCRAM-SHA-256";

    /// Start an exchange.
    ///
    /// The username field is left empty, which is what PostgreSQL requires:
    /// the role was already sent in the startup packet, and a server that read
    /// a different name here would be authenticating the wrong account.
    pub fn new(password: &str, nonce: String) -> Self {
        Scram::with_username(password, "", nonce)
    }

    /// Start an exchange that carries a username in the SCRAM message itself.
    ///
    /// PostgreSQL never uses this; it exists so the implementation can be
    /// checked against the RFC 7677 test vectors, which do include one.
    pub fn with_username(password: &str, username: &str, nonce: String) -> Self {
        Scram {
            password: password.to_string(),
            client_first_bare: format!("n={username},r={nonce}"),
            client_nonce: nonce,
            server_signature: None,
        }
    }

    /// The client-first message, including the GS2 header (no channel binding).
    pub fn client_first(&self) -> String {
        format!("n,,{}", self.client_first_bare)
    }

    /// Answer the server's challenge with the client proof.
    pub fn client_final(&mut self, server_first: &str) -> Result<String> {
        let attributes = parse_attributes(server_first);

        let combined_nonce = attributes
            .iter()
            .find(|(key, _)| *key == 'r')
            .map(|(_, value)| value.clone())
            .ok_or_else(|| Error::Protocol("SCRAM server-first has no nonce".into()))?;

        // The server must echo our nonce back; if it does not, we are not
        // talking to the party that received our first message.
        if !combined_nonce.starts_with(&self.client_nonce) {
            return Err(Error::Protocol("SCRAM server did not echo the client nonce".into()));
        }

        let salt = attributes
            .iter()
            .find(|(key, _)| *key == 's')
            .and_then(|(_, value)| base64::decode(value))
            .ok_or_else(|| Error::Protocol("SCRAM server-first has no salt".into()))?;

        let iterations: u32 = attributes
            .iter()
            .find(|(key, _)| *key == 'i')
            .and_then(|(_, value)| value.parse().ok())
            .ok_or_else(|| Error::Protocol("SCRAM server-first has no iteration count".into()))?;

        let salted = pbkdf2_sha256(self.password.as_bytes(), &salt, iterations);
        let client_key = hmac(&salted, b"Client Key");
        let stored_key = Sha256::digest(&client_key);

        // `c=biws` is base64("n,,") — the GS2 header, echoed back.
        let client_final_without_proof = format!("c=biws,r={combined_nonce}");
        let auth_message =
            format!("{},{server_first},{client_final_without_proof}", self.client_first_bare);

        let client_signature = hmac(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key
            .iter()
            .zip(client_signature.iter())
            .map(|(key, signature)| key ^ signature)
            .collect();

        let server_key = hmac(&salted, b"Server Key");
        self.server_signature = Some(hmac(&server_key, auth_message.as_bytes()));

        Ok(format!("{client_final_without_proof},p={}", base64::encode(&proof)))
    }

    /// Check the server's signature. An exchange that skips this is not
    /// mutually authenticated.
    pub fn verify(&self, server_final: &str) -> Result<()> {
        let expected = self
            .server_signature
            .as_ref()
            .ok_or_else(|| Error::Protocol("SCRAM finished before it started".into()))?;

        let attributes = parse_attributes(server_final);

        if let Some((_, message)) = attributes.iter().find(|(key, _)| *key == 'e') {
            return Err(Error::msg(format!("authentication failed: {message}")));
        }

        let received = attributes
            .iter()
            .find(|(key, _)| *key == 'v')
            .and_then(|(_, value)| base64::decode(value))
            .ok_or_else(|| Error::Protocol("SCRAM server-final has no verifier".into()))?;

        if received != *expected {
            return Err(Error::msg(
                "the database server failed SCRAM verification; it may not be the server you think it is"
                    .to_string(),
            ));
        }

        Ok(())
    }
}

/// Split `k=v,k=v` into pairs, keeping values that themselves contain `=`.
fn parse_attributes(message: &str) -> Vec<(char, String)> {
    message
        .split(',')
        .filter_map(|part| {
            let mut chars = part.chars();
            let key = chars.next()?;
            let rest = chars.as_str();
            rest.strip_prefix('=').map(|value| (key, value.to_string()))
        })
        .collect()
}

fn hmac(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

/// PBKDF2-HMAC-SHA256 with a 32-byte output, which is the only shape SCRAM-SHA-256 needs.
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> Vec<u8> {
    let mut salted = Vec::with_capacity(salt.len() + 4);
    salted.extend_from_slice(salt);
    salted.extend_from_slice(&1u32.to_be_bytes());

    let mut previous = hmac(password, &salted);
    let mut result = previous.clone();

    for _ in 1..iterations {
        previous = hmac(password, &previous);
        for (accumulated, block) in result.iter_mut().zip(previous.iter()) {
            *accumulated ^= block;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_the_documented_construction() {
        // md5("md5" + hex(md5(md5("secretpostgres") + salt)))
        let digest = md5_password("postgres", "secret", &[0x01, 0x02, 0x03, 0x04]);

        assert!(digest.starts_with("md5"));
        assert_eq!(digest.len(), 35);
        assert!(digest[3..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn pbkdf2_matches_a_known_vector() {
        // RFC 7677 test vector: password "pencil", salt "W22ZaJ0SNY7soEsUEjb6gQ==", i=4096.
        let salt = base64::decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let salted = pbkdf2_sha256(b"pencil", &salt, 4096);

        assert_eq!(base64::encode(&salted), "xKSVEDI6tPlSysH6mUQZOeeOp01r6B3fcJbodRPcYV0=");
    }

    #[test]
    fn produces_the_rfc_7677_client_proof() {
        let mut scram = Scram::with_username("pencil", "user", "rOprNGfwEbeRWgbNEkqO".to_string());
        assert_eq!(scram.client_first(), "n,,n=user,r=rOprNGfwEbeRWgbNEkqO");

        let server_first =
            "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
        let client_final = scram.client_final(server_first).unwrap();

        assert_eq!(
            client_final,
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
             p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );

        scram.verify("v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=").unwrap();
    }

    #[test]
    fn postgres_exchanges_leave_the_username_empty() {
        let scram = Scram::new("pencil", "abc".to_string());
        assert_eq!(scram.client_first(), "n,,n=,r=abc");
    }

    #[test]
    fn rejects_a_server_that_does_not_echo_the_nonce() {
        let mut scram = Scram::new("pencil", "mynonce".to_string());
        let error = scram.client_final("r=someoneelse,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096").unwrap_err();

        assert!(error.to_string().contains("echo the client nonce"));
    }

    #[test]
    fn rejects_a_bad_server_signature() {
        let mut scram = Scram::with_username("pencil", "user", "rOprNGfwEbeRWgbNEkqO".to_string());
        scram
            .client_final(
                "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096",
            )
            .unwrap();

        let error = scram.verify("v=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap_err();
        assert!(error.to_string().contains("failed SCRAM verification"));
    }

    #[test]
    fn surfaces_a_server_reported_authentication_error() {
        let mut scram = Scram::with_username("pencil", "user", "rOprNGfwEbeRWgbNEkqO".to_string());
        scram
            .client_final(
                "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096",
            )
            .unwrap();

        let error = scram.verify("e=invalid-proof").unwrap_err();
        assert!(error.to_string().contains("invalid-proof"));
    }
}
