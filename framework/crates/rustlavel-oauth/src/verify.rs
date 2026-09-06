//! Deciding whether a bearer token is good, the two ways there are.
//!
//! A resource server has to answer one question on every request — *is this
//! token valid, and what may it do* — and there are exactly two honest answers
//! to how.
//!
//! **Introspection** (RFC 7662) asks the authorization server. It is always
//! current: a token revoked a second ago is refused a second ago. It costs a
//! network hop per request unless the answer is cached, and it makes the
//! authorization server something every request depends on.
//!
//! **A self-contained token** carries its own claims and a signature, and the
//! resource server checks the signature with a public key it already holds. No
//! hop, no dependency, and revocation becomes hard: a token handed out is good
//! until it expires, whatever anybody decides in between.
//!
//! Both are here because both are right for different deployments, and the risk
//! of shipping two is worth naming: **a security path that is rarely used is the
//! one that is wrong without anybody knowing.** So neither is the real one with
//! the other bolted on — they return the same [`Claims`], they refuse for the
//! same reasons, and the tests below cover them equally.
//!
//! The signature is ES256 over `p256`, which this tree already carries. The JWT
//! *format* — base64url, JSON, the signing input — is written here like every
//! other format in this project. The primitive is not: a hand-rolled signature
//! is exactly what the cryptography exception exists to prevent.

use rustlavel_auth::base64;
use rustlavel_core::{Error, Json, Result};

use crate::Scopes;

/// What a valid token says about itself.
///
/// The same shape whichever way it was checked, so a resource server is written
/// once and the deployment decides how the answer is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    /// Who the token is for — a user id, usually.
    pub subject: String,
    /// Which client obtained it.
    pub client: Option<String>,
    /// What it may do.
    pub scopes: Scopes,
    /// Seconds since the epoch, or `None` for a token that does not say.
    pub expires_at: Option<i64>,
}

impl Claims {
    /// Whether this token has run out at `now`.
    ///
    /// A token with no expiry is *not* treated as expired: an introspection
    /// response may legitimately omit `exp`, and refusing those would refuse
    /// every token from a server that does not send it.
    pub fn expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }
}

/// Read a self-contained token, verifying its signature.
///
/// `public_key` is a SEC1 point — the uncompressed `04 || x || y` an
/// authorization server publishes.
///
/// Every failure is the same `None`-shaped refusal on purpose. Saying *which*
/// part of a token was wrong tells somebody holding a forgery how much closer
/// they are getting.
pub fn verify_signed(token: &str, public_key: &[u8], now: i64) -> Result<Claims> {
    use p256::ecdsa::signature::Verifier;

    let refuse = || Error::msg("the token was refused");

    let mut parts = token.split('.');
    let (header, payload, signature) = match (parts.next(), parts.next(), parts.next(), parts.next())
    {
        (Some(header), Some(payload), Some(signature), None) => (header, payload, signature),
        // Four parts is an encrypted token, which this does not read; fewer is
        // not a token at all. Both are refused rather than guessed at.
        _ => return Err(refuse()),
    };

    // The algorithm is checked, and checked against a list of one. A token
    // that names its own algorithm and is believed is the oldest JWT bug
    // there is: `alg: none` verifies against nothing, and `alg: HS256` invites
    // a server holding a public key to use it as an HMAC secret — which the
    // attacker also has, because it is public.
    let head = Json::parse(&decode_part(header).ok_or_else(refuse)?).map_err(|_| refuse())?;
    if head.get("alg").and_then(Json::as_str) != Some("ES256") {
        return Err(refuse());
    }

    let point = p256::EncodedPoint::from_bytes(public_key).map_err(|_| refuse())?;
    let key = p256::ecdsa::VerifyingKey::from_encoded_point(&point).map_err(|_| refuse())?;

    let raw = base64::decode(signature).ok_or_else(refuse)?;
    let parsed = p256::ecdsa::Signature::from_slice(&raw).map_err(|_| refuse())?;

    let signed = format!("{header}.{payload}");
    key.verify(signed.as_bytes(), &parsed).map_err(|_| refuse())?;

    // Only after the signature. Reading claims out of an unverified token and
    // acting on them is how a forgery gets a foothold, even when the code
    // "checks the signature" a few lines further down.
    let body = Json::parse(&decode_part(payload).ok_or_else(refuse)?).map_err(|_| refuse())?;
    let claims = claims_from(&body).ok_or_else(refuse)?;

    if claims.expired(now) {
        return Err(refuse());
    }
    Ok(claims)
}

