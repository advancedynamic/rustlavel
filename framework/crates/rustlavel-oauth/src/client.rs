//! The client half: sending somebody to a provider, and trading what comes
//! back for a token.
//!
//! ```ignore
//! let google = OAuthClient::new(Provider::google())
//!     .credentials(config.string("services.google.id", ""), config.string("services.google.secret", ""))
//!     .redirect_uri("https://app.test/auth/google/callback");
//!
//! // Leg one: send them off, remembering the state.
//! let start = google.begin(&StateGuard::session(req.session()))?;
//! Response::see_other(start.url());
//!
//! // Leg two: they come back.
//! let token = google.callback(req.query_string(), &guard).await?;
//! let user = google.user(&token).await?;
//! ```

use crate::error::{OAuthError, OAuthErrorCode};
use crate::pkce::Pkce;
use crate::provider::{ClientAuth, Provider};
use crate::scope::Scopes;
use crate::state::StateGuard;
use crate::token::TokenResponse;
use crate::url;
use crate::user::SocialUser;
use rustlavel_auth::base64;
use rustlavel_client::{Client, ClientResponse};
use rustlavel_core::Json;

/// Where a visitor should be sent, and the two secrets that must survive until
/// they come back.
///
/// `Debug` redacts the verifier: the whole point of PKCE is that it is the one
/// value an interceptor of the redirect does not have.
pub struct Authorization {
    url: String,
    state: String,
    pkce: Pkce,
}

impl Authorization {
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub fn verifier(&self) -> &str {
        self.pkce.verifier()
    }

    pub fn pkce(&self) -> &Pkce {
        &self.pkce
    }

    pub fn into_url(self) -> String {
        self.url
    }
}

impl std::fmt::Debug for Authorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authorization")
            .field("url", &self.url)
            .field("state", &self.state)
            .field("verifier", &"<redacted>")
            .finish()
    }
}

/// An application's registration with one provider.
///
/// `Debug` redacts the secret. This type ends up in application state and in
/// the development error page, and a client secret in either is a credential
/// that has to be rotated.
#[derive(Clone)]
pub struct OAuthClient {
    provider: Provider,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    scopes: Option<Scopes>,
    params: Vec<(String, String)>,
    http: Client,
}

impl OAuthClient {
    pub fn new(provider: Provider) -> OAuthClient {
        OAuthClient {
            provider,
            client_id: String::new(),
            client_secret: String::new(),
            redirect_uri: String::new(),
            scopes: None,
            params: Vec::new(),
            http: Client::new(),
        }
    }

