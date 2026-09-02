//! Two-factor authentication: HOTP (RFC 4226) and TOTP (RFC 6238).
//!
//! The second factor every authenticator app speaks. The server and the phone
//! share one secret; the phone shows a six-digit number derived from that secret
//! and the current half-minute, and the server recomputes the same number. There
//! is no network call and no vendor — the whole protocol is an HMAC and a
//! division.
//!
//! ```ignore
//! // Enrolment: mint a secret, show the QR code, and keep the secret only once
//! // the person has proved they can read a code from it.
//! let totp = Totp::generate();
//! let uri = totp.provisioning_uri("jane@acme.test", "ACME");
//! // ...render `uri` as a QR code, ask for a code, then:
//! if totp.verify(&submitted, unix_now()) {
//!     user.two_factor_secret = Some(totp.secret_base32());
//!     user.recovery_codes = RecoveryCodes::generate(10).hashed();
//! }
//!
//! // Login: rebuild it from the stored secret.
//! let totp = Totp::from_base32(&user.two_factor_secret)?;
//! if totp.verify(&submitted, unix_now()) { /* second factor satisfied */ }
//! ```
//!
//! # What this protects against, and what it does not
//!
//! A TOTP code proves the person holds the shared secret *now*. It does not
//! prove they are on the site they think they are on: a phishing page that asks
//! for a code and replays it within thirty seconds gets in, and that is the
//! attack passkeys exist to stop. TOTP raises the cost of a stolen password; it
//! is not origin-bound and never has been.
//!
//! It also does nothing about replay on its own — see [`step_of`], which exists
//! precisely so a caller can close that hole.

use crate::hashing::{Cost, hash_password, hash_password_with, verify_password};
use crate::random;
use hmac::{Hmac, Mac};
use rustlavel_core::{Error, Result};
use rustlavel_http::url;
use sha1::Sha1;
use sha2::{Sha256, Sha512};
use subtle::{Choice, ConstantTimeEq};

/// The RFC 4648 base32 alphabet, uppercase, in the order the standard fixes it.
pub const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// How many random bytes a generated secret gets.
///
/// RFC 4226 §4 R6 requires at least 128 bits and recommends 160 — one full
/// HMAC-SHA1 output — and 160 bits is what every provisioning implementation in
/// the wild emits. A longer secret is not wrong, but HMAC folds anything past
/// the block size back down through the hash, so it buys nothing an app would
/// notice and it makes the QR code denser for no reason.
pub const SECRET_BYTES: usize = 20;

/// The default step, in seconds. RFC 6238 §5.2 recommends thirty.
pub const DEFAULT_PERIOD: u64 = 30;

/// The default number of digits shown. Six, as every app expects.
pub const DEFAULT_DIGITS: u32 = 6;

/// How many steps either side of the current one [`Totp::verify`] accepts.
///
/// One, which is the RFC 6238 §5.2 compromise: a phone whose clock is a few
/// seconds off — or a person who starts typing at second 29 — must still be
/// able to log in, and each extra step widens the window an attacker has to
/// replay a code they observed.
pub const DEFAULT_SKEW: u64 = 1;

/// Encode bytes as unpadded uppercase base32 (RFC 4648 §6).
///
/// Unpadded because that is what `otpauth://` URIs and every authenticator app
/// use: a trailing `=` in a QR code is noise that some scanners mangle, and the
/// length of the secret already says where it ends.
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;

    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32_ALPHABET[((buffer >> bits) & 0x1f) as usize] as char);
        }
    }

    // The last group is padded on the right with zero bits, never dropped: a
    // secret whose length is not a multiple of five bytes still has to survive
    // the round trip.
    if bits > 0 {
        out.push(BASE32_ALPHABET[((buffer << (5 - bits)) & 0x1f) as usize] as char);
    }

    out
}

/// Decode base32, the way a person actually types it.
///
/// Deliberately lenient about presentation and strict about content. Lowercase
/// is accepted because keyboards default to it; spaces are ignored because
/// every app that offers a manual-entry secret prints it in groups of four; `=`
/// is ignored because some implementations pad and the padding carries no
/// information. Anything else — a `0` typed for an `O`, a stray comma from a
/// copy-and-paste — is rejected rather than silently reinterpreted, because
/// guessing at a secret produces a working-looking `Totp` that never verifies.
///
/// Returns `None` for an unusable string; an empty input decodes to no bytes,
/// which is what RFC 4648 §10 says and is [`Totp::from_base32`]'s problem to
/// refuse, not this function's.
pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;

    for character in text.chars() {
        if character == '=' || character.is_whitespace() {
            continue;
        }

        let value = match character {
            'A'..='Z' => character as u32 - 'A' as u32,
            'a'..='z' => character as u32 - 'a' as u32,
            '2'..='7' => character as u32 - '2' as u32 + 26,
            _ => return None,
        };

        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }

    // Fewer than eight bits are left over by construction, and they are the
    // encoder's right-hand padding. They are dropped rather than checked for
    // being zero: a secret copied out of an app that pads sloppily should still
    // enrol, and the bits cannot change a byte that was already emitted.
    Some(out)
}

/// Which HMAC an authenticator is asked to use.
///
/// SHA-1 is the default and, in practice, the only universally supported
/// choice: RFC 6238 §1.2 allows all three, but an app handed
/// `algorithm=SHA256` may quietly ignore it and compute SHA-1 anyway, which
/// looks to the person like the server rejecting a correct code. Change this
/// only when you control both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Algorithm {
    #[default]
    Sha1,
    Sha256,
    Sha512,
}

