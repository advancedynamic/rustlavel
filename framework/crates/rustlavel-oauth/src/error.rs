//! The error vocabulary both halves of OAuth share.
//!
//! RFC 6749 fixes a small set of error codes and a JSON body to carry them.
//! Getting these right matters more than it looks: a client library decides
//! whether to retry, to re-authenticate, or to give up based on the code alone,
//! so returning `invalid_request` where the spec says `invalid_grant` sends
//! well-behaved clients into a retry loop.

use rustlavel_core::Json;
use rustlavel_http::{IntoResponse, Response, Status};

/// The error codes defined by RFC 6749 §4.1.2.1 (authorisation endpoint) and
/// §5.2 (token endpoint), plus the two RFC 6750 codes a resource server needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthErrorCode {
    // The token endpoint, RFC 6749 §5.2.
    InvalidRequest,
    InvalidClient,
    InvalidGrant,
    UnauthorizedClient,
    UnsupportedGrantType,
    InvalidScope,
    // The authorisation endpoint, RFC 6749 §4.1.2.1.
    AccessDenied,
    UnsupportedResponseType,
    ServerError,
    TemporarilyUnavailable,
    // A resource server rejecting a bearer token, RFC 6750 §3.1.
    InvalidToken,
    InsufficientScope,
}

impl OAuthErrorCode {
    /// The wire spelling. These strings are part of the protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            OAuthErrorCode::InvalidRequest => "invalid_request",
            OAuthErrorCode::InvalidClient => "invalid_client",
            OAuthErrorCode::InvalidGrant => "invalid_grant",
            OAuthErrorCode::UnauthorizedClient => "unauthorized_client",
            OAuthErrorCode::UnsupportedGrantType => "unsupported_grant_type",
            OAuthErrorCode::InvalidScope => "invalid_scope",
            OAuthErrorCode::AccessDenied => "access_denied",
            OAuthErrorCode::UnsupportedResponseType => "unsupported_response_type",
            OAuthErrorCode::ServerError => "server_error",
            OAuthErrorCode::TemporarilyUnavailable => "temporarily_unavailable",
            OAuthErrorCode::InvalidToken => "invalid_token",
            OAuthErrorCode::InsufficientScope => "insufficient_scope",
        }
    }

    pub fn parse(raw: &str) -> Option<OAuthErrorCode> {
        Some(match raw {
            "invalid_request" => OAuthErrorCode::InvalidRequest,
            "invalid_client" => OAuthErrorCode::InvalidClient,
            "invalid_grant" => OAuthErrorCode::InvalidGrant,
            "unauthorized_client" => OAuthErrorCode::UnauthorizedClient,
            "unsupported_grant_type" => OAuthErrorCode::UnsupportedGrantType,
            "invalid_scope" => OAuthErrorCode::InvalidScope,
            "access_denied" => OAuthErrorCode::AccessDenied,
            "unsupported_response_type" => OAuthErrorCode::UnsupportedResponseType,
            "server_error" => OAuthErrorCode::ServerError,
            "temporarily_unavailable" => OAuthErrorCode::TemporarilyUnavailable,
            "invalid_token" => OAuthErrorCode::InvalidToken,
            "insufficient_scope" => OAuthErrorCode::InsufficientScope,
            _ => return None,
        })
    }

    /// The HTTP status this code is returned with.
    ///
    /// RFC 6749 §5.2 is specific: everything is 400 except `invalid_client`,
    /// which is 401 so a client knows to fix its credentials rather than its
    /// request.
    pub fn status(self) -> Status {
        match self {
            OAuthErrorCode::InvalidClient | OAuthErrorCode::InvalidToken => Status::UNAUTHORIZED,
            OAuthErrorCode::InsufficientScope | OAuthErrorCode::AccessDenied => Status::FORBIDDEN,
            OAuthErrorCode::ServerError => Status::INTERNAL_ERROR,
            OAuthErrorCode::TemporarilyUnavailable => Status::SERVICE_UNAVAILABLE,
            _ => Status::BAD_REQUEST,
        }
    }
}

impl std::fmt::Display for OAuthErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An OAuth error: a code, and an optional description for a human.
#[derive(Debug, Clone)]
pub struct OAuthError {
    pub code: OAuthErrorCode,
    pub description: Option<String>,
    pub uri: Option<String>,
    /// Echoed back on a redirect, so the client can match the response to the
    /// request it made.
    pub state: Option<String>,
}

