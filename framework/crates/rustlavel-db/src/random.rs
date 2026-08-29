//! Random bytes, for SCRAM nonces.
//!
//! Reads the operating system's entropy source directly rather than adding a
//! dependency. A nonce only needs to be unpredictable and unique per exchange.

use std::io::Read;

/// Fill a buffer with random bytes from the OS.
pub fn bytes(length: usize) -> Vec<u8> {
    let mut buffer = vec![0u8; length];

    if let Ok(mut source) = std::fs::File::open("/dev/urandom") {
        if source.read_exact(&mut buffer).is_ok() {
            return buffer;
        }
    }

    // Fallback for a system without /dev/urandom: seed from the clock and the
    // address of a fresh allocation, then run a counter-based mixer. Weaker,
    // but a nonce that is merely unique still keeps the exchange correct.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = now ^ (Box::into_raw(Box::new(0u8)) as u64);

    for slot in buffer.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = (state >> 24) as u8;
    }
    buffer
}

/// A printable nonce, using the characters SCRAM allows (no comma).
pub fn nonce(length: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

    bytes(length)
        .into_iter()
        .map(|byte| ALPHABET[byte as usize % ALPHABET.len()] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonces_are_the_requested_length_and_printable() {
        let value = nonce(24);

        assert_eq!(value.len(), 24);
        assert!(value.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn two_nonces_differ() {
        assert_ne!(nonce(24), nonce(24));
    }
}