impl Algorithm {
    /// The name as an `otpauth://` URI spells it.
    pub fn name(self) -> &'static str {
        match self {
            Algorithm::Sha1 => "SHA1",
            Algorithm::Sha256 => "SHA256",
            Algorithm::Sha512 => "SHA512",
        }
    }

    /// Read a name back, accepting the spellings seen in the wild.
    pub fn parse(name: &str) -> Option<Algorithm> {
        match name.to_ascii_uppercase().replace('-', "").as_str() {
            "SHA1" => Some(Algorithm::Sha1),
            "SHA256" => Some(Algorithm::Sha256),
            "SHA512" => Some(Algorithm::Sha512),
            _ => None,
        }
    }
}

impl std::fmt::Display for Algorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// HMAC the eight-byte big-endian counter under `secret`.
///
/// Written out three times rather than made generic. The bound that would let
/// one function accept any of the three digests is a dozen lines of
/// `digest::core_api` associated types, and the compiler error a caller gets
/// when it does not hold is unreadable — which is a worse trade than three
/// obvious lines.
fn mac(algorithm: Algorithm, secret: &[u8], counter: u64) -> Vec<u8> {
    let message = counter.to_be_bytes();

    match algorithm {
        Algorithm::Sha1 => {
            let mut mac =
                Hmac::<Sha1>::new_from_slice(secret).expect("HMAC accepts a key of any length");
            mac.update(&message);
            mac.finalize().into_bytes().to_vec()
        }
        Algorithm::Sha256 => {
            let mut mac =
                Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts a key of any length");
            mac.update(&message);
            mac.finalize().into_bytes().to_vec()
        }
        Algorithm::Sha512 => {
            let mut mac =
                Hmac::<Sha512>::new_from_slice(secret).expect("HMAC accepts a key of any length");
            mac.update(&message);
            mac.finalize().into_bytes().to_vec()
        }
    }
}

/// Clamp a digit count to what RFC 4226 §5.3 allows.
///
/// The RFC's floor is six — fewer digits means a one-in-ten-thousand guess,
/// which is inside what an unthrottled login endpoint hands out for free. The
/// ceiling of ten is arithmetic rather than policy: dynamic truncation yields a
/// 31-bit number, so an eleventh digit would always be a zero.
fn clamped_digits(digits: u32) -> u32 {
    digits.clamp(6, 10)
}

/// One HOTP value: RFC 4226's counter-based code.
///
/// `counter` is a shared, monotonically increasing number rather than a clock —
/// this is the algorithm TOTP is built out of, and it is what a hardware token
/// with a button uses. `digits` is clamped to the RFC's six-to-ten range; see
/// [`clamped_digits`].
///
/// Always SHA-1, because that is the only algorithm RFC 4226 defines. Use
/// [`hotp_with`] to pick another.
pub fn hotp(secret: &[u8], counter: u64, digits: u32) -> String {
    hotp_with(Algorithm::Sha1, secret, counter, digits)
}

/// [`hotp`], with the HMAC named explicitly. This is what TOTP calls.
pub fn hotp_with(algorithm: Algorithm, secret: &[u8], counter: u64, digits: u32) -> String {
    let digits = clamped_digits(digits);
    let hash = mac(algorithm, secret, counter);

    // Dynamic truncation, RFC 4226 §5.3. The low nibble of the last byte picks
    // where in the digest to read from, so which four bytes carry the code is
    // itself a function of the secret; the top bit is masked off because some
    // platforms have no unsigned 32-bit type and would read the result as a
    // negative number.
    let offset = (hash[hash.len() - 1] & 0x0f) as usize;
    let binary = (u32::from(hash[offset]) & 0x7f) << 24
        | u32::from(hash[offset + 1]) << 16
        | u32::from(hash[offset + 2]) << 8
        | u32::from(hash[offset + 3]);

    let value = u64::from(binary) % 10u64.pow(digits);
    format!("{value:0width$}", width = digits as usize)
}

/// Which time step `unix_seconds` falls in — RFC 6238's `T`.
///
/// # This is how you stop a code being replayed
///
/// TOTP by itself does not. A code stays valid for the whole step, and for the
/// neighbouring steps too once a skew window is allowed, so anybody who reads
/// one over a shoulder, out of a phishing form, or off a shared screen can use
/// it again for the best part of a minute — and [`Totp::verify`] will accept it,
/// because it is a correct code.
///
/// The fix is not in this module, because it needs storage this module does not
/// have. Keep the last step you accepted for each user, alongside their secret,
/// and refuse anything that is not strictly newer:
///
/// ```ignore
/// let step = step_of(unix_now(), totp.period());
/// if user.last_totp_step.is_some_and(|last| step <= last) {
///     return Err("that code has already been used");
/// }
/// if totp.verify(&submitted, unix_now()) {
///     user.last_totp_step = Some(step);
/// }
/// ```
///
/// A `period` of zero is read as one second rather than dividing by zero.
pub fn step_of(unix_seconds: u64, period: u64) -> u64 {
    unix_seconds / period.max(1)
}

/// A TOTP configuration: one secret and the parameters an app must agree on.
///
/// Built with [`generate`](Self::generate) at enrolment and with
/// [`from_base32`](Self::from_base32) on every later login. The defaults —
/// twenty random bytes, six digits, thirty seconds, HMAC-SHA1 — are what an
/// authenticator app assumes when a provisioning URI leaves them out, so
/// changing one means the app and the server must both be told.
#[derive(Clone)]
pub struct Totp {
    secret: Vec<u8>,
    digits: u32,
    period: u64,
    algorithm: Algorithm,
    skew: u64,
}