impl OAuthError {
    pub fn new(code: OAuthErrorCode) -> Self {
        OAuthError { code, description: None, uri: None, state: None }
    }

    /// The description is shown to developers, so say what is actually wrong.
    ///
    /// It must never quote the offending secret back — an error body ends up in
    /// logs, browser history and bug reports.
    pub fn because(code: OAuthErrorCode, description: impl Into<String>) -> Self {
        OAuthError { description: Some(description.into()), ..OAuthError::new(code) }
    }

    pub fn invalid_request(description: impl Into<String>) -> Self {
        OAuthError::because(OAuthErrorCode::InvalidRequest, description)
    }

    pub fn invalid_client(description: impl Into<String>) -> Self {
        OAuthError::because(OAuthErrorCode::InvalidClient, description)
    }

    pub fn invalid_grant(description: impl Into<String>) -> Self {
        OAuthError::because(OAuthErrorCode::InvalidGrant, description)
    }

    pub fn invalid_scope(description: impl Into<String>) -> Self {
        OAuthError::because(OAuthErrorCode::InvalidScope, description)
    }

    pub fn server_error(description: impl Into<String>) -> Self {
        OAuthError::because(OAuthErrorCode::ServerError, description)
    }

    pub fn with_state(mut self, state: Option<String>) -> Self {
        self.state = state;
        self
    }

    pub fn with_uri(mut self, uri: impl Into<String>) -> Self {
        self.uri = Some(uri.into());
        self
    }

    /// Read an error out of a provider's JSON response.
    ///
    /// Providers are not uniform here: some send a bare string, some an object,
    /// and some send 200 OK with an error body. The client checks for the field
    /// regardless of status, which is why this is lenient.
    pub fn from_json(body: &Json) -> Option<OAuthError> {
        let code = body.get("error")?;
        let code = match code {
            Json::String(text) => text.as_str(),
            // Google's older endpoints nest it: {"error": {"message": ...}}.
            Json::Object(_) => code.get("status").and_then(Json::as_str).unwrap_or("server_error"),
            _ => return None,
        };

        Some(OAuthError {
            code: OAuthErrorCode::parse(code).unwrap_or(OAuthErrorCode::ServerError),
            description: body
                .get("error_description")
                .and_then(Json::as_str)
                .or_else(|| body.get("error.message").and_then(Json::as_str))
                .map(str::to_string)
                // Keep the unrecognised code visible rather than swallowing it.
                .or_else(|| {
                    OAuthErrorCode::parse(code).is_none().then(|| format!("provider said: {code}"))
                }),
            uri: body.get("error_uri").and_then(Json::as_str).map(str::to_string),
            state: body.get("state").and_then(Json::as_str).map(str::to_string),
        })
    }

    pub fn to_json(&self) -> Json {
        let mut pairs = vec![("error".to_string(), Json::String(self.code.to_string()))];
        if let Some(description) = &self.description {
            pairs.push(("error_description".into(), Json::String(description.clone())));
        }
        if let Some(uri) = &self.uri {
            pairs.push(("error_uri".into(), Json::String(uri.clone())));
        }
        if let Some(state) = &self.state {
            pairs.push(("state".into(), Json::String(state.clone())));
        }
        Json::object(pairs)
    }

    /// The `error=...` query string appended to a redirect URI, per §4.1.2.1.
    pub fn to_query(&self) -> String {
        let mut query = format!("error={}", crate::url::encode(self.code.as_str()));
        if let Some(description) = &self.description {
            query.push_str(&format!("&error_description={}", crate::url::encode(description)));
        }
        if let Some(uri) = &self.uri {
            query.push_str(&format!("&error_uri={}", crate::url::encode(uri)));
        }
        if let Some(state) = &self.state {
            query.push_str(&format!("&state={}", crate::url::encode(state)));
        }
        query
    }
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.description {
            Some(description) => write!(f, "{}: {description}", self.code),
            None => write!(f, "{}", self.code),
        }
    }
}

impl std::error::Error for OAuthError {}

