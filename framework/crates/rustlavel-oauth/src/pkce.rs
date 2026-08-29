//! PKCE — Proof Key for Code Exchange, RFC 7636.
//!
//! The problem PKCE solves: an authorisation code travels back through a
//! browser redirect, where another application on the device can intercept it.
//! With only a code, the attacker can trade it for a token. PKCE makes the
//! client commit to a secret up front — it sends the *hash* of a random
//! verifier with the authorisation request, and the verifier itself with the
//! token request. An intercepted code is useless without the verifier.
//!
//! OAuth 2.1 requires this for every authorisation code flow, public client or
//! not, which is why nothing here makes it optional.

use rustlavel_auth::{base64, random};
use sha2::{Digest, Sha256};

/// How the challenge was derived from the verifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChallengeMethod {
    /// SHA-256. The only method that offers any protection.
    #[default]
    S256,
    /// The challenge *is* the verifier. RFC 7636 §4.2 permits it only where
    /// SHA-256 is genuinely unavailable; it gives an interceptor the verifier
    /// along with the code, so it protects against nothing.
    Plain,
}

impl ChallengeMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            ChallengeMethod::S256 => "S256",
            ChallengeMethod::Plain => "plain",
        }
    }

    pub fn parse(raw: &str) -> Option<ChallengeMethod> {
        match raw {
            "S256" => Some(ChallengeMethod::S256),
            "plain" => Some(ChallengeMethod::Plain),
            _ => None,
        }
    }
}

impl std::fmt::Display for ChallengeMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A verifier and the challenge derived from it.
///
/// The verifier is a secret: it is held by the client between the two legs of
/// the flow and never appears in a URL. `Debug` redacts it so it cannot reach a
/// log by accident.
#[derive(Clone)]
pub struct Pkce {
    verifier: String,
    method: ChallengeMethod,
}

impl Pkce {
    /// A fresh verifier from the OS CSPRNG.
    ///
    /// RFC 7636 §4.1 allows 43–128 characters; 32 random bytes base64url-encode
    /// to 43, which is the minimum length and already 256 bits of entropy.
    pub fn generate() -> Pkce {
        Pkce {
            verifier: base64::encode_url(&random::bytes(32)),
            method: ChallengeMethod::S256,
        }
    }

    /// Rebuild from a verifier the client stored between the two legs.
    ///
    /// Rejects anything outside RFC 7636's grammar rather than passing it on:
    /// a verifier the provider will reject is better caught here, where the
    /// error can say why.
    pub fn from_verifier(verifier: impl Into<String>) -> Result<Pkce, String> {
        let verifier = verifier.into();
        validate_verifier(&verifier)?;
        Ok(Pkce { verifier, method: ChallengeMethod::S256 })
    }

    /// Use `plain` instead of S256. Almost always the wrong choice.
    pub fn plain(mut self) -> Pkce {
        self.method = ChallengeMethod::Plain;
        self
    }

    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    pub fn method(&self) -> ChallengeMethod {
        self.method
    }

    /// The challenge sent with the authorisation request.
    pub fn challenge(&self) -> String {
        match self.method {
            ChallengeMethod::S256 => s256(&self.verifier),
            ChallengeMethod::Plain => self.verifier.clone(),
        }
    }
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pkce")
            .field("verifier", &"<redacted>")
            .field("method", &self.method)
            .finish()
    }
}

/// `BASE64URL(SHA256(ASCII(verifier)))`, without padding — RFC 7636 §4.2.
pub fn s256(verifier: &str) -> String {
    base64::encode_url(&Sha256::digest(verifier.as_bytes()))
}

/// Check a verifier against the challenge that was committed to earlier.
///
/// This is the server's half of PKCE. The comparison is constant-time: the
/// challenge is a public value, but a timing oracle on the derived digest would
/// still leak information about the verifier an attacker is guessing.
pub fn verify(verifier: &str, challenge: &str, method: ChallengeMethod) -> bool {
    if validate_verifier(verifier).is_err() {
        return false;
    }

    let derived = match method {
        ChallengeMethod::S256 => s256(verifier),
        ChallengeMethod::Plain => verifier.to_string(),
    };

    rustlavel_auth::constant_time_eq(derived.as_bytes(), challenge.as_bytes())
}

