//! Database accounts that expire on their own.
//!
//! This is the reason the package exists. Asking for credentials here makes
//! Vault run a `CREATE ROLE` against the database, hand back the username and
//! password it invented, and remember to run the matching `DROP` when the lease
//! ends. Nobody types the password, it is never written down, and a copy pulled
//! out of a leaked backup or an old log line is an account that no longer
//! exists.
//!
//! The cost is that the credential has a deadline, so a long-lived process has
//! to renew the lease — see [`crate::Lease::should_renew`] — or fetch new
//! credentials and reconnect.

use crate::client::VaultClient;
use crate::error::VaultError;
use rustlavel_core::Result;
use crate::lease::Lease;
use rustlavel_core::Json;

/// The database secrets engine at one mount.
pub struct DatabaseSecrets<'a> {
    client: &'a VaultClient,
    mount: String,
}

impl VaultClient {
    /// The database secrets engine at the default `database/` mount.
    pub fn database(&self) -> DatabaseSecrets<'_> {
        DatabaseSecrets { client: self, mount: "database".into() }
    }
}

impl<'a> DatabaseSecrets<'a> {
    /// The same engine mounted elsewhere.
    pub fn mount(mut self, mount: impl Into<String>) -> DatabaseSecrets<'a> {
        self.mount = mount.into().trim_matches('/').to_string();
        self
    }

    /// A new account for a role, created on the spot.
    ///
    /// Each call makes a *different* account. Calling it per query rather than
    /// per process will fill the database with roles and exhaust whatever
    /// connection limit it has, so hold the result for the life of the lease.
    pub async fn credentials(&self, role: &str) -> Result<DatabaseCredentials> {
        let path = format!("{}/creds/{role}", self.mount);
        let response = self.client.get(&path).await?;

        let missing = |field: &str| VaultError::Malformed {
            path: path.clone(),
            message: format!("the credentials carried no `{field}`"),
        };

        Ok(DatabaseCredentials {
            username: response.string("username").ok_or_else(|| missing("username"))?,
            password: response.string("password").ok_or_else(|| missing("password"))?,
            lease: response.lease,
        })
    }

    /// Change the password Vault itself uses to reach the database.
    ///
    /// Worth doing right after the engine is configured: until it runs, the
    /// root password is whatever an operator typed into a terminal, and
    /// afterwards it is a value only Vault has ever seen. It is one-way — there
    /// is no call that reads the new password back — so an operator who needs
    /// superuser access again has to reset it out of band.
    pub async fn rotate_root(&self, name: &str) -> Result<()> {
        self.client.post(&format!("{}/rotate-root/{name}", self.mount), Json::Null).await.map(|_| ())
    }

    /// The credentials for a *static* role: an account that already exists and
    /// whose password Vault rotates on a schedule.
    ///
    /// Unlike [`DatabaseSecrets::credentials`] this returns the same username
    /// every time, and the lease is only how long until the next rotation.
    pub async fn static_credentials(&self, role: &str) -> Result<DatabaseCredentials> {
        let path = format!("{}/static-creds/{role}", self.mount);
        let response = self.client.get(&path).await?;

        let missing = |field: &str| VaultError::Malformed {
            path: path.clone(),
            message: format!("the static credentials carried no `{field}`"),
        };

        // A static role reports `ttl` rather than a lease: there is nothing to
        // renew, because the account outlives any one reader.
        let ttl = response.json().get("data.ttl").and_then(Json::as_i64).unwrap_or(0).max(0);

        Ok(DatabaseCredentials {
            username: response.string("username").ok_or_else(|| missing("username"))?,
            password: response.string("password").ok_or_else(|| missing("password"))?,
            lease: Lease::new(String::new(), std::time::Duration::from_secs(ttl as u64), false),
        })
    }
}

/// A database account and the lease that keeps it alive.
pub struct DatabaseCredentials {
    username: String,
    password: String,
    lease: Lease,
}

impl DatabaseCredentials {
    /// The username Vault invented, of the form `v-approle-app-read-…`.
    ///
    /// Not secret, and the one value worth logging: it is how an operator
    /// matches a connection in `pg_stat_activity` back to the role and the
    /// lease that issued it.
    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn password(&self) -> &str {
        &self.password
    }

    /// When the account will be deleted, and whether the deadline can be moved.
    pub fn lease(&self) -> &Lease {
        &self.lease
    }
}

