//! Signed URLs: links that prove they came from this application.
//!
//! A password-reset mail, an invitation, a one-off download — each is a URL
//! that must work for whoever holds it and for nobody else, without a session
//! and without a row in a database. Appending an HMAC-SHA256 signature over the
//! path and query gives exactly that: the link is self-contained, unforgeable
//! without the application key, and can carry its own expiry.

use crate::key::AppKey;
use crate::{base64, constant_time_eq, unix_now};
use hmac::{Hmac, Mac};
use rustlavel_core::{Config, Result};
use rustlavel_http::response::{IntoResponse, Response};
use rustlavel_http::url;
use sha2::Sha256;
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

/// The query parameter carrying the signature.
pub const SIGNATURE_PARAM: &str = "signature";
/// The query parameter carrying the expiry, as a unix timestamp in seconds.
pub const EXPIRES_PARAM: &str = "expires";

/// Why a signed URL was not accepted.
///
/// Separate variants because the two failures deserve different words to the
/// visitor: an expired reset link should offer a new one, an invalid one should
/// not pretend anything is recoverable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureError {
    /// No `signature` parameter at all.
    Missing,
    /// The signature did not match — the URL was edited, or signed elsewhere.
    Invalid,
    /// The signature was good, but the link is past its `expires` timestamp.
    Expired,
}

impl SignatureError {
    pub fn message(self) -> &'static str {
        match self {
            SignatureError::Missing => "This link is missing its signature.",
            SignatureError::Invalid => "This link is not valid.",
            SignatureError::Expired => "This link has expired.",
        }
    }
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for SignatureError {}

/// Answers 403 — the request was understood and refused, and no amount of
/// authenticating will change that.
impl IntoResponse for SignatureError {
    fn into_response(self) -> Response {
        Response::new(rustlavel_http::Status::FORBIDDEN).with_html(format!("<h1>{}</h1>", self))
    }
}

/// Signs and verifies URLs with a key derived from `APP_KEY`.
#[derive(Clone)]
pub struct UrlSigner {
    key: [u8; 32],
}

impl UrlSigner {
    pub fn new(key: &AppKey) -> Self {
        UrlSigner { key: key.derive("signed-url") }
    }

    pub fn from_config(config: &Config) -> Result<Self> {
        Ok(UrlSigner::new(&AppKey::from_config(config)?))
    }

    /// Sign a URL, optionally giving it an expiry.
    ///
    /// `expires_at` is a unix timestamp in seconds; `None` signs a link that
    /// never expires, which is right for an unsubscribe URL and wrong for a
    /// password reset. Any `signature` or `expires` already on the URL is
    /// dropped first, so re-signing a link is idempotent rather than
    /// accumulating parameters.
    pub fn sign(&self, url: &str, expires_at: Option<u64>) -> String {
        let (origin, path, query) = split_url(url);

        let mut pairs: Vec<(String, String)> = url::parse_query(query)
            .into_iter()
            .filter(|(key, _)| key != SIGNATURE_PARAM && key != EXPIRES_PARAM)
            .collect();
        if let Some(expires) = expires_at {
            pairs.push((EXPIRES_PARAM.to_string(), expires.to_string()));
        }

        let signature = self.signature(&canonical(path, &pairs));
        pairs.push((SIGNATURE_PARAM.to_string(), signature));

        format!("{origin}{}", render(path, &pairs))
    }

    /// Sign a URL that expires `ttl` from now — the common case.
    pub fn sign_temporary(&self, url: &str, ttl: Duration) -> String {
        self.sign(url, Some(unix_now() + ttl.as_secs()))
    }

    /// Check a URL, usually `request.target()`.
    ///
    /// The signature is verified *before* the expiry is read. `expires` is an
    /// attacker-supplied number until the MAC says otherwise, so trusting it
    /// first would let anybody extend their own link.
    pub fn verify(&self, url: &str) -> std::result::Result<(), SignatureError> {
        let (_, path, query) = split_url(url);
        let pairs = url::parse_query(query);

        let signature = pairs
            .iter()
            .find(|(key, _)| key == SIGNATURE_PARAM)
            .map(|(_, value)| value.clone())
            .ok_or(SignatureError::Missing)?;

        let signed: Vec<(String, String)> =
            pairs.into_iter().filter(|(key, _)| key != SIGNATURE_PARAM).collect();

        let expected = self.signature(&canonical(path, &signed));
        if !constant_time_eq(expected.as_bytes(), signature.as_bytes()) {
            return Err(SignatureError::Invalid);
        }

        match signed.iter().find(|(key, _)| key == EXPIRES_PARAM) {
            None => Ok(()),
            Some((_, value)) => match value.parse::<u64>() {
                Ok(expires) if unix_now() <= expires => Ok(()),
                Ok(_) => Err(SignatureError::Expired),
                // Signed by us, yet unparseable: treat it as a forgery, never
                // as "no expiry".
                Err(_) => Err(SignatureError::Invalid),
            },
        }
    }

