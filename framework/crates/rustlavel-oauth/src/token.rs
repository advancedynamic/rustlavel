//! The token endpoint's response, RFC 6749 §5.1.

use crate::error::OAuthError;
use crate::scope::Scopes;
use rustlavel_core::Json;

/// What a token endpoint returns on success.
///
/// `Debug` redacts the secrets. This type is what a developer prints while
/// debugging a grant, and an access token in a log file is a live credential.
#[derive(Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    /// Seconds from issue, not an absolute time — that is what the wire carries.
    pub expires_in: Option<u64>,
    pub refresh_token: Option<String>,
    /// Present when the provider granted something other than what was asked.
    pub scope: Option<Scopes>,
    /// OpenID Connect's identity token, when `openid` was requested.
    pub id_token: Option<String>,
    /// Everything else the provider sent, kept so provider-specific fields are
    /// not lost.
    pub extra: Json,
}

impl TokenResponse {
    pub fn bearer(access_token: impl Into<String>) -> TokenResponse {
        TokenResponse {
            access_token: access_token.into(),
            token_type: "Bearer".to_string(),
            expires_in: None,
            refresh_token: None,
            scope: None,
            id_token: None,
            extra: Json::Null,
        }
    }

    pub fn expiring_in(mut self, seconds: u64) -> TokenResponse {
        self.expires_in = Some(seconds);
        self
    }

    pub fn with_refresh_token(mut self, token: impl Into<String>) -> TokenResponse {
        self.refresh_token = Some(token.into());
        self
    }

    pub fn with_scopes(mut self, scopes: Scopes) -> TokenResponse {
        self.scope = Some(scopes);
        self
    }

    /// Parse a token endpoint's JSON response.
    ///
    /// An `error` field wins over everything else, whatever the HTTP status —
    /// enough providers return 200 with an error body that trusting the status
    /// alone means silently treating a failure as a success.
    pub fn from_json(body: &Json) -> Result<TokenResponse, OAuthError> {
        if let Some(error) = OAuthError::from_json(body) {
            return Err(error);
        }

        let access_token = body
            .get("access_token")
            .and_then(Json::as_str)
            .ok_or_else(|| {
                OAuthError::server_error(
                    "the token response had neither an `access_token` nor an `error` field",
                )
            })?
            .to_string();

        Ok(TokenResponse {
            access_token,
            // RFC 6749 §5.1 requires `token_type`, but providers omit it often
            // enough that refusing the grant over it helps nobody.
            token_type: body
                .get("token_type")
                .and_then(Json::as_str)
                .unwrap_or("Bearer")
                .to_string(),
            // Some providers send this as a JSON string rather than a number.
            expires_in: body.get("expires_in").and_then(|value| match value {
                Json::Number(seconds) if *seconds >= 0.0 => Some(*seconds as u64),
                Json::String(text) => text.parse().ok(),
                _ => None,
            }),
            refresh_token: body.get("refresh_token").and_then(Json::as_str).map(str::to_string),
            scope: body.get("scope").and_then(Json::as_str).map(Scopes::parse),
            id_token: body.get("id_token").and_then(Json::as_str).map(str::to_string),
            extra: body.clone(),
        })
    }

    pub fn to_json(&self) -> Json {
        let mut pairs = vec![
            ("access_token".to_string(), Json::String(self.access_token.clone())),
            ("token_type".to_string(), Json::String(self.token_type.clone())),
        ];
        if let Some(expires_in) = self.expires_in {
            pairs.push(("expires_in".into(), Json::Number(expires_in as f64)));
        }
        if let Some(refresh_token) = &self.refresh_token {
            pairs.push(("refresh_token".into(), Json::String(refresh_token.clone())));
        }
        if let Some(scope) = &self.scope {
            pairs.push(("scope".into(), Json::String(scope.to_string())));
        }
        if let Some(id_token) = &self.id_token {
            pairs.push(("id_token".into(), Json::String(id_token.clone())));
        }
        Json::object(pairs)
    }

    /// The value for an `Authorization` header.
    pub fn authorization_header(&self) -> String {
        format!("{} {}", self.token_type, self.access_token)
    }