impl std::fmt::Debug for DatabaseCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseCredentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("lease", &self.lease)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_client::{Fake, FakeResponse};
    use std::time::Duration;

    /// The exact body OpenBao 2.6.2 returns from `GET /v1/database/creds/…`.
    fn issued() -> FakeResponse {
        FakeResponse::json(
            Json::parse(
                r#"{"request_id":"a9fcdc81",
                    "lease_id":"database/creds/app-readwrite/PTf7VNozTwLK2l8KYsstMdX6",
                    "renewable":true,"lease_duration":3600,
                    "data":{"password":"nW-5kDZyVj5YlYi6TOoN",
                            "username":"v-token-app-read-J6rJCj3Vvkkp17lejPor-1788011369"},
                    "wrap_info":null,"warnings":null,"auth":null}"#,
            )
            .unwrap(),
        )
    }

    fn vault(fake: Fake) -> VaultClient {
        VaultClient::new("https://vault.test:8200").faking(fake)
    }

    #[tokio::test]
    async fn issuing_credentials_returns_the_account_and_its_deadline() {
        let client = vault(Fake::new().on("database/creds/app-readwrite", issued()));

        let creds = client.database().credentials("app-readwrite").await.unwrap();

        assert!(creds.username().starts_with("v-token-app-read-"));
        assert_eq!(creds.password(), "nW-5kDZyVj5YlYi6TOoN");
        assert_eq!(creds.lease().duration(), Duration::from_secs(3600));
        assert!(creds.lease().renewable());
        // The lease id is what revokes the account, so losing it would leave a
        // credential alive until its TTL with no way to shorten it.
        assert_eq!(
            creds.lease().id(),
            "database/creds/app-readwrite/PTf7VNozTwLK2l8KYsstMdX6"
        );
    }

    #[tokio::test]
    async fn an_unknown_role_says_so_in_vaults_words() {
        let unknown = FakeResponse::text(r#"{"errors":["unknown role: nope"]}"#).status(400);
        let client = vault(Fake::new().fallback(unknown));

        let error = client.database().credentials("nope").await.unwrap_err();

        assert!(error.to_string().contains("unknown role: nope"), "{error}");
        // Observable rather than introspective: a bad request is not retried,
        // so exactly one went out.
        assert_eq!(client.fake().unwrap().count(), 1);
    }

    #[tokio::test]
    async fn rotating_the_root_password_answers_with_no_content() {
        let client = vault(Fake::new().fallback(FakeResponse::text("").status(204)));

        client.database().rotate_root("appdb").await.unwrap();

        client.fake().unwrap().assert_sent("/v1/database/rotate-root/appdb");
    }

    #[tokio::test]
    async fn a_static_role_has_a_ttl_rather_than_a_lease() {
        let body = FakeResponse::json(
            Json::parse(
                r#"{"lease_id":"","renewable":false,"lease_duration":0,
                    "data":{"last_vault_rotation":"2026-08-29T13:00:00Z",
                            "password":"rotated","rotation_period":86400,
                            "ttl":3600,"username":"app_static"}}"#,
            )
            .unwrap(),
        );
        let client = vault(Fake::new().fallback(body));

        let creds = client.database().static_credentials("app-static").await.unwrap();

        assert_eq!(creds.username(), "app_static");
        assert_eq!(creds.lease().duration(), Duration::from_secs(3600));
        // Nothing to renew: the account outlives any one reader, and Vault
        // rotates the password on its own schedule.
        assert!(!creds.lease().renewable());
        client.fake().unwrap().assert_sent("/v1/database/static-creds/app-static");
    }

    #[tokio::test]
    async fn the_mount_is_configurable() {
        let client = vault(Fake::new().fallback(issued()));

        client.database().mount("postgres-prod").credentials("app").await.unwrap();

        client.fake().unwrap().assert_sent("/v1/postgres-prod/creds/app");
    }

    #[tokio::test]
    async fn a_response_missing_the_password_is_a_failure_not_an_empty_string() {
        let half = FakeResponse::json(Json::parse(r#"{"data":{"username":"v-token-1"}}"#).unwrap());
        let client = vault(Fake::new().fallback(half));

        let error = client.database().credentials("app").await.unwrap_err().to_string();

        assert!(error.contains("`password`"), "{error}");
    }

    #[test]
    fn debug_prints_the_username_and_not_the_password() {
        let creds = DatabaseCredentials {
            username: "v-token-app-read-1".into(),
            password: "nW-5kDZyVj5YlYi6TOoN".into(),
            lease: Lease::none(),
        };

        let printed = format!("{creds:?}");
        assert!(printed.contains("v-token-app-read-1"), "{printed}");
        assert!(!printed.contains("nW-5kDZyVj5YlYi6TOoN"), "{printed}");
    }
}