/// Read an RFC 7662 introspection response.
///
/// `active` is the whole answer: the specification says a server that will not
/// say anything else still says this, and a response without it is not one to
/// read optimistically.
pub fn read_introspection(body: &Json, now: i64) -> Result<Claims> {
    let refuse = || Error::msg("the token was refused");

    if body.get("active").and_then(Json::as_bool) != Some(true) {
        return Err(refuse());
    }
    let claims = claims_from(body).ok_or_else(refuse)?;

    // Checked here too, even though `active` should already account for it.
    // The clock that matters is this server's, and a cached response outlives
    // the moment it was true.
    if claims.expired(now) {
        return Err(refuse());
    }
    Ok(claims)
}

/// The claims shared by both shapes. RFC 7662 and RFC 9068 agree on these
/// names, which is why one reader serves both.
fn claims_from(body: &Json) -> Option<Claims> {
    let subject = body.get("sub").and_then(Json::as_str)?.to_string();
    if subject.is_empty() {
        return None;
    }
    Some(Claims {
        subject,
        client: body.get("client_id").and_then(Json::as_str).map(str::to_string),
        scopes: Scopes::parse(body.get("scope").and_then(Json::as_str).unwrap_or("")),
        expires_at: body.get("exp").and_then(Json::as_f64).map(|at| at as i64),
    })
}

fn decode_part(part: &str) -> Option<String> {
    String::from_utf8(base64::decode(part)?).ok()
}