impl Totp {
    /// A new random secret, from the operating system's CSPRNG.
    ///
    /// This is a credential: whoever holds it can produce codes forever. It is
    /// generated once, shown once as a QR code, and stored encrypted.
    pub fn generate() -> Self {
        Totp::new(random::bytes(SECRET_BYTES))
    }

    /// Wrap an existing secret, with every default in place.
    pub fn new(secret: Vec<u8>) -> Self {
        Totp {
            secret,
            digits: DEFAULT_DIGITS,
            period: DEFAULT_PERIOD,
            algorithm: Algorithm::default(),
            skew: DEFAULT_SKEW,
        }
    }

    /// Rebuild from the base32 secret as it was stored or typed.
    ///
    /// An empty secret is an error rather than a `Totp` over zero bytes. HMAC
    /// accepts an empty key quite happily, so the mistake would not surface
    /// here — it would surface as every user with a blank column sharing the
    /// same predictable codes.
    pub fn from_base32(secret: &str) -> Result<Self> {
        let bytes = base32_decode(secret).ok_or_else(|| {
            Error::msg("that is not a base32 secret: it contains characters outside A-Z and 2-7")
        })?;

        if bytes.is_empty() {
            return Err(Error::msg("a TOTP secret cannot be empty"));
        }

        Ok(Totp::new(bytes))
    }

    /// How many digits the code has. Clamped to RFC 4226's six-to-ten range.
    pub fn with_digits(mut self, digits: u32) -> Self {
        self.digits = clamped_digits(digits);
        self
    }

    /// How long one code lasts, in seconds. Zero is read as one.
    pub fn with_period(mut self, period: u64) -> Self {
        self.period = period.max(1);
        self
    }

    pub fn with_algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// How many steps either side of now [`verify`](Self::verify) accepts.
    ///
    /// Zero demands a perfectly synchronised phone and will lock people out.
    /// Anything above two is a replay window measured in minutes.
    pub fn with_skew(mut self, skew: u64) -> Self {
        self.skew = skew;
        self
    }

    /// The raw secret. As sensitive as a password hash and then some — this one
    /// is the credential itself, not a digest of it.
    pub fn secret(&self) -> &[u8] {
        &self.secret
    }

    /// The secret as an authenticator app wants to see it.
    pub fn secret_base32(&self) -> String {
        base32_encode(&self.secret)
    }

    pub fn digits(&self) -> u32 {
        self.digits
    }

    pub fn period(&self) -> u64 {
        self.period
    }

    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    pub fn skew(&self) -> u64 {
        self.skew
    }

    /// The code for a given unix timestamp, in seconds.
    pub fn at(&self, unix_seconds: u64) -> String {
        self.at_step(step_of(unix_seconds, self.period))
    }

    /// The code for a step number directly — RFC 6238's `T`.
    pub fn at_step(&self, step: u64) -> String {
        hotp_with(self.algorithm, &self.secret, step, self.digits)
    }

    /// The code right now.
    pub fn now(&self) -> String {
        self.at(crate::unix_now())
    }

    /// Whether `code` is one this secret produced around `unix_seconds`.
    ///
    /// Three things are going on here, and each is deliberate.
    ///
    /// **The comparison is constant time.** A code is only six digits, so an
    /// attacker who can measure how long a rejection took can walk them one
    /// position at a time and be through a million-code space in sixty guesses
    /// instead of a million. Every candidate is compared with `subtle`'s
    /// branch-free equality and the results are OR-ed together, so the loop
    /// always runs to the end: returning early on the first match would leak
    /// *which* step matched, which is the phone's clock offset.
    ///
    /// **A window of ±[`skew`](Self::skew) steps is accepted.** Phone clocks
    /// drift, networks add latency, and people start typing at second 29. With
    /// no window a noticeable fraction of honest logins fail, and the support
    /// answer — turn the second factor off — is worse than the widened replay
    /// window. One step either side is RFC 6238 §5.2's recommendation.
    ///
    /// **A malformed code is rejected up front.** The length and the digit
    /// check both short-circuit, and that leaks nothing: how many digits this
    /// configuration uses is printed in the provisioning URI and shown on the
    /// person's phone. What must not vary with timing is the comparison against
    /// the *value*, and that happens below, on inputs already known to be the
    /// right shape.
    ///
    /// It does not, and cannot, tell you whether this code was used a moment
    /// ago. See [`step_of`].
    pub fn verify(&self, code: &str, unix_seconds: u64) -> bool {
        if code.len() != self.digits as usize || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }

        let step = step_of(unix_seconds, self.period);
        let first = step.saturating_sub(self.skew);
        let last = step.saturating_add(self.skew);

        let mut matched = Choice::from(0u8);
        for candidate in first..=last {
            matched |= self.at_step(candidate).as_bytes().ct_eq(code.as_bytes());
        }

