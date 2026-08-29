//! Random bytes, read straight from the operating system.
//!
//! Session ids, CSRF tokens, encryption nonces and application keys all come
//! from here. Unlike a protocol nonce, most of these values are secrets: an
//! attacker who can predict a session id is logged in as somebody else. So the
//! only source we trust is the OS CSPRNG, and a system that cannot provide one
//! gets a loud error rather than a quiet downgrade.

use std::io::Read;

/// Fill a buffer with `length` bytes from the operating system's CSPRNG.
///
/// Reading `/dev/urandom` directly keeps the dependency list honest — it is
/// the same syscall a `rand` crate would make. If it is unreachable (a
/// chroot without `/dev`, a platform without it) the caller still gets bytes,
/// but they are only unique, not unpredictable, and the process says so.
pub fn bytes(length: usize) -> Vec<u8> {
    let mut buffer = vec![0u8; length];

    if let Ok(mut source) = std::fs::File::open("/dev/urandom")
        && source.read_exact(&mut buffer).is_ok()
    {
        return buffer;
    }

    rustlavel_core::error!(
        "could not read /dev/urandom: session ids and tokens are being generated from a \
         clock-seeded fallback. Do not run this configuration in production."
    );
    fallback(&mut buffer);
    buffer
}

/// A clock- and address-seeded mixer, used only when the OS source is gone.
fn fallback(buffer: &mut [u8]) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = now ^ (Box::into_raw(Box::new(0u8)) as u64) | 1;

    for slot in buffer.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = (state >> 24) as u8;
    }
}

/// `length` random bytes rendered as lowercase hex.
///
/// Hex rather than an alphabet lookup because folding random bytes into a
/// 62-character alphabet with `%` biases the result; hex is a bijection, so a
/// 32-byte id really carries 256 bits.
pub fn hex(length: usize) -> String {
    let mut out = String::with_capacity(length * 2);
    for byte in bytes(length) {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_the_requested_number_of_bytes() {
        assert_eq!(bytes(32).len(), 32);
        assert_eq!(bytes(0).len(), 0);
    }

    #[test]
    fn hex_output_is_twice_as_long_and_lowercase_hexadecimal() {
        let value = hex(32);

        assert_eq!(value.len(), 64);
        assert!(value.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn two_draws_of_the_same_length_differ() {
        assert_ne!(hex(32), hex(32));
    }

    #[test]
    fn the_fallback_mixer_still_fills_every_byte() {
        // Even degraded, the buffer must not be left as zeroes: a session id of
        // all zeroes would be shared by every visitor.
        let mut buffer = vec![0u8; 64];
        fallback(&mut buffer);
        assert!(buffer.iter().any(|byte| *byte != 0));
    }
}