    pub fn credentials(
        mut self,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> OAuthClient {
        self.client_id = client_id.into();
        self.client_secret = client_secret.into();
        self
    }

    /// The URL the provider redirects back to.
    ///
    /// It must match what is registered with the provider byte for byte,
    /// including the scheme and any trailing slash — a mismatch is rejected at
    /// the token endpoint, after the visitor has already consented, which is
    /// why the error is so often reported as "it worked yesterday".
    pub fn redirect_uri(mut self, uri: impl Into<String>) -> OAuthClient {
        self.redirect_uri = uri.into();
        self
    }

    /// Ask for these scopes instead of the provider's defaults.
    pub fn scopes(mut self, scopes: impl Into<Scopes>) -> OAuthClient {
        self.scopes = Some(scopes.into());
        self
    }

    /// Add one scope to whatever is already being asked for.
    pub fn scope(mut self, scope: impl Into<String>) -> OAuthClient {
        let mut scopes = self.scopes.take().unwrap_or_else(|| self.provider.scopes.clone());
        scopes.add(scope);
        self.scopes = Some(scopes);
        self
    }

    /// A provider-specific authorisation parameter: `prompt`, `access_type`,
    /// `login_hint`, `hd`.
    ///
    /// Setting a name the provider preset already sets replaces it rather than
    /// sending it twice — two `prompt` values in one URL is a request most
    /// providers reject and none interpret the way the caller meant.
    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> OAuthClient {
        let name = name.into();
        self.params.retain(|(existing, _)| existing != &name);
        self.params.push((name, value.into()));
        self
    }

    /// Use a particular HTTP client — a shared one, or `Http::fake()` in tests.
    pub fn using(mut self, http: Client) -> OAuthClient {
        self.http = http;
        self
    }

    pub fn provider(&self) -> &Provider {
        &self.provider
    }

    pub fn http(&self) -> &Client {
        &self.http
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// What is actually being asked for: the application's scopes, or the
    /// provider's defaults when it did not say.
    pub fn requested_scopes(&self) -> Scopes {
        self.scopes.clone().unwrap_or_else(|| self.provider.scopes.clone())
    }

    /// Leg one: the URL to send a visitor to, with a fresh state and verifier.
    ///
    /// The caller has to keep both — see [`OAuthClient::begin`], which keeps
    /// them for you.
    pub fn authorize_url(&self) -> Authorization {
        let pkce = Pkce::generate();
        let state = base64::encode_url(&rustlavel_auth::random::bytes(16));
        let url = self.authorize_url_for(&state, &pkce);

        Authorization { url, state, pkce }
    }

    /// Leg one, with the state issued and remembered by a guard.
    pub fn begin(&self, guard: &StateGuard) -> Result<Authorization, OAuthError> {
        let pkce = Pkce::generate();
        let state = guard.issue(&pkce)?;
        let url = self.authorize_url_for(&state, &pkce);

        Ok(Authorization { url, state, pkce })
    }

    /// The authorisation URL for a state and verifier chosen elsewhere.
    ///
    /// PKCE is not a parameter here and there is no overload without it:
    /// OAuth 2.1 requires it for every authorisation code flow, and a client
    /// that could be talked into omitting it is a client that will be.
    pub fn authorize_url_for(&self, state: &str, pkce: &Pkce) -> String {
        let scopes = self.requested_scopes().to_string();
        let challenge = pkce.challenge();
        let extras = self.authorize_params();

        let mut pairs: Vec<(&str, &str)> = vec![
            ("response_type", "code"),
            ("client_id", &self.client_id),
            ("redirect_uri", &self.redirect_uri),
        ];
        if !scopes.is_empty() {
            pairs.push(("scope", &scopes));
        }
        pairs.push(("state", state));
        pairs.push(("code_challenge", &challenge));
        pairs.push(("code_challenge_method", pkce.method().as_str()));
        pairs.extend(extras.iter().map(|(name, value)| (name.as_str(), value.as_str())));

        url::append_query(&self.provider.authorize_url, &url::form_encode(&pairs))
    }

    /// The provider's parameters, with the application's overriding by name.
    fn authorize_params(&self) -> Vec<(String, String)> {
        let mut params = self.provider.authorize_params.clone();
        for (name, value) in &self.params {
            params.retain(|(existing, _)| existing != name);
            params.push((name.clone(), value.clone()));
        }
        params
    }

    /// Leg two: everything the callback has to do, in one call.
    ///
    /// `query` is the raw query string the provider redirected to — in a
    /// handler, `req.target()`'s query part.
    ///
    /// The state is checked *before* the provider's own `error` is read. An
    /// error response is still a response to a request somebody made, and
    /// answering an unsolicited one — even to say "access denied" — is a page
    /// an attacker can drive.
    pub async fn callback(
        &self,
        query: &str,
        guard: &StateGuard,
    ) -> Result<TokenResponse, OAuthError> {
        let fields = url::form_decode(query);
        let field = |name: &str| {
            fields.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
        };

        let pkce = guard.verify(field("state"))?;

        if let Some(code) = field("error") {
            return Err(OAuthError {
                code: OAuthErrorCode::parse(code).unwrap_or(OAuthErrorCode::ServerError),
                description: field("error_description")
                    .map(str::to_string)
                    .or_else(|| Some(format!("the provider said: {code}"))),
                uri: field("error_uri").map(str::to_string),
                state: None,
            });
        }

        let code = field("code").filter(|code| !code.is_empty()).ok_or_else(|| {
            OAuthError::invalid_request("the callback carried neither a `code` nor an `error`")
        })?;

        self.exchange(code, pkce.verifier()).await
    }

    /// Trade an authorisation code for a token.
    pub async fn exchange(
        &self,
        code: &str,
        verifier: &str,
    ) -> Result<TokenResponse, OAuthError> {
        self.token_request(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            // Sent again even though the provider already has it: RFC 6749
            // §4.1.3 makes it part of the grant, and the provider compares it
            // with what leg one asked for.
            ("redirect_uri", &self.redirect_uri),
            ("code_verifier", verifier),
        ])
        .await
    }

    /// Trade a refresh token for a new access token, RFC 6749 §6.
    ///
    /// The response may or may not carry a new refresh token. Providers
    /// disagree, and one that rotates them means the old one stops working the
    /// moment this returns — so store whatever comes back rather than assuming
    /// the one you had is still good.
    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse, OAuthError> {
        self.token_request(&[("grant_type", "refresh_token"), ("refresh_token", refresh_token)])
            .await
    }

    /// Revoke a token, RFC 7009.
    ///
    /// `hint` is `access_token` or `refresh_token`; it is advisory, and a
    /// provider that ignores it still has to search both. Revoking a refresh
    /// token usually revokes the access tokens issued from it — usually,
    /// because RFC 7009 §2.1 only recommends it.
    pub async fn revoke(&self, token: &str, hint: &str) -> Result<(), OAuthError> {
        let Some(endpoint) = self.provider.revoke_url.clone() else {
            return Err(OAuthError::server_error(format!(
                "`{}` has no revocation endpoint. Set one with `Provider::revoke(url)` if it \
                 gained one, or drop the token and let it expire.",
                self.provider.name
            )));
        };

        let mut form: Vec<(&str, &str)> = vec![("token", token)];
        if !hint.is_empty() {
            form.push(("token_type_hint", hint));
        }

        let response = self.post_form(&endpoint, form).await?;

        // RFC 7009 §2.2: a token that was already invalid is a *success*. Only
        // a real refusal — bad credentials, an unsupported token type — is an
        // error, and those carry an RFC 6749 body.
        if response.status.is_success() {
            return Ok(());
        }
        Err(self.error_from(&response))
    }

    /// Fetch the provider's profile for a token and normalise it.
    pub async fn user(&self, token: &TokenResponse) -> Result<SocialUser, OAuthError> {
        let Some(endpoint) = self.provider.userinfo_url.clone() else {
            return Err(OAuthError::server_error(format!(
                "`{}` has no userinfo endpoint. Set one with `Provider::userinfo(url)`.",
                self.provider.name
            )));
        };

        let mut request =
            self.http.get(&endpoint).header("authorization", token.authorization_header());
        for (name, value) in &self.provider.headers {
            request = request.header(name, value.clone());
        }

        let response = request.send().await.map_err(|error| {
            OAuthError::server_error(format!("could not reach {endpoint}: {error}"))
        })?;

        let body = decode_body(&response);
        if !response.status.is_success() {
            return Err(self.error_from(&response));
        }

        SocialUser::from_provider(&self.provider, &body).ok_or_else(|| {
            OAuthError::server_error(format!(
                "`{}` returned a profile with no `{}` field to identify it by",
                self.provider.name,
                self.provider.map.id.join("` or `")
            ))
        })
    }

    /// A request to the token endpoint, with the client's credentials attached
    /// the way this provider wants them.
    async fn token_request(&self, form: &[(&str, &str)]) -> Result<TokenResponse, OAuthError> {
        let url = self.provider.token_url.clone();
        let response = self.post_form(&url, form.to_vec()).await?;
        let body = decode_body(&response);

        if body.is_null() {
            let text = response.text();
            let excerpt: String = text.chars().take(200).collect();
            return Err(OAuthError::server_error(format!(
                "the token endpoint at {url} answered HTTP {} with a body that is neither JSON \
                 nor a form: {excerpt}",
                response.status
            )));
        }

        // Not gated on the status: GitHub reports `bad_verification_code` with
        // a 200, and `TokenResponse::from_json` reads `error` regardless.
        TokenResponse::from_json(&body)
    }

    async fn post_form(
        &self,
        url: &str,
        mut form: Vec<(&str, &str)>,
    ) -> Result<ClientResponse, OAuthError> {
        if self.provider.client_auth == ClientAuth::Body {
            form.push(("client_id", &self.client_id));
            form.push(("client_secret", &self.client_secret));
        }

        let mut request = self
            .http
            .post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(url::form_encode(&form));

        for (name, value) in &self.provider.headers {
            request = request.header(name, value.clone());
        }
        if self.provider.client_auth == ClientAuth::Basic {
            request = request.header("authorization", self.basic_credentials());
        }

        request
            .send()
            .await
            .map_err(|error| OAuthError::server_error(format!("could not reach {url}: {error}")))
    }

    /// `Basic base64(percent(id) ":" percent(secret))`, RFC 6749 §2.3.1.
    ///
    /// The percent-encoding is not optional and is the usual reason a secret
    /// "works in curl but not here": a secret containing `:` would otherwise
    /// split into a different id and secret entirely.
    fn basic_credentials(&self) -> String {
        let joined =
            format!("{}:{}", url::encode(&self.client_id), url::encode(&self.client_secret));
        format!("Basic {}", base64::encode(joined.as_bytes()))
    }

    /// Read a provider's refusal, falling back to the status when the body says
    /// nothing useful.
    fn error_from(&self, response: &ClientResponse) -> OAuthError {
        if let Some(error) = OAuthError::from_json(&decode_body(response)) {
            return error;
        }

        let text = response.text();
        let excerpt: String = text.chars().take(200).collect();
        OAuthError::because(
            match response.status.code() {
                401 => OAuthErrorCode::InvalidClient,
                403 => OAuthErrorCode::AccessDenied,
                503 => OAuthErrorCode::TemporarilyUnavailable,
                _ => OAuthErrorCode::ServerError,
            },
            format!("`{}` answered HTTP {}: {excerpt}", self.provider.name, response.status),
        )
    }
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("provider", &self.provider.name)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .field("scopes", &self.requested_scopes())
            .finish()
    }
}