/// RFC 7636 §4.1: 43–128 characters from `[A-Za-z0-9-._~]`.
///
/// The length floor is the point of the rule — a short verifier is guessable,
/// and guessing it defeats the whole exchange.
pub fn validate_verifier(verifier: &str) -> Result<(), String> {
    if !(43..=128).contains(&verifier.len()) {
        return Err(format!(
            "a PKCE verifier must be 43 to 128 characters (RFC 7636 §4.1); this one is {}",
            verifier.len()
        ));
    }

    match verifier.bytes().find(|byte| {
        !matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~')
    }) {
        Some(byte) => Err(format!(
            "a PKCE verifier may only contain [A-Za-z0-9-._~]; this one contains {:?}",
            byte as char
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_worked_example_from_rfc_7636() {
        // Appendix B, the vector every implementation is checked against.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(s256(verifier), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn a_generated_verifier_satisfies_the_grammar() {
        let pkce = Pkce::generate();

        assert_eq!(pkce.verifier().len(), 43, "32 bytes of entropy, unpadded");
        validate_verifier(pkce.verifier()).expect("generated verifiers must be valid");
    }

    #[test]
    fn two_verifiers_are_never_the_same() {
        assert_ne!(Pkce::generate().verifier(), Pkce::generate().verifier());
    }

    #[test]
    fn the_challenge_is_not_the_verifier_under_s256() {
        let pkce = Pkce::generate();
        assert_ne!(pkce.challenge(), pkce.verifier());
        assert_eq!(pkce.challenge(), s256(pkce.verifier()));
    }

    #[test]
    fn plain_leaks_the_verifier_into_the_url() {
        // Asserting the property rather than approving of it: this is exactly
        // why `plain` protects against nothing.
        let pkce = Pkce::generate().plain();
        assert_eq!(pkce.challenge(), pkce.verifier());
    }

    #[test]
    fn the_right_verifier_passes_and_a_wrong_one_does_not() {
        let pkce = Pkce::generate();
        let challenge = pkce.challenge();

        assert!(verify(pkce.verifier(), &challenge, ChallengeMethod::S256));
        assert!(!verify(Pkce::generate().verifier(), &challenge, ChallengeMethod::S256));
    }

    #[test]
    fn a_verifier_cannot_be_replayed_under_the_wrong_method() {
        // An attacker who holds the challenge must not be able to pass it off
        // as the verifier by claiming `plain`.
        let pkce = Pkce::generate();
        let challenge = pkce.challenge();

        assert!(!verify(&challenge, &challenge, ChallengeMethod::S256));
    }

    #[test]
    fn verification_refuses_a_malformed_verifier_outright() {
        // Without the length check, "" hashed and compared would be a perfectly
        // ordinary verifier — the grammar is what makes it guess-resistant.
        assert!(!verify("", &s256(""), ChallengeMethod::S256));
        assert!(!verify("short", &s256("short"), ChallengeMethod::S256));
    }

    #[test]
    fn the_grammar_error_says_what_is_wrong() {
        let too_short = validate_verifier("abc").unwrap_err();
        assert!(too_short.contains("43 to 128"), "got {too_short}");

        let long_enough_but_illegal = format!("{}/{}", "a".repeat(21), "b".repeat(21));
        let bad_character = validate_verifier(&long_enough_but_illegal).unwrap_err();
        assert!(bad_character.contains('/'), "got {bad_character}");
    }

    #[test]
    fn a_verifier_at_each_boundary_is_accepted() {
        assert!(validate_verifier(&"a".repeat(43)).is_ok());
        assert!(validate_verifier(&"a".repeat(128)).is_ok());
        assert!(validate_verifier(&"a".repeat(42)).is_err());
        assert!(validate_verifier(&"a".repeat(129)).is_err());
    }

    #[test]
    fn from_verifier_rejects_what_the_provider_would_reject() {
        assert!(Pkce::from_verifier("too-short").is_err());
        assert!(Pkce::from_verifier("a".repeat(43)).is_ok());
    }

    #[test]
    fn methods_round_trip_through_their_wire_spelling() {
        assert_eq!(ChallengeMethod::parse("S256"), Some(ChallengeMethod::S256));
        assert_eq!(ChallengeMethod::parse("plain"), Some(ChallengeMethod::Plain));
        assert_eq!(ChallengeMethod::parse("s256"), None, "the spelling is case-sensitive");
        assert_eq!(ChallengeMethod::parse("none"), None);
    }

    #[test]
    fn debug_does_not_print_the_verifier() {
        let pkce = Pkce::generate();
        let printed = format!("{pkce:?}");

        assert!(!printed.contains(pkce.verifier()), "the verifier reached a log");
        assert!(printed.contains("redacted"));
    }
}
