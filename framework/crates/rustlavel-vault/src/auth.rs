//! Proving who you are to Vault.
//!
//! Every secret in this package is behind a token, and a token has to come from
//! somewhere. The methods here are the four an application actually uses: a
//! token handed in by an operator, an AppRole for a service, a Kubernetes
//! service account for a pod, and a username and password for a person.
//!
//! What they have in common is that the credential is worth more than anything
//! it unlocks, so none of these types print theirs. What they do not have in
//! common is the shape of the exchange — a token is already a token, and the
//! rest have to be traded for one — which is why [`Login`] describes the
//! request rather than performing it.

use crate::client::VaultClient;
use crate::error::VaultError;
use rustlavel_core::Result;
use crate::lease::Lease;
use rustlavel_core::Json;
use std::time::Duration;

/// The path a service account's token is mounted at inside a Kubernetes pod.
pub const KUBERNETES_TOKEN_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// How a login method asks for a token.
///
/// Two shapes, because Vault has two: most methods trade a credential at an
/// endpoint, and one — a token you were simply given — is already the answer
/// and only needs its lease looked up.
pub enum LoginRequest {
    /// `POST path` with `body`; the token comes back as `auth.client_token`.
    Endpoint { path: String, body: Json },
    /// The token is in hand. Its TTL and renewability still have to be read
    /// from the server, because nothing about the string says either.
    Existing { token: String },
}

/// A way of obtaining a Vault token.
///
/// Deliberately synchronous and object-safe: the fallible part of a login is
/// building the request — reading a service-account file that may not be
/// mounted — and doing that here means the failure names the file rather than
/// arriving later as a bare 400 from Vault.
pub trait Login {
    fn request(&self) -> Result<LoginRequest>;
}

impl VaultClient {
    /// Log in, keep the token, and return the lease it came with.
    ///
    /// The lease is the point: a service that logs in once at boot and never
    /// looks at the lease again will stop working somewhere between an hour and
    /// a month later, at a time nobody chose.
    pub async fn login(&self, method: &dyn Login) -> Result<Lease> {
        match method.request()? {
            LoginRequest::Endpoint { path, body } => {
                let response = self.post(&path, body).await?;
                let token = response.auth.get("client_token").and_then(Json::as_str).map(str::to_string).ok_or_else(|| {
                    VaultError::Malformed {
                        path: path.clone(),
                        message: "the login succeeded but carried no auth.client_token".into(),
                    }
                })?;
                self.set_token(token);
                Ok(response.lease)
            }
            LoginRequest::Existing { token } => {
                self.set_token(token);
                self.lookup_self().await
            }
        }
    }

    /// What the current token's lease is, according to the server.
    ///
    /// Its TTL lives in `data.ttl` rather than the `lease_duration` every other
    /// response uses, so this cannot go through `VaultResponse::lease`.
    pub async fn lookup_self(&self) -> Result<Lease> {
        let response = self.get("auth/token/lookup-self").await?;
        let data = &response.data;

        let seconds = data.get("ttl").and_then(Json::as_i64).unwrap_or(0).max(0);
        let renewable = data.get("renewable").and_then(Json::as_bool).unwrap_or(false);

        Ok(Lease::new(String::new(), Duration::from_secs(seconds as u64), renewable))
    }
}

/// A token handed to the process, by an operator or by an orchestrator.
///
/// Still worth a login rather than a bare `set_token`: it costs one round trip
/// and tells the process whether the token expires in ten minutes or never,
/// which is the difference between a renewal loop and an outage.
pub struct Token {
    token: String,
}

impl Token {
    pub fn new(token: impl Into<String>) -> Token {
        Token { token: token.into() }
    }
}

impl Login for Token {
    fn request(&self) -> Result<LoginRequest> {
        Ok(LoginRequest::Existing { token: self.token.clone() })
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token").field("token", &"<redacted>").finish()
    }
}

/// The usual way a service authenticates: a role id and a secret id.
///
/// The two halves are meant to travel separately — the role id baked into the
/// deployment, the secret id delivered at start-up and often single-use — so
/// that neither on its own is enough.
pub struct AppRole {
    mount: String,
    role_id: String,
    secret_id: String,
}