    /// Whether the token is within `slack` seconds of expiring.
    ///
    /// Refreshing early is deliberate: a token that expires while a request is
    /// in flight fails, and the caller has no way to tell that apart from a
    /// revoked token.
    pub fn expires_within(&self, elapsed_seconds: u64, slack: u64) -> bool {
        match self.expires_in {
            Some(expires_in) => elapsed_seconds + slack >= expires_in,
            None => false,
        }
    }
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token", &"<redacted>")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "<redacted>"))
            .field("scope", &self.scope)
            .field("id_token", &self.id_token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_response() {
        let body = Json::parse(
            r#"{"access_token":"at","token_type":"Bearer","expires_in":3600,
                "refresh_token":"rt","scope":"read write"}"#,
        )
        .unwrap();

        let token = TokenResponse::from_json(&body).unwrap();
        assert_eq!(token.access_token, "at");
        assert_eq!(token.expires_in, Some(3600));
        assert_eq!(token.refresh_token.as_deref(), Some("rt"));
        assert_eq!(token.scope.unwrap().to_string(), "read write");
    }

    #[test]
    fn an_error_body_is_an_error_even_with_a_200() {
        // GitHub does exactly this. Trusting the status code would hand the
        // caller a TokenResponse with no token in it.
        let body = Json::parse(
            r#"{"error":"bad_verification_code","error_description":"expired"}"#,
        )
        .unwrap();

        let error = TokenResponse::from_json(&body).unwrap_err();
        assert!(error.to_string().contains("expired"));
    }

    #[test]
    fn a_response_with_no_token_and_no_error_says_so() {
        let error = TokenResponse::from_json(&Json::parse("{}").unwrap()).unwrap_err();
        assert!(error.to_string().contains("access_token"), "got {error}");
    }

    #[test]
    fn expires_in_is_accepted_as_a_string_too() {
        // Some providers quote it. Dropping it would make every token look
        // like it never expires.
        let body = Json::parse(r#"{"access_token":"at","expires_in":"3600"}"#).unwrap();
        assert_eq!(TokenResponse::from_json(&body).unwrap().expires_in, Some(3600));
    }

    #[test]
    fn a_nonsense_expiry_is_dropped_rather_than_wrapped() {
        // -1 as u64 is 18 quintillion seconds; a token that "never expires" is
        // worse than one with no stated expiry.
        let body = Json::parse(r#"{"access_token":"at","expires_in":-1}"#).unwrap();
        assert_eq!(TokenResponse::from_json(&body).unwrap().expires_in, None);
    }

    #[test]
    fn a_missing_token_type_defaults_to_bearer() {
        let body = Json::parse(r#"{"access_token":"at"}"#).unwrap();
        assert_eq!(TokenResponse::from_json(&body).unwrap().token_type, "Bearer");
    }

    #[test]
    fn provider_specific_fields_are_kept() {
        let body = Json::parse(r#"{"access_token":"at","team_id":"T1"}"#).unwrap();
        let token = TokenResponse::from_json(&body).unwrap();

        assert_eq!(token.extra.get("team_id").and_then(Json::as_str), Some("T1"));
    }

    #[test]
    fn round_trips_through_json() {
        let original = TokenResponse::bearer("at")
            .expiring_in(3600)
            .with_refresh_token("rt")
            .with_scopes(Scopes::of(["read"]));

        let parsed = TokenResponse::from_json(&original.to_json()).unwrap();
        assert_eq!(parsed.access_token, "at");
        assert_eq!(parsed.expires_in, Some(3600));
        assert_eq!(parsed.refresh_token.as_deref(), Some("rt"));
    }

    #[test]
    fn builds_an_authorization_header() {
        assert_eq!(TokenResponse::bearer("at").authorization_header(), "Bearer at");
    }

    #[test]
    fn refreshes_before_the_token_actually_dies() {
        let token = TokenResponse::bearer("at").expiring_in(3600);

        assert!(!token.expires_within(3000, 60));
        assert!(token.expires_within(3550, 60), "should refresh inside the slack window");
        assert!(!TokenResponse::bearer("at").expires_within(99_999, 60), "no stated expiry");
    }

    #[test]
    fn debug_prints_no_credentials() {
        let token = TokenResponse::bearer("secret-access").with_refresh_token("secret-refresh");
        let printed = format!("{token:?}");

        assert!(!printed.contains("secret-access"));
        assert!(!printed.contains("secret-refresh"));
    }
}
