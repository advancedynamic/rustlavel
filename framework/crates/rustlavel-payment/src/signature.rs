//! Checking that a webhook came from the gateway.
//!
//! Every gateway this crate has met signs its callbacks with HMAC-SHA256 over
//! the raw body, under a secret shared at setup. What differs is the header
//! the signature travels in, whether it is hex or base64, and whether anything
//! else is mixed into the signed string — so those are a driver's business and
//! the two primitives are here.
//!
//! **The comparison is constant-time.** A comparison that stops at the first
//! wrong byte tells an attacker, through timing, how many bytes they have
//! right; `hmac`'s `verify_slice` does not.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// HMAC-SHA256 of `message` under `secret`, as lowercase hex.
pub fn hmac_sha256_hex(secret: &[u8], message: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether `presented` is the HMAC-SHA256 of `message` under `secret`.
///
/// `presented` may be hex in either case. Nothing is compared with `==`.
pub fn verify_hmac_sha256_hex(secret: &[u8], message: &[u8], presented: &str) -> bool {
    let Some(bytes) = decode_hex(presented.trim()) else { return false };
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(message);
    mac.verify_slice(&bytes).is_ok()
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4231 test case 2: key "Jefe", message "what do ya want for nothing?".
    #[test]
    fn matches_the_rfc_4231_vector() {
        assert_eq!(
            hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn verifies_either_case_and_refuses_anything_else() {
        let secret = b"whsec_test";
        let body = br#"{"id":"ch_1","status":"paid"}"#;
        let good = hmac_sha256_hex(secret, body);

        assert!(verify_hmac_sha256_hex(secret, body, &good));
        assert!(verify_hmac_sha256_hex(secret, body, &good.to_uppercase()));
        assert!(verify_hmac_sha256_hex(secret, body, &format!("  {good}\n")));

        assert!(!verify_hmac_sha256_hex(secret, body, &hmac_sha256_hex(b"other", body)));
        assert!(!verify_hmac_sha256_hex(secret, b"{\"id\":\"ch_2\"}", &good), "a changed body verified");
        assert!(!verify_hmac_sha256_hex(secret, body, "not-hex"));
        assert!(!verify_hmac_sha256_hex(secret, body, ""));
        // Shorter than a real signature must fail, not compare a prefix.
        assert!(!verify_hmac_sha256_hex(secret, body, &good[..32]));
    }
}
