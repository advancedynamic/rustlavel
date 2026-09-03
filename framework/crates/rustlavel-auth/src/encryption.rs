//! Authenticated encryption for anything the application hands to a client.
//!
//! AES-256-GCM, which is *authenticated* encryption: decryption verifies a tag
//! over the ciphertext before returning a single byte of plaintext. That matters
//! more than confidentiality here — an encrypted cookie that could be edited by
//! its holder would be worse than no cookie at all, because the application
//! would trust the forgery.
//!
//! A payload is a self-describing string: base64url of
//! `version(1) || nonce(12) || ciphertext || tag(16)`. Nothing but the key is
//! needed to read it back, the version byte leaves room to change cipher later
//! without invalidating what is already in people's browsers, and the URL-safe
//! alphabet means the result drops into a cookie or a query string untouched.

use crate::base64;
use crate::key::AppKey;
use crate::random;
use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use rustlavel_core::{Config, Error, Result};

/// The payload format this build writes. Present in every ciphertext.
const VERSION: u8 = 1;
/// AES-GCM's standard nonce size; the only one with a security proof behind it.
const NONCE_LENGTH: usize = 12;
/// The GCM authentication tag appended by the cipher.
const TAG_LENGTH: usize = 16;

/// Encrypts and decrypts with the application key.
///
/// Cheap to clone and safe to share, so it is normally registered once as
/// application state and reached with `req.state::<Encrypter>()`.
#[derive(Clone)]
pub struct Encrypter {
    key: AppKey,
}

impl Encrypter {
    /// Build from an application key. Encryption uses its own derived sub-key,
    /// never the raw `APP_KEY`, so a ciphertext and a URL signature can never
    /// be confused for one another.
    pub fn new(key: AppKey) -> Self {
        Encrypter { key }
    }

    /// Build from `app.key` in the configuration tree.
    pub fn from_config(config: &Config) -> Result<Self> {
        Ok(Encrypter::new(AppKey::from_config(config)?))
    }

    /// A fresh `APP_KEY` line for `.env`, used by `rustlavel key:generate`.
    pub fn generate_key() -> String {
        AppKey::generate()
    }

    /// Encrypt a string.
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        self.encrypt_bytes(plaintext.as_bytes())
    }

    /// Encrypt arbitrary bytes.
    ///
    /// A fresh nonce is drawn for every message. Reusing a nonce under one key
    /// is the single fatal mistake in GCM — it leaks the XOR of two plaintexts
    /// and, worse, the authentication subkey — so the nonce is never derived
    /// from the message, a counter, or the clock.
    pub fn encrypt_bytes(&self, plaintext: &[u8]) -> Result<String> {
        let nonce_bytes = random::bytes(NONCE_LENGTH);
        let nonce: [u8; NONCE_LENGTH] =
            nonce_bytes.as_slice().try_into().expect("random::bytes returned the length asked for");
        let cipher = self.cipher();
        // `Nonce::from` on a fixed-size array rather than `from_slice`: the
        // latter is deprecated in newer generic-array releases, and a caller
        // who runs `cargo update` should not inherit a warning from here.
        let ciphertext = cipher
            .encrypt(&Nonce::from(nonce), plaintext)
            .map_err(|_| Error::msg("could not encrypt the payload"))?;

        let mut payload = Vec::with_capacity(1 + NONCE_LENGTH + ciphertext.len());
        payload.push(VERSION);
        payload.extend_from_slice(&nonce_bytes);
        payload.extend_from_slice(&ciphertext);

        Ok(base64::encode_url(&payload))
    }

    /// Decrypt a payload back into a string.
    pub fn decrypt(&self, payload: &str) -> Result<String> {
        let plaintext = self.decrypt_bytes(payload)?;
        String::from_utf8(plaintext)
            .map_err(|_| Error::msg("the decrypted payload is not valid UTF-8"))
    }

    /// Decrypt a payload back into bytes.
    ///
    /// Every failure — wrong key, flipped bit, truncated string, unknown
    /// version — returns the same kind of error and reveals nothing about
    /// which one it was. A caller that told an attacker "the tag was wrong"
    /// rather than "the base64 was wrong" would be handing out an oracle.
    pub fn decrypt_bytes(&self, payload: &str) -> Result<Vec<u8>> {
        let failed = || Error::msg("could not decrypt the payload: it is invalid or was tampered with");

        let raw = base64::decode(payload).ok_or_else(failed)?;
        if raw.len() < 1 + NONCE_LENGTH + TAG_LENGTH || raw[0] != VERSION {
            return Err(failed());
        }

        let (nonce, ciphertext) = raw[1..].split_at(NONCE_LENGTH);
        let nonce: [u8; NONCE_LENGTH] = nonce.try_into().map_err(|_| failed())?;
        self.cipher()
            .decrypt(&Nonce::from(nonce), ciphertext)
            .map_err(|_| failed())
    }

    fn cipher(&self) -> Aes256Gcm {
        let derived = self.key.derive("encryption");
        Aes256Gcm::new(&Key::<Aes256Gcm>::from(derived))
    }
}

