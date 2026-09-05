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

    // No fallback. There used to be one — a xorshift seeded from the clock and
    // a heap address — behind an error log, and it was the most dangerous line
    // in the framework. This function is the only entropy source there is: it
    // makes session ids, CSRF tokens, API token secrets, argon2 salts, AES-GCM
    // nonces, WebAuthn challenges and `APP_KEY`. A session id drawn from 64
    // bits of clock is a session id somebody who knows roughly when you signed
    // in can enumerate; an `APP_KEY` drawn from it makes every signed cookie
    // forgeable.
    //
    // The condition is not hypothetical: file-descriptor exhaustion is
    // something an attacker can induce by opening connections, and the failure
    // was silent apart from a log line nobody reads in time.
    //
    // So this stops. A process that cannot obtain randomness must not go on
    // inventing secrets, and there is no safe value to return from here.
    panic!(
        "could not read /dev/urandom, and there is no safe substitute. Every secret this \
         framework makes — session ids, CSRF tokens, API tokens, password salts, encryption \
         nonces, APP_KEY — comes from here, so it stops rather than issue one that can be \
         guessed. Check the file-descriptor limit and that /dev is mounted."
    );
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

    /// There must be no way to get a secret out of here that did not come from
    /// the operating system.
    ///
    /// The test this replaces asserted that the *fallback* filled every byte —
    /// it guarded the shape of a mixer that should never have been reachable,
    /// and reading it was what made the fallback look considered rather than
    /// dangerous. A test can only check that nothing like it comes back.
    #[test]
    fn there_is_no_substitute_for_the_operating_systems_randomness() {
        let source = include_str!("random.rs");
        // Comments stripped: the note above `bytes` names the mixer it removed,
        // which is worth keeping and is not code.
        let code: String = source
            .split("#[cfg(test)]")
            .next()
            .expect("there is code above the tests")
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in ["xorshift", "SystemTime::now", "Box::into_raw", "fn fallback"] {
            assert!(
                !code.contains(banned),
                "`{banned}` is back in the entropy path. Every secret the framework issues comes \
                 from this function; a clock- or address-seeded value here is a guessable \
                 session id, and the failure is silent."
            );
        }
    }
}