        matched.into()
    }

    /// [`verify`](Self::verify) against the system clock.
    pub fn verify_now(&self, code: &str) -> bool {
        self.verify(code, crate::unix_now())
    }

    /// The `otpauth://` URI an authenticator app scans.
    ///
    /// The label is `{issuer}:{account}`, which is what the Key Uri Format
    /// specifies and what makes an app show "ACME (jane@acme.test)" rather than
    /// a bare address. Both halves are percent-encoded *separately* and then
    /// joined with a literal colon, so a colon inside the issuer or the account
    /// arrives as `%3A` and cannot be mistaken for the separator — an account
    /// name is user-controlled, and a user who can move the separator can make
    /// their entry impersonate another issuer in the app's list.
    ///
    /// The URI carries the secret in the clear. It belongs in a QR code on a
    /// page served over TLS, never in a log, an analytics event or a URL a
    /// server records.
    pub fn provisioning_uri(&self, account: &str, issuer: &str) -> String {
        format!(
            "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm={}&digits={}&period={}",
            url::encode(issuer),
            url::encode(account),
            self.secret_base32(),
            url::encode(issuer),
            self.algorithm.name(),
            self.digits,
            self.period,
        )
    }
}

/// Redacted by hand. The secret is the whole second factor, and `{:?}` on a
/// user record is exactly how it would reach a log file.
impl std::fmt::Debug for Totp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Totp")
            .field("secret", &"<redacted>")
            .field("digits", &self.digits)
            .field("period", &self.period)
            .field("algorithm", &self.algorithm)
            .field("skew", &self.skew)
            .finish()
    }
}

/// The alphabet recovery codes are drawn from.
///
/// Thirty-two characters, which is a power of two, so a random byte masked to
/// five bits picks one with no modulo bias. `0`, `1`, `O` and `I` are absent:
/// these codes are printed on paper and read back by somebody who has just lost
/// their phone and is not in a mood to tell an oh from a nought. Only uppercase
/// is used, which is what keeps `L` in — it is a lowercase `l` that gets
/// mistaken for a one, and [`normalise`] uppercases before it compares.
///
/// Dropping a fifth character would leave thirty-one, and a 31-symbol alphabet
/// cannot be sampled from a byte without either bias or a rejection loop.
pub const RECOVERY_ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

/// How many characters a recovery code carries, before formatting.
///
/// Ten characters from a 32-symbol alphabet is fifty bits — far past anything
/// guessable, and short enough to type twice without losing patience.
pub const RECOVERY_CODE_LENGTH: usize = 10;

/// A freshly generated set of recovery codes, in the clear.
///
/// The one moment they exist readably. Show them once, tell the person to print
/// them, store only [`hashed`](Self::hashed) — exactly like a password, and for
/// the same reason: each one is a complete bypass of the second factor.
#[derive(Clone)]
pub struct RecoveryCodes {
    codes: Vec<String>,
}

impl RecoveryCodes {
    /// Mint `count` codes. Ten is the usual number, and what Laravel issues.
    pub fn generate(count: usize) -> RecoveryCodes {
        // One byte per character, drawn in a single read: five bits of each is
        // used and the rest discarded, which is cheaper than being clever and
        // cannot introduce the bias that `% 32` over a non-power-of-two
        // alphabet would.
        let bytes = random::bytes(count * RECOVERY_CODE_LENGTH);

        let codes = bytes
            .chunks(RECOVERY_CODE_LENGTH)
            .map(|chunk| {
                let characters: String = chunk
                    .iter()
                    .map(|byte| RECOVERY_ALPHABET[(byte & 0x1f) as usize] as char)
                    .collect();
                punctuate(&characters)
            })
            .collect();

        RecoveryCodes { codes }
    }

    /// The codes, to show once and never again.
    pub fn codes(&self) -> &[String] {
        &self.codes
    }

    pub fn into_codes(self) -> Vec<String> {
        self.codes
    }

    /// What to store: an argon2 hash of each code.
    ///
    /// Argon2 rather than SHA-256, unlike [`crate::tokens`], and the difference
    /// is deliberate. An API token is 256 bits of CSPRNG output, so there is
    /// nothing to search; a recovery code is fifty bits *and is typed by a
    /// person*, which means it may well be written down, photographed, or
    /// entered into the wrong box. Fifty bits is beyond an online attacker but
    /// it is not beyond somebody who has stolen the table, and a KDF is what
    /// makes the difference. They are also verified once, on a rare path, so
    /// the cost buys the protection without being felt.
    pub fn hashed(&self) -> Vec<String> {
        self.hashed_with(Cost::DEFAULT)
    }

    /// [`hashed`](Self::hashed) at an explicit cost, for tests. See
    /// [`Cost::FAST`], and do not store real codes hashed with it.
    pub fn hashed_with(&self, cost: Cost) -> Vec<String> {
        self.codes
            .iter()
            .map(|code| {
                hash_password_with(code, cost)
                    .expect("argon2 accepts every Cost this crate can construct")
            })
            .collect()
    }
}

/// Redacted, for the reason [`Totp`]'s is.
impl std::fmt::Debug for RecoveryCodes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryCodes")
            .field("count", &self.codes.len())
            .field("codes", &"<redacted>")
            .finish()
    }
}

/// Hash a single recovery code the way [`RecoveryCodes::hashed`] does.
///
/// Useful when codes are stored one row at a time rather than as a set.
pub fn hash_recovery_code(code: &str) -> Result<String> {
    hash_password(&normalise(code))
}

/// Punctuate ten characters as `xxxxx-xxxxx`.
///
/// Purely so the eye can find its place halfway through. The dash is not part
/// of the code — [`normalise`] throws it away again — so somebody who types the
/// ten characters straight through is not turned away.
fn punctuate(characters: &str) -> String {
    if characters.len() != RECOVERY_CODE_LENGTH {
        return characters.to_string();
    }
    format!("{}-{}", &characters[..5], &characters[5..])
}