impl AppRole {
    pub fn new(role_id: impl Into<String>, secret_id: impl Into<String>) -> AppRole {
        AppRole {
            mount: "auth/approle".into(),
            role_id: role_id.into(),
            secret_id: secret_id.into(),
        }
    }

    /// For a method enabled somewhere other than `auth/approle`.
    pub fn mount(mut self, mount: impl Into<String>) -> AppRole {
        self.mount = normalise(mount);
        self
    }
}

impl Login for AppRole {
    fn request(&self) -> Result<LoginRequest> {
        Ok(LoginRequest::Endpoint {
            path: format!("{}/login", self.mount),
            body: Json::object([
                ("role_id", Json::from(self.role_id.as_str())),
                ("secret_id", Json::from(self.secret_id.as_str())),
            ]),
        })
    }
}

/// The role id is an identifier, not a credential, and seeing it is how you
/// work out which role a pod is using. The secret id is the credential.
impl std::fmt::Debug for AppRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppRole")
            .field("mount", &self.mount)
            .field("role_id", &self.role_id)
            .field("secret_id", &"<redacted>")
            .finish()
    }
}

/// A pod authenticating as its own service account.
///
/// The JWT is read at login rather than at construction: the kubelet rotates
/// the projected token on disk, and a copy taken at start-up is stale by the
/// time a long-lived process needs to log in again.
pub struct Kubernetes {
    mount: String,
    role: String,
    token_path: String,
}

impl Kubernetes {
    pub fn new(role: impl Into<String>) -> Kubernetes {
        Kubernetes {
            mount: "auth/kubernetes".into(),
            role: role.into(),
            token_path: KUBERNETES_TOKEN_PATH.into(),
        }
    }

    pub fn mount(mut self, mount: impl Into<String>) -> Kubernetes {
        self.mount = normalise(mount);
        self
    }

    /// Where to read the service-account JWT.
    ///
    /// Overridable so a test does not need a cluster, and because a projected
    /// volume with an audience can be mounted anywhere.
    pub fn token_path(mut self, path: impl Into<String>) -> Kubernetes {
        self.token_path = path.into();
        self
    }
}

impl Login for Kubernetes {
    fn request(&self) -> Result<LoginRequest> {
        let jwt = std::fs::read_to_string(&self.token_path).map_err(|error| {
            VaultError::Malformed {
                path: self.token_path.clone(),
                message: format!(
                    "cannot read the service-account token: {error}. \
                     Outside a pod, point `token_path` at a JWT."
                ),
            }
        })?;

        Ok(LoginRequest::Endpoint {
            path: format!("{}/login", self.mount),
            body: Json::object([
                ("role", Json::from(self.role.as_str())),
                ("jwt", Json::from(jwt.trim())),
            ]),
        })
    }
}

/// The JWT is never held on this type, so there is nothing here to leak.
impl std::fmt::Debug for Kubernetes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kubernetes")
            .field("mount", &self.mount)
            .field("role", &self.role)
            .field("token_path", &self.token_path)
            .finish()
    }
}

/// A person's username and password.
///
/// Rarely right for a service — a password does not rotate itself and cannot be
/// scoped to one deployment — but it is what a developer has on a laptop.
pub struct UserPass {
    mount: String,
    username: String,
    password: String,
}

impl UserPass {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> UserPass {
        UserPass {
            mount: "auth/userpass".into(),
            username: username.into(),
            password: password.into(),
        }
    }

    pub fn mount(mut self, mount: impl Into<String>) -> UserPass {
        self.mount = normalise(mount);
        self
    }
}

impl Login for UserPass {
    fn request(&self) -> Result<LoginRequest> {
        Ok(LoginRequest::Endpoint {
            // The username is a path segment here, not a field — the one
            // endpoint in this file that is not addressed by `…/login` alone.
            path: format!("{}/login/{}", self.mount, self.username),
            body: Json::object([("password", Json::from(self.password.as_str()))]),
        })
    }
}

impl std::fmt::Debug for UserPass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserPass")
            .field("mount", &self.mount)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Accept `approle`, `auth/approle` and `auth/approle/` as the same mount.