/// A provider's response body as JSON, whatever it actually sent.
///
/// GitHub answers its token endpoint in `application/x-www-form-urlencoded`
/// unless asked for JSON, so a client that only parses JSON works right up
/// until somebody drops the `Accept` header. Form pairs are lifted into an
/// object of strings, which the rest of the crate reads identically —
/// `TokenResponse` already accepts a quoted `expires_in`.
///
/// `Json::Null` means neither shape parsed; the caller turns that into an error
/// that quotes the body, because a provider returning an HTML error page is a
/// real failure mode and "expected value at line 1" does not describe it.
fn decode_body(response: &ClientResponse) -> Json {
    let text = response.text();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Json::Null;
    }

    let content_type = response.headers.content_type().unwrap_or("");
    if content_type.contains("json") || trimmed.starts_with('{') {
        return Json::parse(trimmed).unwrap_or(Json::Null);
    }

    if content_type.contains("form-urlencoded") || (trimmed.contains('=') && !trimmed.contains(' '))
    {
        let pairs = url::form_decode(trimmed);
        if !pairs.is_empty() {
            return Json::object(pairs.into_iter().map(|(k, v)| (k, Json::String(v))));
        }
    }

    Json::Null
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkce;
    use rustlavel_client::fake::{Fake, FakeResponse};
    use rustlavel_http::Headers;

    fn client(provider: Provider, fake: Fake) -> OAuthClient {
        OAuthClient::new(provider)
            .credentials("client-123", "s3cr+t/val=ue")
            .redirect_uri("https://app.test/auth/callback")
            .using(Client::new().faking(fake))
    }

    /// The query string of an authorisation URL, as pairs.
    fn query_of(url: &str) -> Vec<(String, String)> {
        url::form_decode(url.split_once('?').expect("a query string").1)
    }

    fn value<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
        pairs.iter().find(|(key, _)| key == name).map(|(_, v)| v.as_str())
    }

    #[test]
    fn the_authorisation_url_carries_exactly_the_parameters_the_rfc_asks_for() {
        let client = OAuthClient::new(Provider::google())
            .credentials("client-123", "secret")
            .redirect_uri("https://app.test/auth/google/callback");
        let start = client.authorize_url();

        let pairs = query_of(start.url());
        let names: Vec<&str> = pairs.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "response_type",
                "client_id",
                "redirect_uri",
                "scope",
                "state",
                "code_challenge",
                "code_challenge_method",
            ]
        );

        assert!(start.url().starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert_eq!(value(&pairs, "response_type"), Some("code"));
        assert_eq!(value(&pairs, "client_id"), Some("client-123"));
        assert_eq!(value(&pairs, "redirect_uri"), Some("https://app.test/auth/google/callback"));
        assert_eq!(value(&pairs, "scope"), Some("email openid profile"));
        assert_eq!(value(&pairs, "state"), Some(start.state()));
    }

    #[test]
    fn the_redirect_uri_is_escaped_rather_than_pasted_into_the_url() {
        // Unescaped, the `?` and `&` of a redirect URI with its own query would
        // become parameters of the authorisation request.
        let client = OAuthClient::new(Provider::google())
            .credentials("id", "secret")
            .redirect_uri("https://app.test/cb?tenant=acme&next=/x");

        let url = client.authorize_url().into_url();
        assert!(url.contains("redirect_uri=https%3A%2F%2Fapp.test%2Fcb%3Ftenant%3Dacme%26next%3D%2Fx"));
        assert_eq!(url.matches("tenant").count(), 1);
    }

    #[test]
    fn the_authorisation_url_commits_to_a_pkce_challenge_and_never_the_verifier() {
        let start = OAuthClient::new(Provider::github())
            .credentials("id", "secret")
            .redirect_uri("https://app.test/cb")
            .authorize_url();

        let pairs = query_of(start.url());
        assert_eq!(value(&pairs, "code_challenge_method"), Some("S256"));
        assert_eq!(value(&pairs, "code_challenge"), Some(pkce::s256(start.verifier()).as_str()));
        assert!(
            !start.url().contains(start.verifier()),
            "the verifier reached the URL, which defeats PKCE entirely"
        );
    }

    #[test]
    fn extra_parameters_are_appended_and_replace_rather_than_repeat() {
        let url = OAuthClient::new(Provider::google())
            .credentials("id", "secret")
            .redirect_uri("https://app.test/cb")
            .with("access_type", "offline")
            .with("prompt", "select_account")
            .with("prompt", "consent")
            .authorize_url()
            .into_url();

        let pairs = query_of(&url);
        assert_eq!(value(&pairs, "access_type"), Some("offline"));
        assert_eq!(value(&pairs, "prompt"), Some("consent"));
        assert_eq!(pairs.iter().filter(|(name, _)| name == "prompt").count(), 1);
    }

    #[test]
    fn scopes_replace_or_extend_the_providers_defaults() {
        let base = OAuthClient::new(Provider::github()).credentials("id", "secret");

        assert_eq!(base.clone().requested_scopes().to_string(), "read:user user:email");
        assert_eq!(base.clone().scopes("repo").requested_scopes().to_string(), "repo");
        assert_eq!(
            base.scope("gist").requested_scopes().to_string(),
            "gist read:user user:email"
        );
    }

    #[test]
    fn two_authorisations_never_share_a_state_or_a_verifier() {
        let client = OAuthClient::new(Provider::google()).credentials("id", "secret");
        let (first, second) = (client.authorize_url(), client.authorize_url());

        assert_ne!(first.state(), second.state());
        assert_ne!(first.verifier(), second.verifier());
    }

    #[tokio::test]
    async fn a_successful_exchange_sends_the_grant_and_returns_the_token() {
        let client = client(
            Provider::google(),
            Fake::new().on(
                "oauth2.googleapis.com/token",
                FakeResponse::json(
                    Json::parse(
                        r#"{"access_token":"at-1","token_type":"Bearer","expires_in":3599,
                            "refresh_token":"rt-1","scope":"openid email"}"#,
                    )
                    .unwrap(),
                ),
            ),
        );

        let token = client.exchange("the-code", &"v".repeat(43)).await.unwrap();
        assert_eq!(token.access_token, "at-1");
        assert_eq!(token.expires_in, Some(3599));
        assert_eq!(token.refresh_token.as_deref(), Some("rt-1"));

        let sent = &client.http().fake().unwrap().recorded()[0];
        let form = url::form_decode(&sent.body_text());
        assert_eq!(value(&form, "grant_type"), Some("authorization_code"));
        assert_eq!(value(&form, "code"), Some("the-code"));
        assert_eq!(value(&form, "code_verifier"), Some("v".repeat(43).as_str()));
        assert_eq!(value(&form, "redirect_uri"), Some("https://app.test/auth/callback"));
        assert_eq!(sent.headers.get("content-type"), Some("application/x-www-form-urlencoded"));
    }

    #[tokio::test]
    async fn basic_credentials_are_percent_encoded_before_they_are_joined() {
        // The secret in the fixture contains `+`, `/` and `=`. Without the
        // encoding RFC 6749 §2.3.1 requires, a secret containing `:` would
        // split into a different id and secret and the provider would answer
        // `invalid_client`.
        let client = client(
            Provider::google(),
            Fake::new().fallback(FakeResponse::json(
                Json::parse(r#"{"access_token":"at"}"#).unwrap(),
            )),
        );
        client.exchange("code", &"v".repeat(43)).await.unwrap();

        let sent = &client.http().fake().unwrap().recorded()[0];
        let header = sent.headers.get("authorization").unwrap();
        let decoded = String::from_utf8(
            base64::decode(header.strip_prefix("Basic ").unwrap()).unwrap(),
        )
        .unwrap();

        assert_eq!(decoded, "client-123:s3cr%2Bt%2Fval%3Due");
        let form = url::form_decode(&sent.body_text());
        assert_eq!(value(&form, "client_secret"), None, "Basic means not in the body too");
    }

    #[tokio::test]
    async fn a_body_authenticating_provider_puts_the_credentials_in_the_form() {
        let client = client(
            Provider::github(),
            Fake::new().fallback(FakeResponse::json(
                Json::parse(r#"{"access_token":"at"}"#).unwrap(),
            )),
        );
        client.exchange("code", &"v".repeat(43)).await.unwrap();

        let sent = &client.http().fake().unwrap().recorded()[0];
        let form = url::form_decode(&sent.body_text());

        assert_eq!(value(&form, "client_id"), Some("client-123"));
        assert_eq!(value(&form, "client_secret"), Some("s3cr+t/val=ue"));
        assert_eq!(sent.headers.get("authorization"), None);
    }

    #[tokio::test]
    async fn github_is_asked_for_json_and_is_understood_anyway_when_it_sends_a_form() {
        // Both halves of GitHub's quirk. The preset sends the header; the
        // parser does not depend on it, because a provider that drops it must
        // not break the grant.
        let asked = client(
            Provider::github(),
            Fake::new().fallback(FakeResponse::json(
                Json::parse(r#"{"access_token":"at-json"}"#).unwrap(),
            )),
        );
        assert_eq!(asked.exchange("c", &"v".repeat(43)).await.unwrap().access_token, "at-json");
        assert_eq!(
            asked.http().fake().unwrap().recorded()[0].headers.get("accept"),
            Some("application/json")
        );

        let mut headers = Headers::new();
        headers.set("content-type", "application/x-www-form-urlencoded; charset=utf-8");
        let form_response = FakeResponse {
            status: rustlavel_http::Status::OK,
            headers,
            body: b"access_token=at-form&scope=read%3Auser&token_type=bearer".to_vec(),
        };

        let unasked = client(
            Provider::github().without_headers(),
            Fake::new().fallback(form_response),
        );
        let token = unasked.exchange("c", &"v".repeat(43)).await.unwrap();

        assert_eq!(token.access_token, "at-form");
        assert_eq!(token.token_type, "bearer");
        assert_eq!(token.scope.unwrap().to_string(), "read:user");
    }

    #[tokio::test]
    async fn an_error_reported_with_a_200_is_still_an_error() {
        // GitHub's actual behaviour. Trusting the status would hand the caller
        // a token response with no token in it.
        let client = client(
            Provider::github(),
            Fake::new().fallback(FakeResponse::json(
                Json::parse(
                    r#"{"error":"bad_verification_code",
                        "error_description":"The code passed is incorrect or expired."}"#,
                )
                .unwrap(),
            )),
        );

        let error = client.exchange("stale", &"v".repeat(43)).await.unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::ServerError, "an unrecognised code, kept visible");
        assert!(error.to_string().contains("incorrect or expired"), "{error}");
    }

    #[tokio::test]
    async fn a_400_with_an_rfc_error_body_keeps_its_code() {
        let client = client(
            Provider::google(),
            Fake::new().fallback(
                FakeResponse::json(
                    Json::parse(
                        r#"{"error":"invalid_grant","error_description":"Code was already redeemed."}"#,
                    )
                    .unwrap(),
                )
                .status(400),
            ),
        );

        let error = client.exchange("used", &"v".repeat(43)).await.unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::InvalidGrant);
        assert!(error.to_string().contains("already redeemed"));
    }

    #[tokio::test]
    async fn a_redirect_uri_mismatch_is_reported_with_the_providers_own_words() {
        // The single most common setup failure, and the reason the message is
        // passed through rather than replaced: `redirect_uri_mismatch` is not
        // an RFC 6749 code, so without this the developer sees `server_error`
        // and nothing to search for.
        let client = client(
            Provider::google(),
            Fake::new().fallback(
                FakeResponse::json(
                    Json::parse(r#"{"error":"redirect_uri_mismatch"}"#).unwrap(),
                )
                .status(400),
            ),
        );

        let error = client.exchange("code", &"v".repeat(43)).await.unwrap_err();
        assert!(error.to_string().contains("redirect_uri_mismatch"), "{error}");
    }

    #[tokio::test]
    async fn a_token_with_no_stated_expiry_is_accepted_and_never_looks_expired() {
        // GitHub's OAuth apps issue tokens that do not expire; treating a
        // missing `expires_in` as zero would refresh on every request.
        let client = client(
            Provider::github(),
            Fake::new().fallback(FakeResponse::json(
                Json::parse(r#"{"access_token":"at","token_type":"bearer","scope":""}"#).unwrap(),
            )),
        );

        let token = client.exchange("c", &"v".repeat(43)).await.unwrap();
        assert_eq!(token.expires_in, None);
        assert!(!token.expires_within(86_400, 60));
    }

    #[tokio::test]
    async fn an_html_error_page_is_quoted_rather_than_reported_as_a_parse_error() {
        let client = client(
            Provider::google(),
            Fake::new().fallback(FakeResponse::text("<html><body>502 Bad Gateway</body>").status(502)),
        );

        let error = client.exchange("c", &"v".repeat(43)).await.unwrap_err().to_string();
        assert!(error.contains("502"), "{error}");
        assert!(error.contains("Bad Gateway"), "{error}");
    }

    #[tokio::test]
    async fn refreshing_sends_the_refresh_grant_and_keeps_the_new_refresh_token() {
        let client = client(
            Provider::google(),
            Fake::new().fallback(FakeResponse::json(
                Json::parse(
                    r#"{"access_token":"at-2","expires_in":3599,"refresh_token":"rt-2"}"#,
                )
                .unwrap(),
            )),
        );

        let token = client.refresh("rt-1").await.unwrap();
        assert_eq!(token.access_token, "at-2");
        assert_eq!(token.refresh_token.as_deref(), Some("rt-2"), "rotated tokens must be kept");

        let form = url::form_decode(&client.http().fake().unwrap().recorded()[0].body_text());
        assert_eq!(value(&form, "grant_type"), Some("refresh_token"));
        assert_eq!(value(&form, "refresh_token"), Some("rt-1"));
        assert_eq!(value(&form, "code"), None);
    }

    #[tokio::test]
    async fn a_refused_refresh_says_the_grant_is_invalid_rather_than_returning_nothing() {
        let client = client(
            Provider::google(),
            Fake::new().fallback(
                FakeResponse::json(Json::parse(r#"{"error":"invalid_grant"}"#).unwrap())
                    .status(400),
            ),
        );

        assert_eq!(client.refresh("revoked").await.unwrap_err().code, OAuthErrorCode::InvalidGrant);
    }

    #[tokio::test]
    async fn revoking_posts_the_token_to_the_revocation_endpoint() {
        let client = client(
            Provider::google(),
            Fake::new().on("oauth2.googleapis.com/revoke", FakeResponse::text("")),
        );

        client.revoke("rt-1", "refresh_token").await.unwrap();

        let sent = &client.http().fake().unwrap().recorded()[0];
        let form = url::form_decode(&sent.body_text());
        assert_eq!(value(&form, "token"), Some("rt-1"));
        assert_eq!(value(&form, "token_type_hint"), Some("refresh_token"));
    }

    #[tokio::test]
    async fn revoking_a_token_that_was_already_dead_is_a_success() {
        // RFC 7009 §2.2 is explicit about this, and a caller that treated it as
        // a failure would retry a logout forever.
        let client =
            client(Provider::google(), Fake::new().fallback(FakeResponse::text("").status(200)));

        assert!(client.revoke("already-gone", "access_token").await.is_ok());
    }

    #[tokio::test]
    async fn a_refused_revocation_is_reported() {
        let client = client(
            Provider::google(),
            Fake::new().fallback(
                FakeResponse::json(Json::parse(r#"{"error":"invalid_client"}"#).unwrap())
                    .status(401),
            ),
        );

        assert_eq!(
            client.revoke("t", "access_token").await.unwrap_err().code,
            OAuthErrorCode::InvalidClient
        );
    }

    #[tokio::test]
    async fn revoking_at_a_provider_with_no_endpoint_says_so_instead_of_pretending() {
        let client = client(Provider::github(), Fake::new());

        let error = client.revoke("t", "access_token").await.unwrap_err().to_string();
        assert!(error.contains("no revocation endpoint"), "{error}");
        client.http().fake().unwrap().assert_count(0);
    }

    #[tokio::test]
    async fn the_userinfo_call_is_bearer_authenticated_and_mapped() {
        let client = client(
            Provider::github(),
            Fake::new().on(
                "api.github.com/user",
                FakeResponse::json(
                    Json::parse(
                        r#"{"id":1234,"login":"ada","name":null,
                            "avatar_url":"https://gh.test/a.png","email":null}"#,
                    )
                    .unwrap(),
                ),
            ),
        );

        let user = client.user(&TokenResponse::bearer("at-1")).await.unwrap();
        assert_eq!(user.qualified_id(), "github:1234");
        assert_eq!(user.name.as_deref(), Some("ada"));
        assert_eq!(user.email, None, "GitHub hides a private address");

        let sent = &client.http().fake().unwrap().recorded()[0];
        assert_eq!(sent.headers.get("authorization"), Some("Bearer at-1"));
        assert_eq!(sent.headers.get("accept"), Some("application/json"));
    }

    #[tokio::test]
    async fn every_preset_maps_its_own_profile_shape() {
        struct Case {
            provider: Provider,
            endpoint: &'static str,
            body: &'static str,
            id: &'static str,
            name: &'static str,
            email: Option<&'static str>,
            avatar: Option<&'static str>,
        }

        let cases = vec![
            Case {
                provider: Provider::google(),
                endpoint: "openidconnect.googleapis.com/v1/userinfo",
                body: r#"{"sub":"107","name":"Ada","email":"ada@x.test",
                         "picture":"https://g.test/a.png"}"#,
                id: "google:107",
                name: "Ada",
                email: Some("ada@x.test"),
                avatar: Some("https://g.test/a.png"),
            },
            Case {
                provider: Provider::gitlab(),
                endpoint: "gitlab.com/api/v4/user",
                body: r#"{"id":42,"username":"ada","name":"Ada","email":"ada@x.test",
                         "avatar_url":"https://gl.test/a.png"}"#,
                id: "gitlab:42",
                name: "Ada",
                email: Some("ada@x.test"),
                avatar: Some("https://gl.test/a.png"),
            },
            Case {
                provider: Provider::microsoft(),
                endpoint: "graph.microsoft.com/v1.0/me",
                body: r#"{"id":"aad-9","displayName":"Ada","mail":null,
                         "userPrincipalName":"ada@corp.test"}"#,
                id: "microsoft:aad-9",
                name: "Ada",
                email: Some("ada@corp.test"),
                avatar: None,
            },
            Case {
                provider: Provider::discord(),
                endpoint: "discord.com/api/users/@me",
                body: r#"{"id":"803","username":"nelly","global_name":"Nelly",
                         "avatar":"834","email":"n@x.test"}"#,
                id: "discord:803",
                name: "Nelly",
                email: Some("n@x.test"),
                avatar: Some("https://cdn.discordapp.com/avatars/803/834.png"),
            },
        ];

        for case in cases {
            let label = case.provider.name.clone();
            let client = client(
                case.provider,
                Fake::new()
                    .on(case.endpoint, FakeResponse::json(Json::parse(case.body).unwrap())),
            );

            let user = client.user(&TokenResponse::bearer("at")).await.unwrap();
            assert_eq!(user.qualified_id(), case.id, "{label}");
            assert_eq!(user.name.as_deref(), Some(case.name), "{label}");
            assert_eq!(user.email.as_deref(), case.email, "{label}");
            assert_eq!(user.avatar.as_deref(), case.avatar, "{label}");
        }
    }

    #[tokio::test]
    async fn a_userinfo_call_with_a_dead_token_is_reported_as_such() {
        let client = client(
            Provider::google(),
            Fake::new().fallback(
                FakeResponse::json(Json::parse(r#"{"error":"invalid_token"}"#).unwrap())
                    .status(401),
            ),
        );

        assert_eq!(
            client.user(&TokenResponse::bearer("stale")).await.unwrap_err().code,
            OAuthErrorCode::InvalidToken
        );
    }

    #[tokio::test]
    async fn a_profile_with_nothing_to_identify_it_by_is_an_error_not_an_empty_user() {
        let client = client(
            Provider::google(),
            Fake::new().fallback(FakeResponse::json(Json::parse(r#"{"name":"Ada"}"#).unwrap())),
        );

        let error = client.user(&TokenResponse::bearer("at")).await.unwrap_err().to_string();
        assert!(error.contains("sub"), "the message should name the field it looked for: {error}");
    }

    /// A client whose token endpoint always succeeds, and a session guard.
    fn round_trip() -> (OAuthClient, StateGuard) {
        let client = client(
            Provider::google(),
            Fake::new().fallback(FakeResponse::json(
                Json::parse(r#"{"access_token":"at-1","expires_in":3599}"#).unwrap(),
            )),
        );
        let guard = StateGuard::session(&rustlavel_auth::SessionHandle::new(
            rustlavel_auth::Session::new(),
        ));

        (client, guard)
    }

    #[tokio::test]
    async fn a_whole_flow_goes_out_with_a_state_and_comes_back_through_it() {
        let (client, guard) = round_trip();
        let start = client.begin(&guard).unwrap();

        let query = format!("code=the-code&state={}", url::encode(start.state()));
        let token = client.callback(&query, &guard).await.unwrap();

        assert_eq!(token.access_token, "at-1");
        // The verifier the callback used is the one leg one committed to.
        let form = url::form_decode(&client.http().fake().unwrap().recorded()[0].body_text());
        assert_eq!(value(&form, "code_verifier"), Some(start.verifier()));
    }

    #[tokio::test]
    async fn a_callback_that_accepts_any_state_is_login_csrf_so_this_one_does_not() {
        // The attack: the attacker completes leg one against their own account,
        // holds the resulting code, and gets the victim's browser to load the
        // callback with it. If the code alone were enough, the victim is now
        // signed in as the attacker and everything they do next lands in the
        // attacker's account.
        let (client, guard) = round_trip();
        client.begin(&guard).unwrap();

        for query in [
            "code=attackers-code",
            "code=attackers-code&state=",
            "code=attackers-code&state=guessed",
        ] {
            let error = client.callback(query, &guard).await.unwrap_err();
            assert_eq!(error.code, OAuthErrorCode::InvalidRequest, "{query}");
            client.http().fake().unwrap().assert_count(0);
        }
    }

    #[tokio::test]
    async fn a_replayed_callback_is_refused_before_the_code_is_exchanged() {
        let (client, guard) = round_trip();
        let start = client.begin(&guard).unwrap();
        let query = format!("code=the-code&state={}", url::encode(start.state()));

        assert!(client.callback(&query, &guard).await.is_ok());
        assert!(client.callback(&query, &guard).await.is_err());
        client.http().fake().unwrap().assert_count(1);
    }

    #[tokio::test]
    async fn a_callback_from_another_visitors_flow_is_refused() {
        let (client, mine) = round_trip();
        let (_, theirs) = round_trip();

        let start = client.begin(&theirs).unwrap();
        let query = format!("code=c&state={}", url::encode(start.state()));

        assert!(client.callback(&query, &mine).await.is_err());
    }

    #[tokio::test]
    async fn a_visitor_who_declined_gets_the_providers_reason_and_no_exchange() {
        let (client, guard) = round_trip();
        let start = client.begin(&guard).unwrap();

        let query = format!(
            "error=access_denied&error_description=The%20user%20denied&state={}",
            url::encode(start.state())
        );
        let error = client.callback(&query, &guard).await.unwrap_err();

        assert_eq!(error.code, OAuthErrorCode::AccessDenied);
        assert!(error.to_string().contains("denied"));
        client.http().fake().unwrap().assert_count(0);
    }

    #[tokio::test]
    async fn an_unsolicited_error_is_refused_by_the_state_check_first() {
        // Checking `error` before `state` would leave a page an attacker can
        // drive with a crafted link, which is a nuisance at best and a
        // phishing surface at worst.
        let (client, guard) = round_trip();

        let error = client.callback("error=access_denied", &guard).await.unwrap_err();
        assert_eq!(error.code, OAuthErrorCode::InvalidRequest, "the state, not the denial");
    }

    #[tokio::test]
    async fn a_callback_with_a_good_state_and_no_code_says_what_is_missing() {
        let (client, guard) = round_trip();
        let start = client.begin(&guard).unwrap();

        let query = format!("state={}", url::encode(start.state()));
        let error = client.callback(&query, &guard).await.unwrap_err();

        assert!(error.to_string().contains("neither a `code` nor an `error`"), "{error}");
    }

    #[tokio::test]
    async fn the_stateless_mode_completes_a_flow_with_nothing_stored() {
        let key = rustlavel_auth::AppKey::from_base64(&rustlavel_auth::AppKey::generate()).unwrap();
        let guard = StateGuard::sealed(&key);
        let (client, _) = round_trip();

        let start = client.begin(&guard).unwrap();
        let query = format!("code=c&state={}", url::encode(start.state()));

        assert!(client.callback(&query, &guard).await.is_ok());
        // A fresh guard on another machine, same application key: the state
        // still verifies, which is the whole point of the mode.
        assert!(client.callback(&query, &StateGuard::sealed(&key)).await.is_ok());
    }

    #[test]
    fn debug_prints_the_client_id_and_never_the_secret() {
        let client = OAuthClient::new(Provider::google()).credentials("public-id", "TOP-SECRET");
        let printed = format!("{client:?}");

        assert!(printed.contains("public-id"), "the id is not a secret and helps debugging");
        assert!(!printed.contains("TOP-SECRET"));
    }

    #[test]
    fn debug_of_an_authorisation_never_prints_the_verifier() {
        let start = OAuthClient::new(Provider::google()).credentials("id", "s").authorize_url();
        let printed = format!("{start:?}");

        assert!(!printed.contains(start.verifier()));
        assert!(printed.contains(start.state()), "the state is public and identifies the flow");
    }
}