/// Reduce a typed code to the form that was hashed.
///
/// Uppercased, with everything outside the alphabet dropped, then re-punctuated.
/// That absorbs the dash, the spaces a phone keyboard inserts, and the
/// lowercase a person types — none of which carry information, and all of which
/// would otherwise reject a correct code at the worst possible moment.
fn normalise(code: &str) -> String {
    let characters: String = code
        .chars()
        .map(|character| character.to_ascii_uppercase())
        .filter(|character| {
            character.is_ascii() && RECOVERY_ALPHABET.contains(&(*character as u8))
        })
        .collect();

    punctuate(&characters)
}

/// Spend a recovery code: verify it, remove the hash it matched, and say so.
///
/// One-time by construction. `hashes` is the stored list and it is edited in
/// place, so the caller's next step is to persist it — a code that is accepted
/// and left in the list is a permanent second-factor bypass, which is the whole
/// thing this function exists to prevent.
///
/// Not constant time across the list, and it does not need to be: fifty bits of
/// entropy means an attacker cannot get close enough for the timing of *which*
/// hash matched to be worth anything. What it does cost is one argon2
/// verification per stored hash on a miss, so this path must be rate-limited
/// like a login — ten hashes at the default cost is most of half a second of
/// CPU per attempt.
pub fn consume_recovery_code(code: &str, hashes: &mut Vec<String>) -> bool {
    let normalised = normalise(code);
    if normalised.is_empty() {
        return false;
    }

    let Some(position) = hashes.iter().position(|hash| verify_password(&normalised, hash)) else {
        return false;
    };

    hashes.remove(position);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4226 Appendix D and the SHA-1 rows of RFC 6238 Appendix B.
    const RFC_SECRET_SHA1: &[u8] = b"12345678901234567890";
    /// RFC 6238 Appendix B, SHA-256 rows: the seed is extended to 32 bytes.
    const RFC_SECRET_SHA256: &[u8] = b"12345678901234567890123456789012";
    /// RFC 6238 Appendix B, SHA-512 rows: extended to 64 bytes.
    const RFC_SECRET_SHA512: &[u8] =
        b"1234567890123456789012345678901234567890123456789012345678901234";

    #[test]
    fn base32_matches_the_rfc_4648_test_vectors() {
        // RFC 4648 §10, minus the padding this encoder does not emit.
        let vectors = [
            ("", ""),
            ("f", "MY"),
            ("fo", "MZXQ"),
            ("foo", "MZXW6"),
            ("foob", "MZXW6YQ"),
            ("fooba", "MZXW6YTB"),
            ("foobar", "MZXW6YTBOI"),
        ];

        for (plain, encoded) in vectors {
            assert_eq!(base32_encode(plain.as_bytes()), encoded, "encoding {plain:?}");
            assert_eq!(
                base32_decode(encoded).as_deref(),
                Some(plain.as_bytes()),
                "decoding {encoded:?}"
            );
        }
    }

    #[test]
    fn base32_round_trips_every_length_up_to_a_full_group() {
        for length in 0..=32 {
            let bytes: Vec<u8> = (0..length).map(|i| i as u8 ^ 0xa5).collect();
            let encoded = base32_encode(&bytes);

            assert!(encoded.bytes().all(|b| BASE32_ALPHABET.contains(&b)), "{encoded}");
            assert_eq!(base32_decode(&encoded), Some(bytes), "at length {length}");
        }
    }

    #[test]
    fn decoding_forgives_case_spaces_and_padding() {
        let expected = Some(b"foobar".to_vec());

        assert_eq!(base32_decode("mzxw6ytboi"), expected);
        assert_eq!(base32_decode("MZXW 6YTB OI"), expected);
        assert_eq!(base32_decode("mzxw 6ytb OI"), expected);
        assert_eq!(base32_decode("MZXW6YTBOI======"), expected);
        assert_eq!(base32_decode("MZXW6YTB OI==\n"), expected);
    }

    #[test]
    fn decoding_rejects_anything_outside_the_alphabet() {
        // `0`, `1` and `8` are the classic mistypes, and each has to be an
        // error rather than a quietly different secret.
        for text in ["MZXW6YTB0I", "MZXW6YTB1I", "MZXW6YTB8I", "MZXW-6YTB", "MZXW6YTB!", "héllo"] {
            assert_eq!(base32_decode(text), None, "{text:?} should not decode");
        }
    }

    #[test]
    fn hotp_reproduces_the_whole_rfc_4226_appendix_d_table() {
        let expected = [
            "755224", "287082", "359152", "969429", "338314", "254676", "287922", "162583",
            "399871", "520489",
        ];

        for (counter, code) in expected.iter().enumerate() {
            assert_eq!(
                &hotp(RFC_SECRET_SHA1, counter as u64, 6),
                code,
                "RFC 4226 counter {counter}"
            );
        }
    }

    #[test]
    fn totp_reproduces_the_rfc_6238_appendix_b_table_for_sha1() {
        // The RFC prints the table with one seed for every row, but the values
        // were computed with a seed per algorithm — errata 2866. These are the
        // 20-byte seed's rows.
        let vectors = [
            (59u64, "94287082"),
            (1_111_111_109, "07081804"),
            (1_111_111_111, "14050471"),
            (1_234_567_890, "89005924"),
            (2_000_000_000, "69279037"),
            (20_000_000_000, "65353130"),
        ];

        let totp = Totp::new(RFC_SECRET_SHA1.to_vec()).with_digits(8);
        for (at, code) in vectors {
            assert_eq!(totp.at(at), code, "RFC 6238 SHA-1 at T={at}");
        }
    }

    #[test]
    fn totp_reproduces_the_rfc_6238_appendix_b_table_for_sha256() {
        let vectors = [
            (59u64, "46119246"),
            (1_111_111_109, "68084774"),
            (1_111_111_111, "67062674"),
            (1_234_567_890, "91819424"),
            (2_000_000_000, "90698825"),
            (20_000_000_000, "77737706"),
        ];

        let totp = Totp::new(RFC_SECRET_SHA256.to_vec())
            .with_digits(8)
            .with_algorithm(Algorithm::Sha256);
        for (at, code) in vectors {
            assert_eq!(totp.at(at), code, "RFC 6238 SHA-256 at T={at}");
        }
    }

    #[test]
    fn totp_reproduces_the_rfc_6238_appendix_b_table_for_sha512() {
        let vectors = [
            (59u64, "90693936"),
            (1_111_111_109, "25091201"),
            (1_111_111_111, "99943326"),
            (1_234_567_890, "93441116"),
            (2_000_000_000, "38618901"),
            (20_000_000_000, "47863826"),
        ];

        let totp = Totp::new(RFC_SECRET_SHA512.to_vec())
            .with_digits(8)
            .with_algorithm(Algorithm::Sha512);
        for (at, code) in vectors {
            assert_eq!(totp.at(at), code, "RFC 6238 SHA-512 at T={at}");
        }
    }

    #[test]
    fn the_rfc_6238_times_land_on_the_steps_the_rfc_prints() {
        // Appendix B's "T (hex)" column, which is the one number the whole
        // algorithm hangs on.
        assert_eq!(step_of(59, 30), 0x1);
        assert_eq!(step_of(1_111_111_109, 30), 0x23523EC);
        assert_eq!(step_of(1_111_111_111, 30), 0x23523ED);
        assert_eq!(step_of(1_234_567_890, 30), 0x273EF07);
        assert_eq!(step_of(2_000_000_000, 30), 0x3F940AA);
        assert_eq!(step_of(20_000_000_000, 30), 0x27BC86AA);
    }

    #[test]
    fn a_step_is_the_timestamp_divided_by_the_period() {
        assert_eq!(step_of(0, 30), 0);
        assert_eq!(step_of(29, 30), 0);
        assert_eq!(step_of(30, 30), 1);
        assert_eq!(step_of(61, 60), 1);

        // A zero period is read as one second rather than dividing by zero.
        assert_eq!(step_of(7, 0), 7);
    }

    #[test]
    fn the_default_shape_is_six_digits_over_thirty_seconds_of_sha1() {
        let totp = Totp::generate();

        assert_eq!(totp.digits(), 6);
        assert_eq!(totp.period(), 30);
        assert_eq!(totp.algorithm(), Algorithm::Sha1);
        assert_eq!(totp.skew(), 1);
        assert_eq!(totp.secret().len(), SECRET_BYTES);
        assert_eq!(totp.now().len(), 6);
        assert!(totp.now().bytes().all(|b| b.is_ascii_digit()));
    }

    #[test]
    fn two_generated_secrets_differ() {
        assert_ne!(Totp::generate().secret(), Totp::generate().secret());
    }

    #[test]
    fn a_secret_survives_the_base32_round_trip() {
        let totp = Totp::generate();
        let encoded = totp.secret_base32();

        assert_eq!(encoded.len(), 32, "20 bytes is 32 base32 characters");
        let rebuilt = Totp::from_base32(&encoded).expect("its own output should parse");

        assert_eq!(rebuilt.secret(), totp.secret());
        assert_eq!(rebuilt.at(1_234_567_890), totp.at(1_234_567_890));
    }

    #[test]
    fn an_empty_or_malformed_secret_is_refused_rather_than_accepted_as_zero_bytes() {
        assert!(Totp::from_base32("").is_err());
        assert!(Totp::from_base32("   ").is_err());
        assert!(Totp::from_base32("====").is_err());
        assert!(Totp::from_base32("MZXW6YTB0I").is_err());

        assert!(Totp::from_base32("mzxw 6ytb oi").is_ok(), "leniency still applies");
    }

    #[test]
    fn the_current_code_verifies() {
        let totp = Totp::generate();
        let now = crate::unix_now();

        assert!(totp.verify(&totp.at(now), now));
        assert!(totp.verify_now(&totp.now()));
    }

    #[test]
    fn one_step_either_side_is_accepted_and_two_are_not() {
        let totp = Totp::new(RFC_SECRET_SHA1.to_vec());
        let now = 1_234_567_890;

        assert!(totp.verify(&totp.at(now - 30), now), "a code from the previous step");
        assert!(totp.verify(&totp.at(now), now));
        assert!(totp.verify(&totp.at(now + 30), now), "and one from the next");

        assert!(!totp.verify(&totp.at(now - 60), now), "two steps back is outside the window");
        assert!(!totp.verify(&totp.at(now + 60), now));
    }

    #[test]
    fn the_window_is_configurable_in_both_directions() {
        let secret = RFC_SECRET_SHA1.to_vec();
        let now = 1_234_567_890;

        let strict = Totp::new(secret.clone()).with_skew(0);
        assert!(strict.verify(&strict.at(now), now));
        assert!(!strict.verify(&strict.at(now - 30), now), "no window means no drift allowed");

        let generous = Totp::new(secret).with_skew(2);
        assert!(generous.verify(&generous.at(now - 60), now));
        assert!(!generous.verify(&generous.at(now - 90), now));
    }

    #[test]
    fn the_window_does_not_underflow_at_the_epoch() {
        // A clock that has not been set yet reports a timestamp near zero, and
        // `step - skew` must not wrap around to the far end of u64.
        let totp = Totp::new(RFC_SECRET_SHA1.to_vec()).with_skew(5);

        assert!(totp.verify(&totp.at(0), 0));
        assert!(!totp.verify("000000", u64::MAX));
    }

    #[test]
    fn a_code_from_another_secret_is_rejected() {
        let mine = Totp::generate();
        let yours = Totp::generate();
        let now = crate::unix_now();

        assert!(!mine.verify(&yours.at(now), now));
        assert!(!yours.verify(&mine.at(now), now));
    }

    #[test]
    fn a_code_of_the_wrong_shape_is_rejected_without_panicking() {
        let totp = Totp::generate();
        let now = crate::unix_now();
        let real = totp.at(now);
        let too_short = real[..5].to_string();
        let too_long = format!("{real}0");
        let padded = format!(" {real}");

        for code in [
            "",
            "12345",
            "1234567",
            "12345 ",
            " 123456",
            "abcdef",
            "12345a",
            "12-345",
            // Arabic-Indic digits: `is_ascii_digit` says no, and it must.
            "١٢٣٤٥٦",
            too_short.as_str(),
            too_long.as_str(),
            padded.as_str(),
        ] {
            assert!(!totp.verify(code, now), "{code:?} should not verify");
        }

        // And the real one still does, so the test is not passing by refusing
        // everything.
        assert!(totp.verify(&real, now));
    }

    #[test]
    fn a_different_algorithm_produces_a_different_code_from_the_same_secret() {
        let secret = RFC_SECRET_SHA1.to_vec();
        let at = 1_234_567_890;

        let sha1 = Totp::new(secret.clone()).with_digits(8);
        let sha256 = Totp::new(secret.clone()).with_digits(8).with_algorithm(Algorithm::Sha256);
        let sha512 = Totp::new(secret).with_digits(8).with_algorithm(Algorithm::Sha512);

        assert_ne!(sha1.at(at), sha256.at(at));
        assert_ne!(sha256.at(at), sha512.at(at));
        assert!(!sha1.verify(&sha256.at(at), at), "the algorithm has to match on both ends");
    }

    #[test]
    fn digits_are_clamped_to_the_range_rfc_4226_allows() {
        let secret = RFC_SECRET_SHA1.to_vec();

        assert_eq!(Totp::new(secret.clone()).with_digits(0).digits(), 6);
        assert_eq!(Totp::new(secret.clone()).with_digits(4).digits(), 6);
        assert_eq!(Totp::new(secret.clone()).with_digits(8).digits(), 8);
        assert_eq!(Totp::new(secret.clone()).with_digits(99).digits(), 10);

        assert_eq!(hotp(RFC_SECRET_SHA1, 0, 0).len(), 6);
        assert_eq!(hotp(RFC_SECRET_SHA1, 0, 8), "84755224", "the RFC's 8-digit form of row 0");
        assert_eq!(hotp(RFC_SECRET_SHA1, 0, 99).len(), 10);
    }

    #[test]
    fn a_period_of_zero_is_read_as_one_second() {
        let totp = Totp::new(RFC_SECRET_SHA1.to_vec()).with_period(0);

        assert_eq!(totp.period(), 1);
        assert!(totp.verify(&totp.at(1_000), 1_000));
    }

    #[test]
    fn the_provisioning_uri_carries_every_parameter() {
        let totp = Totp::new(b"foobar".to_vec());
        let uri = totp.provisioning_uri("jane@acme.test", "ACME");

        assert_eq!(
            uri,
            "otpauth://totp/ACME:jane%40acme.test\
             ?secret=MZXW6YTBOI&issuer=ACME&algorithm=SHA1&digits=6&period=30"
        );
    }

    #[test]
    fn a_label_with_a_space_or_a_colon_is_percent_encoded() {
        let totp = Totp::new(b"foobar".to_vec())
            .with_digits(8)
            .with_period(60)
            .with_algorithm(Algorithm::Sha512);

        let uri = totp.provisioning_uri("jane doe:admin", "ACME Corp");

        // The separating colon is the only literal one; the account's own colon
        // is escaped, so it cannot pose as the issuer boundary.
        assert!(uri.starts_with("otpauth://totp/ACME%20Corp:jane%20doe%3Aadmin?"), "{uri}");
        assert_eq!(uri.matches(':').count(), 2, "one in the scheme, one separating the label");

        assert!(uri.contains("&issuer=ACME%20Corp"));
        assert!(uri.contains("&algorithm=SHA512"));
        assert!(uri.contains("&digits=8"));
        assert!(uri.contains("&period=60"));
    }

    #[test]
    fn algorithm_names_round_trip() {
        for algorithm in [Algorithm::Sha1, Algorithm::Sha256, Algorithm::Sha512] {
            assert_eq!(Algorithm::parse(algorithm.name()), Some(algorithm));
            assert_eq!(algorithm.to_string(), algorithm.name());
        }

        assert_eq!(Algorithm::parse("sha1"), Some(Algorithm::Sha1));
        assert_eq!(Algorithm::parse("SHA-256"), Some(Algorithm::Sha256));
        assert_eq!(Algorithm::parse("md5"), None);
        assert_eq!(Algorithm::parse(""), None);
    }

    #[test]
    fn a_secret_never_appears_in_debug_output() {
        let totp = Totp::generate();
        let printed = format!("{totp:?}");

        assert!(!printed.contains(&totp.secret_base32()), "the secret escaped into {printed}");
        assert!(printed.contains("<redacted>"));
        // The parameters are still there, or the redaction is too blunt to help.
        assert!(printed.contains("digits: 6"));
        assert!(printed.contains("Sha1"));
    }

    #[test]
    fn recovery_codes_are_typeable_and_unambiguous() {
        let codes = RecoveryCodes::generate(10);

        assert_eq!(codes.codes().len(), 10);
        for code in codes.codes() {
            assert_eq!(code.len(), RECOVERY_CODE_LENGTH + 1, "{code} should be xxxxx-xxxxx");
            assert_eq!(&code[5..6], "-");

            for character in code.chars().filter(|c| *c != '-') {
                assert!(RECOVERY_ALPHABET.contains(&(character as u8)), "{code} contains {character}");
            }
            for confusable in ['0', 'O', '1', 'I', 'l'] {
                assert!(!code.contains(confusable), "{code} contains a {confusable}");
            }
        }
    }

    #[test]
    fn every_generated_code_is_distinct() {
        let codes = RecoveryCodes::generate(50);
        let mut seen = codes.codes().to_vec();
        seen.sort();
        seen.dedup();

        assert_eq!(seen.len(), 50, "50 bits of entropy should not collide");
        assert_ne!(RecoveryCodes::generate(1).codes(), RecoveryCodes::generate(1).codes());
    }

    #[test]
    fn a_recovery_code_works_exactly_once() {
        let codes = RecoveryCodes::generate(3);
        let mut hashes = codes.hashed_with(Cost::FAST);
        let spent = codes.codes()[1].clone();

        assert!(consume_recovery_code(&spent, &mut hashes), "the first use should be accepted");
        assert_eq!(hashes.len(), 2, "the hash it matched must be gone");

        assert!(!consume_recovery_code(&spent, &mut hashes), "the second use must not be");
        assert_eq!(hashes.len(), 2);

        // The others are untouched.
        assert!(consume_recovery_code(&codes.codes()[0], &mut hashes));
        assert!(consume_recovery_code(&codes.codes()[2], &mut hashes));
        assert!(hashes.is_empty());
    }

    #[test]
    fn an_unknown_code_is_refused_and_removes_nothing() {
        let codes = RecoveryCodes::generate(3);
        let mut hashes = codes.hashed_with(Cost::FAST);

        for guess in ["", "-", "22222-22222", "not a code", &codes.codes()[0].replace('-', "X")] {
            assert!(!consume_recovery_code(guess, &mut hashes), "{guess:?} should not be accepted");
        }
        assert_eq!(hashes.len(), 3);

        // A code from a different set is just as unknown.
        let others = RecoveryCodes::generate(1);
        assert!(!consume_recovery_code(&others.codes()[0], &mut hashes));
        assert_eq!(hashes.len(), 3);
    }

    #[test]
    fn a_code_is_accepted_however_it_was_typed() {
        let codes = RecoveryCodes::generate(1);
        let code = codes.codes()[0].clone();

        for typed in [
            code.clone(),
            code.to_lowercase(),
            code.replace('-', ""),
            code.replace('-', " "),
            format!("  {code}  "),
        ] {
            let mut hashes = codes.hashed_with(Cost::FAST);
            assert!(consume_recovery_code(&typed, &mut hashes), "{typed:?} should be accepted");
        }
    }

    #[test]
    fn the_stored_hashes_reveal_nothing_about_the_codes() {
        let codes = RecoveryCodes::generate(5);
        let hashes = codes.hashed_with(Cost::FAST);
        let stored = hashes.join(" ");

        for code in codes.codes() {
            assert!(!stored.contains(code.as_str()), "{code} was stored in the clear");
            assert!(!stored.contains(&code.replace('-', "")));
        }

        for hash in &hashes {
            assert!(hash.starts_with("$argon2id$"), "unexpected hash: {hash}");
        }

        // Two identical codes would still hash differently — each hash carries
        // its own salt — so the list leaks not even which codes repeat.
        let repeated = RecoveryCodes { codes: vec!["ABCDE-FGHJK".into(), "ABCDE-FGHJK".into()] };
        let pair = repeated.hashed_with(Cost::FAST);
        assert_ne!(pair[0], pair[1]);
    }

    #[test]
    fn a_set_of_codes_never_prints_itself() {
        let codes = RecoveryCodes::generate(2);
        let printed = format!("{codes:?}");

        for code in codes.codes() {
            assert!(!printed.contains(code.as_str()), "a code escaped into {printed}");
        }
        assert!(printed.contains("<redacted>"));
        assert!(printed.contains("count: 2"));
    }

    #[test]
    fn a_single_code_can_be_hashed_and_spent_on_its_own() {
        let codes = RecoveryCodes::generate(1);
        let code = codes.codes()[0].clone();

        let mut hashes = vec![hash_recovery_code(&code).expect("the default cost is valid")];

        assert!(consume_recovery_code(&code.to_lowercase(), &mut hashes));
        assert!(hashes.is_empty());
    }
}