///
/// Vault's own CLI takes the bare name and the UI shows the trailing slash, so
/// a caller copying either would otherwise post to `auth/auth/approle/login`
/// and get a 404 that says nothing about why.
fn normalise(mount: impl Into<String>) -> String {
    let mount = mount.into();
    let trimmed = mount.trim_matches('/');
    if trimmed.starts_with("auth/") { trimmed.to_string() } else { format!("auth/{trimmed}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_client::{Fake, FakeResponse};

    /// The body OpenBao 2.6.2 returns from `auth/approle/login`.
    fn login_response() -> FakeResponse {
        FakeResponse::json(
            Json::parse(
                r#"{"lease_id":"","renewable":false,"lease_duration":0,"data":null,
                    "auth":{"client_token":"s.K550TB79WPxslNsUjh51Y6z3",
                            "accessor":"ZaY7ukg1a53506PweIujWibJ",
                            "policies":["default"],"lease_duration":3600,
                            "renewable":true,"token_type":"service"}}"#,
            )
            .unwrap(),
        )
    }

    fn vault(fake: Fake) -> VaultClient {
        VaultClient::new("https://vault.test:8200").faking(fake)
    }

    #[tokio::test]
    async fn an_approle_login_sets_the_token_and_returns_its_lease() {
        let client = vault(Fake::new().on("auth/approle/login", login_response()));

        let lease = client.login(&AppRole::new("role-uuid", "secret-uuid")).await.unwrap();

        assert_eq!(client.token().as_str(), ("s.K550TB79WPxslNsUjh51Y6z3"));
        assert_eq!(lease.duration(), Duration::from_secs(3600));
        assert!(lease.renewable());

        let fake = client.fake().unwrap();
        fake.assert_sent("vault.test:8200/v1/auth/approle/login");
        let sent = fake.recorded()[0].json().unwrap();
        assert_eq!(sent.get("role_id").unwrap().as_str(), Some("role-uuid"));
        assert_eq!(sent.get("secret_id").unwrap().as_str(), Some("secret-uuid"));
    }

    #[tokio::test]
    async fn an_approle_can_be_mounted_anywhere() {
        let client = vault(Fake::new().on("login", login_response()));

        // All three spellings an operator might copy out of the CLI or the UI.
        for mount in ["approle-prod", "auth/approle-prod", "/auth/approle-prod/"] {
            client.login(&AppRole::new("r", "s").mount(mount)).await.unwrap();
        }

        for request in client.fake().unwrap().recorded() {
            assert!(
                request.url.ends_with("/v1/auth/approle-prod/login"),
                "mount was not normalised: {}",
                request.url
            );
        }
    }

    #[tokio::test]
    async fn a_userpass_login_puts_the_username_in_the_path() {
        let client = vault(Fake::new().fallback(login_response()));

        client.login(&UserPass::new("ada", "hunter2")).await.unwrap();

        let sent = &client.fake().unwrap().recorded()[0];
        assert!(sent.url.ends_with("/v1/auth/userpass/login/ada"), "{}", sent.url);
        assert_eq!(sent.json().unwrap().get("password").unwrap().as_str(), Some("hunter2"));
        // The username is a path segment, so it must not also be a field.
        assert!(sent.json().unwrap().get("username").is_none());
    }

    #[tokio::test]
    async fn a_kubernetes_login_sends_the_service_account_jwt() {
        let path = std::env::temp_dir().join(format!("rustlavel-vault-k8s-jwt-sends-{}", std::process::id()));
        std::fs::write(&path, "eyJhbGciOi.pod.jwt\n").unwrap();

        let client = vault(Fake::new().fallback(login_response()));
        client
            .login(
                &Kubernetes::new("myapp").token_path(path.to_string_lossy().into_owned()),
            )
            .await
            .unwrap();

        let sent = &client.fake().unwrap().recorded()[0];
        assert!(sent.url.ends_with("/v1/auth/kubernetes/login"), "{}", sent.url);
        let body = sent.json().unwrap();
        assert_eq!(body.get("role").unwrap().as_str(), Some("myapp"));
        // The trailing newline the kubelet writes would be sent verbatim and
        // rejected by the API server as an invalid token.
        assert_eq!(body.get("jwt").unwrap().as_str(), Some("eyJhbGciOi.pod.jwt"));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_missing_service_account_file_says_what_to_do_about_it() {
        // `LoginRequest` holds the credential, so it has no `Debug` and cannot
        // be `unwrap_err`ed — which is the point.
        let request = Kubernetes::new("myapp")
            .token_path("/nowhere/rustlavel-vault/serviceaccount/token")
            .request();
        let Err(error) = request else { panic!("reading a missing JWT should fail") };
        let error = error.to_string();

        assert!(error.contains("service-account token"), "{error}");
        assert!(error.contains("token_path"), "{error}");
    }

    #[tokio::test]
    async fn a_token_login_still_asks_the_server_how_long_it_has() {
        // The shape `auth/token/lookup-self` returns: the TTL is in `data.ttl`,
        // not the `lease_duration` every other endpoint uses, and reading the
        // wrong one hands back a lease that claims to be eternal.
        let lookup = FakeResponse::json(
            Json::parse(
                r#"{"lease_id":"","renewable":false,"lease_duration":0,
                    "data":{"id":"s.given","ttl":3600,"renewable":true,
                            "policies":["default"],"creation_ttl":3600},
                    "auth":null}"#,
            )
            .unwrap(),
        );
        let client = vault(Fake::new().on("auth/token/lookup-self", lookup));

        let lease = client.login(&Token::new("s.given")).await.unwrap();

        assert_eq!(client.token().as_str(), ("s.given"));
        assert_eq!(lease.duration(), Duration::from_secs(3600));
        assert!(lease.renewable());
        assert!(!lease.never_expires());
        // The lookup has to be authenticated by the token it is asking about.
        let sent = &client.fake().unwrap().recorded()[0];
        assert_eq!(sent.headers.get("x-vault-token"), Some("s.given"));
    }

    #[tokio::test]
    async fn a_root_token_reads_back_as_one_that_never_expires() {
        // OpenBao reports a root token as ttl 0 and renewable false; a renewal
        // loop that cannot tell that from an expired lease will spin.
        let lookup = FakeResponse::json(
            Json::parse(
                r#"{"data":{"id":"root-token","ttl":0,"renewable":false,"policies":["root"]}}"#,
            )
            .unwrap(),
        );
        let client = vault(Fake::new().fallback(lookup));

        let lease = client.login(&Token::new("root-token")).await.unwrap();

        assert!(lease.never_expires());
        assert!(!lease.should_renew());
        assert!(!lease.is_expired());
    }

    #[tokio::test]
    async fn a_rejected_login_keeps_vaults_own_explanation() {
        let refused =
            FakeResponse::text(r#"{"errors":["invalid role or secret ID"]}"#).status(400);
        let client = vault(Fake::new().fallback(refused));

        let error = client.login(&AppRole::new("r", "wrong")).await.unwrap_err();

        assert!(error.to_string().contains("invalid role or secret ID"), "{error}");
        // A failed login must not leave a token behind.
        assert!(!client.has_token());
    }

    #[tokio::test]
    async fn a_login_that_returns_no_token_is_a_failure_not_a_silent_success() {
        let client = vault(Fake::new().fallback(FakeResponse::json(Json::object([
            ("auth", Json::Null),
        ]))));

        let error = client.login(&AppRole::new("r", "s")).await.unwrap_err().to_string();

        assert!(error.contains("auth.client_token"), "{error}");
    }

    #[test]
    fn no_login_method_prints_its_secret() {
        // These are exactly the values a developer prints while debugging a
        // failing login, and a secret id in a scrollback has to be revoked.
        assert!(!format!("{:?}", Token::new("s.tok")).contains("s.tok"));
        assert!(!format!("{:?}", AppRole::new("role", "sid")).contains("sid"));
        assert!(!format!("{:?}", UserPass::new("ada", "hunter2")).contains("hunter2"));

        // The role id is an identifier, not a credential, and is how you tell
        // which role a pod is using.
        assert!(format!("{:?}", AppRole::new("role-uuid", "sid")).contains("role-uuid"));
        assert!(format!("{:?}", UserPass::new("ada", "p")).contains("ada"));
    }
}