/// How long an introspection answer may be reused.
///
/// The whole cost of introspection is the hop, and the whole cost of caching it
/// is that a revoked token keeps working for this long. Thirty seconds is a
/// deliberate middle: it removes almost every hop under load and bounds the
/// damage of a revocation to something a person can be told.
pub const INTROSPECTION_TTL: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{SigningKey, signature::Signer};

    fn signed_token(claims: &str, key: &SigningKey, algorithm: &str) -> String {
        let header = base64::encode_url(format!(r#"{{"alg":"{algorithm}","typ":"JWT"}}"#).as_bytes());
        let payload = base64::encode_url(claims.as_bytes());
        let signing_input = format!("{header}.{payload}");
        let signature: p256::ecdsa::Signature = key.sign(signing_input.as_bytes());
        format!("{signing_input}.{}", base64::encode_url(&signature.to_bytes()))
    }

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32].into()).expect("a valid scalar")
    }

    fn public(key: &SigningKey) -> Vec<u8> {
        key.verifying_key().to_encoded_point(false).as_bytes().to_vec()
    }

    #[test]
    fn a_signed_token_is_read_and_its_claims_come_back() {
        let key = key();
        let token = signed_token(
            r#"{"sub":"42","client_id":"checkout","scope":"orders.read orders.write","exp":2000}"#,
            &key,
            "ES256",
        );

        let claims = verify_signed(&token, &public(&key), 1000).expect("a good token");
        assert_eq!(claims.subject, "42");
        assert_eq!(claims.client.as_deref(), Some("checkout"));
        assert!(claims.scopes.contains("orders.read"));
        assert_eq!(claims.expires_at, Some(2000));
    }

    /// The oldest JWT bug there is. A token that names its own algorithm and is
    /// believed lets `none` verify against nothing — and lets `HS256` invite a
    /// server holding a *public* key to use it as an HMAC secret the attacker
    /// also has.
    #[test]
    fn a_token_naming_another_algorithm_is_refused() {
        let key = key();
        for algorithm in ["none", "HS256", "RS256", "es256"] {
            let token = signed_token(r#"{"sub":"42","exp":2000}"#, &key, algorithm);
            assert!(
                verify_signed(&token, &public(&key), 1000).is_err(),
                "a token claiming `alg: {algorithm}` was accepted"
            );
        }
    }

    #[test]
    fn a_tampered_payload_is_refused() {
        let key = key();
        let token = signed_token(r#"{"sub":"42","exp":2000}"#, &key, "ES256");
        let (head, rest) = token.split_once('.').unwrap();
        let (_, signature) = rest.split_once('.').unwrap();

        let forged = format!(
            "{head}.{}.{signature}",
            base64::encode_url(br#"{"sub":"1","exp":2000}"#)
        );
        assert!(verify_signed(&forged, &public(&key), 1000).is_err());
    }

    #[test]
    fn a_token_signed_by_somebody_else_is_refused() {
        let mine = key();
        let theirs = SigningKey::from_bytes(&[9u8; 32].into()).unwrap();
        let token = signed_token(r#"{"sub":"42","exp":2000}"#, &theirs, "ES256");
        assert!(verify_signed(&token, &public(&mine), 1000).is_err());
    }

    #[test]
    fn an_expired_signed_token_is_refused() {
        let key = key();
        let token = signed_token(r#"{"sub":"42","exp":1000}"#, &key, "ES256");
        assert!(verify_signed(&token, &public(&key), 1000).is_err(), "exp == now is expired");
        assert!(verify_signed(&token, &public(&key), 999).is_ok());
    }

    #[test]
    fn something_that_is_not_a_token_is_refused() {
        let key = key();
        for rubbish in ["", "a", "a.b", "a.b.c.d", "....", "not a token at all"] {
            assert!(verify_signed(rubbish, &public(&key), 1000).is_err(), "{rubbish:?}");
        }
    }

    // --- and the same ground, introspected -------------------------------
    //
    // Deliberately the same list. Two ways of answering one question, and the
    // rarely-used one is the one that is wrong without anybody noticing.

    #[test]
    fn an_active_introspection_response_is_read_and_its_claims_come_back() {
        let body = Json::parse(
            r#"{"active":true,"sub":"42","client_id":"checkout","scope":"orders.read orders.write","exp":2000}"#,
        )
        .unwrap();

        let claims = read_introspection(&body, 1000).expect("an active token");
        assert_eq!(claims.subject, "42");
        assert_eq!(claims.client.as_deref(), Some("checkout"));
        assert!(claims.scopes.contains("orders.write"));
    }

    /// `active: false` is the whole answer, and a response that does not say so
    /// is not one to read optimistically.
    #[test]
    fn an_inactive_or_silent_response_is_refused() {
        for body in [
            r#"{"active":false,"sub":"42"}"#,
            r#"{"sub":"42","exp":2000}"#,
            r#"{"active":"true","sub":"42"}"#,
            r#"{}"#,
        ] {
            let parsed = Json::parse(body).unwrap();
            assert!(read_introspection(&parsed, 1000).is_err(), "{body}");
        }
    }

    #[test]
    fn an_expired_introspection_response_is_refused() {
        let body = Json::parse(r#"{"active":true,"sub":"42","exp":1000}"#).unwrap();
        assert!(read_introspection(&body, 1000).is_err(), "a cached answer outlives being true");
        assert!(read_introspection(&body, 999).is_ok());
    }

    #[test]
    fn a_response_with_no_subject_is_refused() {
        for body in [r#"{"active":true}"#, r#"{"active":true,"sub":""}"#] {
            let parsed = Json::parse(body).unwrap();
            assert!(read_introspection(&parsed, 1000).is_err(), "{body}");
        }
    }

    /// A token with no expiry is not an expired token — an introspection
    /// response may legitimately omit `exp`, and refusing those would refuse
    /// every token from a server that does not send it.
    #[test]
    fn a_token_without_an_expiry_is_not_treated_as_expired() {
        let body = Json::parse(r#"{"active":true,"sub":"42"}"#).unwrap();
        assert!(read_introspection(&body, i64::MAX).is_ok());
    }

    /// Whichever way the answer was reached, a resource server sees the same
    /// thing — which is what lets it be written once.
    #[test]
    fn both_paths_produce_the_same_claims() {
        let key = key();
        let token = signed_token(
            r#"{"sub":"42","client_id":"checkout","scope":"orders.read","exp":2000}"#,
            &key,
            "ES256",
        );
        let introspected = Json::parse(
            r#"{"active":true,"sub":"42","client_id":"checkout","scope":"orders.read","exp":2000}"#,
        )
        .unwrap();

        assert_eq!(
            verify_signed(&token, &public(&key), 1000).unwrap(),
            read_introspection(&introspected, 1000).unwrap()
        );
    }
}