impl std::fmt::Debug for Encrypter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Encrypter { key: <redacted> }")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encrypter() -> Encrypter {
        Encrypter::new(AppKey::from_base64(&AppKey::generate()).unwrap())
    }

    #[test]
    fn encryption_round_trips() {
        let encrypter = encrypter();
        let payload = encrypter.encrypt("session=41; user=ada").unwrap();

        assert_eq!(encrypter.decrypt(&payload).unwrap(), "session=41; user=ada");
    }

    #[test]
    fn round_trips_empty_unicode_and_binary_payloads() {
        let encrypter = encrypter();

        assert_eq!(encrypter.decrypt(&encrypter.encrypt("").unwrap()).unwrap(), "");
        assert_eq!(encrypter.decrypt(&encrypter.encrypt("héllo 🔐").unwrap()).unwrap(), "héllo 🔐");

        let binary: Vec<u8> = (0..=255).collect();
        let payload = encrypter.encrypt_bytes(&binary).unwrap();
        assert_eq!(encrypter.decrypt_bytes(&payload).unwrap(), binary);
    }

    #[test]
    fn two_encryptions_of_the_same_text_differ() {
        let encrypter = encrypter();
        let first = encrypter.encrypt("same message").unwrap();
        let second = encrypter.encrypt("same message").unwrap();

        assert_ne!(first, second, "each message must get a fresh nonce");
        assert_eq!(encrypter.decrypt(&first).unwrap(), encrypter.decrypt(&second).unwrap());
    }

    #[test]
    fn a_tampered_payload_fails_to_decrypt() {
        let encrypter = encrypter();
        let payload = encrypter.encrypt("balance=100").unwrap();
        let raw = base64::decode(&payload).unwrap();

        // Flip one bit of the ciphertext: the plaintext an attacker is trying
        // to change from `balance=100`.
        let mut edited = raw.clone();
        edited[1 + NONCE_LENGTH] ^= 0x01;
        assert!(
            encrypter.decrypt(&base64::encode_url(&edited)).is_err(),
            "GCM must reject a modified ciphertext"
        );

        // Flip one bit of the authentication tag.
        let mut edited = raw.clone();
        let last = edited.len() - 1;
        edited[last] ^= 0x01;
        assert!(encrypter.decrypt(&base64::encode_url(&edited)).is_err());

        // Swap in a different nonce, leaving ciphertext and tag intact.
        let mut edited = raw;
        edited[1] ^= 0xff;
        assert!(encrypter.decrypt(&base64::encode_url(&edited)).is_err());
    }

    #[test]
    fn truncated_reordered_and_misversioned_payloads_are_rejected() {
        let encrypter = encrypter();
        let payload = encrypter.encrypt("secret").unwrap();

        assert!(encrypter.decrypt(&payload[..payload.len() - 4]).is_err());
        assert!(encrypter.decrypt("").is_err());
        assert!(encrypter.decrypt("!!!not base64!!!").is_err());

        // A payload whose version byte we do not recognise is not guessed at.
        let mut raw = base64::decode(&payload).unwrap();
        raw[0] = 99;
        assert!(encrypter.decrypt(&base64::encode_url(&raw)).is_err());
    }

    #[test]
    fn a_payload_from_another_key_does_not_decrypt() {
        let payload = encrypter().encrypt("mine").unwrap();

        assert!(encrypter().decrypt(&payload).is_err());
    }

    #[test]
    fn generated_keys_are_accepted_and_never_repeat() {
        let first = Encrypter::generate_key();
        let second = Encrypter::generate_key();

        assert_ne!(first, second);
        assert!(AppKey::from_base64(&first).is_ok());
    }

    #[test]
    fn builds_from_configuration_and_complains_when_the_key_is_missing() {
        let config = Config::new();
        config.set("app.key", Encrypter::generate_key());
        assert!(Encrypter::from_config(&config).is_ok());

        let error = Encrypter::from_config(&Config::new()).unwrap_err().to_string();
        assert!(error.contains("key:generate"), "the message should say how to fix it: {error}");
    }

    #[test]
    fn debug_output_never_contains_the_key() {
        assert_eq!(format!("{:?}", encrypter()), "Encrypter { key: <redacted> }");
    }
}
