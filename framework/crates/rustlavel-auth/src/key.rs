//! The application key, and the sub-keys derived from it.
//!
//! One secret is configured (`APP_KEY`) and everything else is derived from it,
//! so an operator has exactly one thing to rotate. Encryption, session-cookie
//! signing and URL signing each get their *own* key, derived with a distinct
//! label: a signature produced for one purpose must never verify for another,
//! or a signed download link becomes a forged session cookie.

use crate::base64;
use hmac::{Hmac, Mac};
use rustlavel_core::{Config, Error, Result};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// How many bytes of key material the framework requires.
pub const KEY_LENGTH: usize = 32;

/// The 32-byte secret behind every signature and every ciphertext.
///
/// `Debug` is implemented by hand and prints nothing useful, so a key cannot
/// leak into a log line or the development error page by accident.
#[derive(Clone)]
pub struct AppKey([u8; KEY_LENGTH]);

impl AppKey {
    /// Wrap raw key material.
    pub fn from_bytes(bytes: [u8; KEY_LENGTH]) -> Self {
        AppKey(bytes)
    }

    /// Parse a configured key, with or without Laravel's `base64:` prefix.
    pub fn from_base64(value: &str) -> Result<Self> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(Error::msg(
                "APP_KEY is empty. Run `rustlavel key:generate` and put the result in .env — \
                 without it sessions, cookies and signed URLs cannot be secured.",
            ));
        }

        let encoded = trimmed.strip_prefix("base64:").unwrap_or(trimmed);
        let decoded = base64::decode(encoded).ok_or_else(|| {
            Error::msg("APP_KEY is not valid base64. Regenerate it with `rustlavel key:generate`.")
        })?;

        let bytes: [u8; KEY_LENGTH] = decoded.as_slice().try_into().map_err(|_| {
            Error::msg(format!(
                "APP_KEY decodes to {} bytes, but {KEY_LENGTH} are required. Regenerate it with \
                 `rustlavel key:generate`.",
                decoded.len()
            ))
        })?;

        Ok(AppKey(bytes))
    }

    /// Read `app.key` out of the configuration tree.
    pub fn from_config(config: &Config) -> Result<Self> {
        AppKey::from_base64(&config.string("app.key", ""))
    }

    /// A fresh key, formatted the way it belongs in `.env`.
    ///
    /// The `base64:` prefix is kept so the value is self-describing: an
    /// operator can tell at a glance that the key is encoded, not a passphrase.
    pub fn generate() -> String {
        format!("base64:{}", base64::encode(&crate::random::bytes(KEY_LENGTH)))
    }

    pub fn as_bytes(&self) -> &[u8; KEY_LENGTH] {
        &self.0
    }

    /// Derive a purpose-specific sub-key.
    ///
    /// HMAC-SHA256 with the application key as the MAC key and the label as the
    /// message: a standard one-step KDF, and the reason a `signed-url`
    /// signature can never be replayed as a `session-cookie` signature.
    pub fn derive(&self, purpose: &str) -> [u8; KEY_LENGTH] {
        let mut mac = HmacSha256::new_from_slice(&self.0).expect("HMAC accepts any key length");
        mac.update(b"rustlavel:");
        mac.update(purpose.as_bytes());
        mac.finalize().into_bytes().into()
    }
}

impl std::fmt::Debug for AppKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AppKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_generated_key_with_or_without_the_prefix() {
        let generated = AppKey::generate();
        assert!(generated.starts_with("base64:"));

        let with_prefix = AppKey::from_base64(&generated).unwrap();
        let without_prefix =
            AppKey::from_base64(generated.strip_prefix("base64:").unwrap()).unwrap();

        assert_eq!(with_prefix.as_bytes(), without_prefix.as_bytes());
    }

    #[test]
    fn rejects_keys_that_are_missing_malformed_or_the_wrong_length() {
        assert!(AppKey::from_base64("").is_err());
        assert!(AppKey::from_base64("base64:not valid!").is_err());

        let short = base64::encode(&[0u8; 16]);
        let error = AppKey::from_base64(&short).unwrap_err().to_string();
        assert!(error.contains("16 bytes"), "message should name the actual length: {error}");
    }

    #[test]
    fn reads_the_key_from_configuration() {
        let config = Config::new();
        config.set("app.key", AppKey::generate());

        assert!(AppKey::from_config(&config).is_ok());
        assert!(AppKey::from_config(&Config::new()).is_err());
    }

    #[test]
    fn different_purposes_derive_different_sub_keys() {
        let key = AppKey::from_bytes([7u8; KEY_LENGTH]);

        assert_ne!(key.derive("encryption"), key.derive("signed-url"));
        assert_ne!(key.derive("signed-url"), key.derive("session-cookie"));
        // Derivation is deterministic, or nothing signed yesterday would verify.
        assert_eq!(key.derive("signed-url"), key.derive("signed-url"));
        // And a sub-key is never the application key itself.
        assert_ne!(&key.derive("encryption"), key.as_bytes());
    }

    #[test]
    fn debug_output_never_contains_the_key() {
        let key = AppKey::from_bytes([0xab; KEY_LENGTH]);
        assert_eq!(format!("{key:?}"), "AppKey(<redacted>)");
    }
}