    /// Whether a URL verifies, for callers that only want a yes or no.
    pub fn is_valid(&self, url: &str) -> bool {
        self.verify(url).is_ok()
    }

    fn signature(&self, canonical: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(canonical.as_bytes());
        base64::encode_url(&mac.finalize().into_bytes())
    }
}

impl std::fmt::Debug for UrlSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UrlSigner { key: <redacted> }")
    }
}

/// Split a URL into everything before the path, the path, and the raw query.
///
/// Both `https://app.test/reset?x=1` and `/reset?x=1` are accepted, and both
/// sign identically: the host is deliberately outside the signature so a link
/// mailed with one hostname still verifies when the request arrives behind a
/// proxy, on a staging domain, or as a bare path in a test.
fn split_url(url: &str) -> (&str, &str, &str) {
    let (location, query) = match url.split_once('?') {
        Some((location, query)) => (location, query),
        None => (url, ""),
    };

    let path_start = match location.find("://") {
        Some(scheme_end) => location[scheme_end + 3..]
            .find('/')
            .map_or(location.len(), |offset| scheme_end + 3 + offset),
        None => 0,
    };

    (&location[..path_start], &location[path_start..], query)
}

/// The exact bytes that get signed.
///
/// Parameters are sorted by name, so a client that reorders the query string —
/// or a mail client that rewrites the link — does not break the signature,
/// while duplicate names keep their relative order and stay distinguishable.
fn canonical(path: &str, pairs: &[(String, String)]) -> String {
    let mut sorted: Vec<&(String, String)> = pairs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let path = if path.is_empty() { "/" } else { path };
    let query = sorted
        .iter()
        .map(|(key, value)| format!("{}={}", url::encode(key), url::encode(value)))
        .collect::<Vec<_>>()
        .join("&");

    if query.is_empty() { path.to_string() } else { format!("{path}?{query}") }
}

