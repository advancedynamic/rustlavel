//! Reading request parameters, and refusing the ambiguous ones.
//!
//! RFC 6749 §3.1 and §3.2 both say request parameters "MUST NOT be included
//! more than once". That reads like tidiness and is not: if `?redirect_uri=A&
//! redirect_uri=B` is accepted, then everything depends on which copy each
//! layer reads. A proxy, a framework and a hand-written parser can each pick
//! differently, and an attacker who finds two that disagree gets a code
//! validated against A and delivered to B. So a repeated parameter is refused
//! here, once, before anything looks at a value.
//!
//! An empty value is treated as absent, because `redirect_uri=` is not a URI
//! and `code=` is not a code; reporting "missing" is the honest message.

use rustlavel_http::Request;
use rustlavel_oauth::OAuthError;

/// A snapshot of one request's parameters, from the query or the body.
pub struct Params(Vec<(String, String)>);

impl Params {
    /// The query string, for `GET /oauth/authorize`.
    pub fn from_query(request: &Request) -> Params {
        Params(request.query_pairs().to_vec())
    }

    /// The form body, for every `POST` endpoint.
    ///
    /// Deliberately *not* the query string as well: the token endpoint accepts
    /// credentials, and a `client_secret` in a URL ends up in access logs and
    /// browser history. If it is not in the body, it was not sent.
    pub fn from_body(request: &mut Request) -> Params {
        Params(request.form().to_vec())
    }

    /// One value, or `None` if it was absent or empty.
    pub fn get(&self, name: &str) -> Result<Option<&str>, OAuthError> {
        let mut found =
            self.0.iter().filter(|(key, _)| key == name).map(|(_, value)| value.as_str());
        let first = found.next();

        if found.next().is_some() {
            return Err(OAuthError::invalid_request(format!(
                "the `{name}` parameter was sent more than once. RFC 6749 §3.1 forbids that, \
                 because which copy each layer reads is exactly the ambiguity an attacker needs."
            )));
        }

        Ok(first.filter(|value| !value.is_empty()))
    }

    /// One value that has to be there.
    pub fn required(&self, name: &str) -> Result<&str, OAuthError> {
        self.get(name)?.ok_or_else(|| {
            OAuthError::invalid_request(format!("the `{name}` parameter is required"))
        })
    }

    pub fn has(&self, name: &str) -> bool {
        self.0.iter().any(|(key, _)| key == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_http::Method;

    fn query(target: &str) -> Params {
        Params::from_query(&Request::new(Method::Get, target))
    }

    #[test]
    fn reads_a_value_and_reports_a_missing_one() {
        let params = query("/authorize?client_id=web");

        assert_eq!(params.get("client_id").expect("read"), Some("web"));
        assert_eq!(params.get("state").expect("read"), None);

        let error = params.required("state").unwrap_err();
        assert!(error.to_string().contains("`state` parameter is required"));
    }

    #[test]
    fn an_empty_value_is_treated_as_absent() {
        let params = query("/authorize?redirect_uri=&code=");

        assert_eq!(params.get("redirect_uri").expect("read"), None);
        assert!(params.required("code").is_err());
        // But the parameter was still present on the wire, which the caller can
        // see when it matters.
        assert!(params.has("redirect_uri"));
    }

    #[test]
    fn a_repeated_parameter_is_refused_rather_than_resolved() {
        // The attack: one layer reads A, another reads B, and the code issued
        // for A is delivered to B.
        let params = query("/authorize?redirect_uri=https://a.test/cb&redirect_uri=https://evil");

        let error = params.get("redirect_uri").unwrap_err();
        assert_eq!(error.code, rustlavel_oauth::OAuthErrorCode::InvalidRequest);
        assert!(error.to_string().contains("more than once"));
        assert!(params.required("redirect_uri").is_err(), "and `required` refuses it too");
    }

    #[test]
    fn a_repeated_parameter_is_refused_even_when_both_copies_agree() {
        // No exception for the harmless-looking case: the rule is easier to
        // reason about than the exception, and a proxy may not have seen both.
        let params = query("/authorize?scope=read&scope=read");
        assert!(params.get("scope").is_err());
    }

    #[test]
    fn the_body_is_read_for_a_post_and_the_query_is_not() {
        // A `client_secret` in a URL is a secret in the access log. If it was
        // not in the body, it was not sent.
        let mut request = Request::new(Method::Post, "/token?client_secret=from-url")
            .with_form(&[("grant_type", "client_credentials")]);
        let params = Params::from_body(&mut request);

        assert_eq!(params.get("grant_type").expect("read"), Some("client_credentials"));
        assert_eq!(params.get("client_secret").expect("read"), None);
    }
}