impl IntoResponse for OAuthError {
    fn into_response(self) -> Response {
        let response = Response::json(self.to_json()).with_status(self.code.status());

        // RFC 6750 §3: a 401 from a resource server has to say how to
        // authenticate, or the client cannot tell this apart from any other 401.
        match self.code {
            OAuthErrorCode::InvalidToken | OAuthErrorCode::InsufficientScope => {
                let mut challenge = format!(r#"Bearer error="{}""#, self.code);
                if let Some(description) = &self.description {
                    // Quoted-string: a stray quote would split the header.
                    let escaped = description.replace('\\', r"\\").replace('"', r#"\""#);
                    challenge.push_str(&format!(r#", error_description="{escaped}""#));
                }
                response.with_header("WWW-Authenticate", challenge)
            }
            OAuthErrorCode::InvalidClient => {
                response.with_header("WWW-Authenticate", r#"Basic realm="oauth""#)
            }
            _ => response,
        }
    }
}

impl From<OAuthError> for rustlavel_core::Error {
    fn from(error: OAuthError) -> rustlavel_core::Error {
        rustlavel_core::Error::msg(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_through_their_wire_spelling() {
        for code in [
            OAuthErrorCode::InvalidRequest,
            OAuthErrorCode::InvalidClient,
            OAuthErrorCode::InvalidGrant,
            OAuthErrorCode::UnauthorizedClient,
            OAuthErrorCode::UnsupportedGrantType,
            OAuthErrorCode::InvalidScope,
            OAuthErrorCode::AccessDenied,
            OAuthErrorCode::UnsupportedResponseType,
            OAuthErrorCode::ServerError,
            OAuthErrorCode::TemporarilyUnavailable,
            OAuthErrorCode::InvalidToken,
            OAuthErrorCode::InsufficientScope,
        ] {
            assert_eq!(OAuthErrorCode::parse(code.as_str()), Some(code));
        }
    }

    #[test]
    fn bad_client_credentials_are_a_401_not_a_400() {
        // The distinction RFC 6749 §5.2 draws: fix your credentials, not your
        // request. Clients branch on it.
        assert_eq!(OAuthErrorCode::InvalidClient.status(), Status::UNAUTHORIZED);
        assert_eq!(OAuthErrorCode::InvalidGrant.status(), Status::BAD_REQUEST);
    }

    #[test]
    fn a_rejected_bearer_token_says_how_to_authenticate() {
        let response = OAuthError::because(OAuthErrorCode::InvalidToken, "expired").into_response();

        assert_eq!(response.status, Status::UNAUTHORIZED);
        let challenge = response.headers.get("WWW-Authenticate").unwrap();
        assert!(challenge.contains(r#"error="invalid_token""#));
        assert!(challenge.contains(r#"error_description="expired""#));
    }

    #[test]
    fn a_quote_in_the_description_cannot_split_the_header() {
        let response =
            OAuthError::because(OAuthErrorCode::InvalidToken, r#"say "hi""#).into_response();

        let challenge = response.headers.get("WWW-Authenticate").unwrap();
        assert!(challenge.contains(r#"say \"hi\""#), "got {challenge}");
        assert_eq!(challenge.matches('"').count() - challenge.matches(r#"\""#).count(), 4);
    }

    #[test]
    fn parses_a_providers_error_body() {
        let body = Json::parse(
            r#"{"error":"invalid_grant","error_description":"code already used"}"#,
        )
        .unwrap();

        let error = OAuthError::from_json(&body).unwrap();
        assert_eq!(error.code, OAuthErrorCode::InvalidGrant);
        assert_eq!(error.description.as_deref(), Some("code already used"));
    }

    #[test]
    fn an_unrecognised_code_is_kept_in_the_description() {
        // Otherwise a provider-specific code vanishes and the developer is left
        // with a bare "server_error" and nothing to search for.
        let body = Json::parse(r#"{"error":"rate_limited"}"#).unwrap();

        let error = OAuthError::from_json(&body).unwrap();
        assert_eq!(error.code, OAuthErrorCode::ServerError);
        assert!(error.description.unwrap().contains("rate_limited"));
    }

    #[test]
    fn a_body_with_no_error_field_is_not_an_error() {
        let body = Json::parse(r#"{"access_token":"abc"}"#).unwrap();
        assert!(OAuthError::from_json(&body).is_none());
    }

    #[test]
    fn the_redirect_form_escapes_its_values() {
        let error = OAuthError::because(OAuthErrorCode::AccessDenied, "user said no")
            .with_state(Some("a b&c".into()));

        let query = error.to_query();
        assert!(query.contains("error=access_denied"));
        assert!(query.contains("error_description=user%20said%20no"));
        assert!(query.contains("state=a%20b%26c"), "got {query}");
    }
}