/// Render a path and its parameters back into a URL, in the given order.
fn render(path: &str, pairs: &[(String, String)]) -> String {
    let path = if path.is_empty() { "/" } else { path };
    if pairs.is_empty() {
        return path.to_string();
    }

    let query = pairs
        .iter()
        .map(|(key, value)| format!("{}={}", url::encode(key), url::encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{path}?{query}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> UrlSigner {
        UrlSigner::new(&AppKey::from_bytes([9u8; 32]))
    }

    #[test]
    fn a_signed_url_verifies() {
        let signer = signer();
        let signed = signer.sign("/downloads/42", None);

        assert!(signed.starts_with("/downloads/42?signature="));
        assert_eq!(signer.verify(&signed), Ok(()));
    }

    #[test]
    fn signing_preserves_the_existing_query_string() {
        let signer = signer();
        let signed = signer.sign("/invoices?year=2026&format=pdf", None);

        assert!(signed.contains("year=2026"));
        assert!(signed.contains("format=pdf"));
        assert!(signer.is_valid(&signed));
    }

    #[test]
    fn an_unsigned_url_is_missing_its_signature() {
        assert_eq!(signer().verify("/downloads/42"), Err(SignatureError::Missing));
        assert_eq!(signer().verify("/downloads/42?other=1"), Err(SignatureError::Missing));
    }

    #[test]
    fn a_modified_path_or_parameter_fails() {
        let signer = signer();
        let signed = signer.sign("/downloads/42?user=7", None);

        assert_eq!(signer.verify(&signed.replace("/42", "/43")), Err(SignatureError::Invalid));
        assert_eq!(signer.verify(&signed.replace("user=7", "user=8")), Err(SignatureError::Invalid));
        assert_eq!(
            signer.verify(&format!("{signed}&admin=1")),
            Err(SignatureError::Invalid),
            "an appended parameter must invalidate the signature"
        );
    }

    #[test]
    fn a_forged_signature_fails() {
        let signer = signer();
        let forged = format!("/downloads/42?signature={}", base64::encode_url(&[0u8; 32]));

        assert_eq!(signer.verify(&forged), Err(SignatureError::Invalid));
        assert_eq!(signer.verify("/downloads/42?signature="), Err(SignatureError::Invalid));
    }

    #[test]
    fn a_url_signed_with_another_key_fails() {
        let signed = signer().sign("/downloads/42", None);
        let other = UrlSigner::new(&AppKey::from_bytes([1u8; 32]));

        assert_eq!(other.verify(&signed), Err(SignatureError::Invalid));
    }

    #[test]
    fn an_expiry_in_the_future_is_accepted_and_one_in_the_past_is_not() {
        let signer = signer();

        let live = signer.sign("/reset/token", Some(unix_now() + 3_600));
        assert!(live.contains("expires="));
        assert_eq!(signer.verify(&live), Ok(()));

        let stale = signer.sign("/reset/token", Some(unix_now() - 1));
        assert_eq!(signer.verify(&stale), Err(SignatureError::Expired));
    }

    #[test]
    fn extending_an_expiry_by_hand_is_rejected() {
        let signer = signer();
        let expired_at = unix_now() - 1;
        let stale = signer.sign("/reset/token", Some(expired_at));

        let extended =
            stale.replace(&format!("expires={expired_at}"), &format!("expires={}", unix_now() + 999));

        // Not `Expired` — the signature no longer matches, which is the point.
        assert_eq!(signer.verify(&extended), Err(SignatureError::Invalid));
    }

    #[test]
    fn sign_temporary_expires_after_its_ttl() {
        let signer = signer();

        assert!(signer.is_valid(&signer.sign_temporary("/x", Duration::from_secs(60))));
        // A zero ttl expires at the current second, which still counts as live.
        assert!(signer.is_valid(&signer.sign_temporary("/x", Duration::ZERO)));
    }

    #[test]
    fn re_signing_replaces_rather_than_appends() {
        let signer = signer();
        let once = signer.sign("/x", Some(unix_now() + 60));
        let twice = signer.sign(&once, Some(unix_now() + 120));

        assert_eq!(twice.matches("signature=").count(), 1);
        assert_eq!(twice.matches("expires=").count(), 1);
        assert!(signer.is_valid(&twice));
    }

    #[test]
    fn absolute_and_relative_forms_of_the_same_url_verify_alike() {
        let signer = signer();
        let signed = signer.sign("https://app.test/reset/token", Some(unix_now() + 60));

        assert!(signed.starts_with("https://app.test/reset/token?"));

        let (_, path, query) = split_url(&signed);
        assert_eq!(signer.verify(&format!("{path}?{query}")), Ok(()));
    }

    #[test]
    fn reordering_the_query_string_does_not_break_the_signature() {
        let signer = signer();
        let signed = signer.sign("/x?b=2&a=1", None);
        let signature = signed.split("signature=").nth(1).unwrap();

        assert_eq!(signer.verify(&format!("/x?a=1&b=2&signature={signature}")), Ok(()));
    }

    #[test]
    fn parameters_with_reserved_characters_round_trip() {
        let signer = signer();
        let signed = signer.sign("/search?q=rust%20%26%20laravel", None);

        assert!(signer.is_valid(&signed), "signed url was {signed}");
    }

    #[test]
    fn splits_urls_with_and_without_an_origin() {
        assert_eq!(split_url("/a/b?x=1"), ("", "/a/b", "x=1"));
        assert_eq!(split_url("https://h.test/a?x=1"), ("https://h.test", "/a", "x=1"));
        assert_eq!(split_url("https://h.test"), ("https://h.test", "", ""));
        assert_eq!(split_url("/a"), ("", "/a", ""));
    }

    #[test]
    fn an_expiry_that_is_not_a_number_is_treated_as_a_forgery() {
        let signer = signer();
        // Sign a URL that legitimately carries a non-numeric `expires`, which
        // can only happen if the signature covers it.
        let pairs = vec![(EXPIRES_PARAM.to_string(), "soon".to_string())];
        let signature = signer.signature(&canonical("/x", &pairs));

        assert_eq!(
            signer.verify(&format!("/x?expires=soon&signature={signature}")),
            Err(SignatureError::Invalid)
        );
    }

    #[test]
    fn an_invalid_signature_renders_as_a_403() {
        let response = SignatureError::Expired.into_response();

        assert_eq!(response.status.code(), 403);
        assert!(response.body_string().contains("expired"));
    }
}
