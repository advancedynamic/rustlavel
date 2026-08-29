//! The KV version 2 engine: static secrets, kept in versions.
//!
//! This is where a database username and password actually live, and the engine
//! has two pieces of awkwardness that every caller would otherwise have to
//! carry:
//!
//! 1. The secret is nested twice. `/v1/secret/data/myapp` answers
//!    `{"data": {"data": {…}, "metadata": {…}}}`, so the obvious `data` is the
//!    wrapper and the one below it is the secret.
//! 2. The mount is split. Values are read and written under `secret/data/…`,
//!    but listing, metadata and permanent deletion live under
//!    `secret/metadata/…`, and versioned deletes under `secret/delete/…`,
//!    `secret/undelete/…` and `secret/destroy/…`.
//!
//! Getting either wrong produces a working-looking call that returns the wrong
//! thing, which is why this module exists rather than a note in the docs.

use crate::client::VaultClient;
use crate::error::VaultError;
use rustlavel_core::{Error, Json, Result};

/// The KV v2 engine at one mount.
pub struct Kv<'a> {
    client: &'a VaultClient,
    mount: String,
}

impl VaultClient {
    /// The KV v2 engine at the default `secret/` mount.
    pub fn kv(&self) -> Kv<'_> {
        Kv { client: self, mount: "secret".into() }
    }
}

impl<'a> Kv<'a> {
    /// The same engine mounted elsewhere. `secret` is only the dev-server
    /// default; a real deployment usually mounts one per team.
    pub fn mount(mut self, mount: impl Into<String>) -> Kv<'a> {
        self.mount = mount.into().trim_matches('/').to_string();
        self
    }

    /// The latest version of a secret, or `None` if there is none.
    ///
    /// `None` covers two states Vault reports identically: nothing was ever
    /// written there, and the latest version was deleted. Neither is a failure.
    pub async fn read(&self, path: &str) -> Result<Option<Secret>> {
        self.read_at(&self.data_path(path)).await
    }

    /// One specific version, whatever has been written since.
    ///
    /// This is what pins a deployment to a known-good secret while somebody is
    /// mid-rotation, and what reads back the value a rollback needs.
    pub async fn read_version(&self, path: &str, version: u64) -> Result<Option<Secret>> {
        self.read_at(&format!("{}?version={version}", self.data_path(path))).await
    }

    /// The latest version, or an error naming the path.
    ///
    /// For the case where the secret is not optional — a service that cannot
    /// start without its database password should fail at boot saying which
    /// path was empty, not carry a `None` into the first query.
    pub async fn require(&self, path: &str) -> Result<Secret> {
        let missing = || VaultError::NotFound { path: self.data_path(path) };
        self.read(path).await?.ok_or_else(|| missing().into())
    }

    async fn read_at(&self, path: &str) -> Result<Option<Secret>> {
        let Some(response) = self.client.get_optional(path).await? else {
            return Ok(None);
        };

        // The inner `data`. A soft-deleted version answers 404 with the wrapper
        // still populated and this null, so it is already handled above — but a
        // KV v1 mount answers 200 with no nesting at all, and that is worth
        // saying out loud rather than returning an empty secret.
        let values = response.data.get("data").cloned().unwrap_or(Json::Null);
        if values.is_null() {
            return Err(VaultError::Malformed {
                path: path.to_string(),
                message: "the response has no nested `data`, which is what a KV version 1 mount \
                          looks like when it is read as version 2"
                    .to_string(),
            }
            .into());
        }

        let metadata = response.data.get("metadata").cloned().unwrap_or(Json::Null);
        Ok(Some(Secret {
            version: number(&metadata, "version"),
            created_time: text(&metadata, "created_time"),
            deletion_time: Some(text(&metadata, "deletion_time")).filter(|t| !t.is_empty()),
            destroyed: metadata.get("destroyed").and_then(Json::as_bool).unwrap_or(false),
            values,
        }))
    }

    /// Write a new version, replacing every value at the path.
    ///
    /// Replacing, not merging: a key left out of `values` is gone from the new
    /// version. [`Kv::patch`] is the one that merges.
    pub async fn write<K, V, I>(&self, path: &str, values: I) -> Result<VersionInfo>
    where
        K: Into<String>,
        V: Into<Json>,
        I: IntoIterator<Item = (K, V)>,
    {
        let response = self
            .client
            .post(&self.data_path(path), wrap(values))
            .await?;
        Ok(VersionInfo::from_json(&response.data))
    }

    /// Merge values into the newest version, leaving the rest in place.
    ///
    /// Still creates a version — nothing in KV v2 is edited in place — but does
    /// it without the read-modify-write race that doing it by hand would open.
    /// The path has to exist already; Vault answers 404 otherwise.
    pub async fn patch<K, V, I>(&self, path: &str, values: I) -> Result<VersionInfo>
    where
        K: Into<String>,
        V: Into<Json>,
        I: IntoIterator<Item = (K, V)>,
    {
        let response = self.client.patch(&self.data_path(path), wrap(values)).await?;
        Ok(VersionInfo::from_json(&response.data))
    }

    /// Soft-delete the newest version. Recoverable with [`Kv::undelete`].
    pub async fn delete(&self, path: &str) -> Result<()> {
        self.client.delete(&self.data_path(path)).await?;
        Ok(())
    }

    /// Soft-delete specific versions.
    pub async fn delete_versions(&self, path: &str, versions: &[u64]) -> Result<()> {
        self.client.post(&self.path("delete", path), versions_body(versions)).await.map(|_| ())
    }

    /// Bring soft-deleted versions back.
    ///
    /// The reason a delete is soft at all: a rotation that deletes the wrong
    /// path is a mistake somebody should be able to take back, and only
    /// [`Kv::destroy`] makes it permanent.
    pub async fn undelete(&self, path: &str, versions: &[u64]) -> Result<()> {
        self.client.post(&self.path("undelete", path), versions_body(versions)).await.map(|_| ())
    }

    /// Erase specific versions for good. Not recoverable.
    pub async fn destroy(&self, path: &str, versions: &[u64]) -> Result<()> {
        self.client.post(&self.path("destroy", path), versions_body(versions)).await.map(|_| ())
    }

    /// Erase the path, every version and its metadata. Not recoverable.
    pub async fn destroy_all(&self, path: &str) -> Result<()> {
        self.client.delete(&self.path("metadata", path)).await?;
        Ok(())
    }

    /// The keys directly under a prefix; pass `""` for the top of the mount.
    ///
    /// A key ending in `/` is a sub-prefix rather than a secret, which is
    /// Vault's only way of saying so.
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        // A prefix with nothing under it is a 404, and an empty list is the
        // honest answer: Vault has no empty folders, so "no keys" and "no such
        // prefix" are the same state.
        let Some(response) = self.client.get_optional(&format!(
            "{}?list=true",
            self.path("metadata", prefix)
        )).await? else {
            return Ok(Vec::new());
        };

        Ok(response
            .data
            .get("keys")
            .and_then(Json::as_array)
            .map(|keys| keys.iter().filter_map(Json::as_str).map(str::to_string).collect())
            .unwrap_or_default())
    }

    /// Everything Vault records about a path except the values: which versions
    /// exist, which are deleted, and how many are kept.
    pub async fn metadata(&self, path: &str) -> Result<Option<SecretMetadata>> {
        let Some(response) = self.client.get_optional(&self.path("metadata", path)).await? else {
            return Ok(None);
        };
        Ok(Some(SecretMetadata::from_json(&response.data)))
    }

    fn data_path(&self, path: &str) -> String {
        self.path("data", path)
    }

    /// KV v2 addresses the same secret through five prefixes depending on what
    /// is being done to it. This is the only place that knows which.
    fn path(&self, kind: &str, path: &str) -> String {
        let path = path.trim_matches('/');
        if path.is_empty() {
            format!("{}/{kind}", self.mount)
        } else {
            format!("{}/{kind}/{path}", self.mount)
        }
    }
}

/// One version of a secret: its values, and where it sits in the history.
///
/// No `Debug` derive. The values are the secret, and a struct like this is
/// exactly what ends up in a `dbg!` while somebody works out why a connection
/// failed.
pub struct Secret {
    values: Json,
    pub version: u64,
    pub created_time: String,
    /// Set when this version has been soft-deleted.
    pub deletion_time: Option<String>,
    pub destroyed: bool,
}

impl Secret {
    /// One field, as a string.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).and_then(Json::as_str)
    }

    /// One field, or an error naming the key.
    ///
    /// The failure this prevents is a service starting with an empty password
    /// because a key was renamed in Vault and nothing here noticed.
    pub fn require(&self, key: &str) -> Result<String> {
        self.get(key).map(str::to_string).ok_or_else(|| Error::from(VaultError::Malformed {
            path: format!("version {} of the secret", self.version),
            message: format!(
                "no `{key}` field. The secret holds: {}",
                if self.keys().is_empty() { "nothing".to_string() } else { self.keys().join(", ") }
            ),
        }))
    }

    /// Every value, for the callers that want more than strings.
    pub fn values(&self) -> &Json {
        &self.values
    }

    /// The field names, which are safe to log where the values are not.
    pub fn keys(&self) -> Vec<&str> {
        self.values
            .as_object()
            .map(|map| map.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret")
            .field("version", &self.version)
            .field("keys", &self.keys())
            .field("values", &"<redacted>")
            .field("deletion_time", &self.deletion_time)
            .field("destroyed", &self.destroyed)
            .finish()
    }
}

/// What a write answers with: which version it became.
#[derive(Debug, Clone)]
pub struct VersionInfo {
    pub version: u64,
    pub created_time: String,
    pub deletion_time: Option<String>,
    pub destroyed: bool,
}

impl VersionInfo {
    fn from_json(data: &Json) -> VersionInfo {
        VersionInfo {
            version: number(data, "version"),
            created_time: text(data, "created_time"),
            deletion_time: Some(text(data, "deletion_time")).filter(|t| !t.is_empty()),
            destroyed: data.get("destroyed").and_then(Json::as_bool).unwrap_or(false),
        }
    }
}

/// The history of a path.
#[derive(Debug, Clone)]
pub struct SecretMetadata {
    pub current_version: u64,
    pub oldest_version: u64,
    pub created_time: String,
    pub updated_time: String,
    /// How many versions the mount keeps; `0` means the mount's own default.
    pub max_versions: u64,
    /// Whether writes must carry the version they expect to replace.
    pub cas_required: bool,
    /// Every version still recorded, oldest first.
    pub versions: Vec<VersionInfo>,
}

impl SecretMetadata {
    fn from_json(data: &Json) -> SecretMetadata {
        let mut versions: Vec<VersionInfo> = data
            .get("versions")
            .and_then(Json::as_object)
            .map(|map| {
                map.iter()
                    .map(|(number, entry)| {
                        let mut info = VersionInfo::from_json(entry);
                        info.version = number.parse().unwrap_or(0);
                        info
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The keys are strings, so a plain map ordering puts "10" before "2".
        versions.sort_by_key(|version| version.version);

        SecretMetadata {
            current_version: number(data, "current_version"),
            oldest_version: number(data, "oldest_version"),
            created_time: text(data, "created_time"),
            updated_time: text(data, "updated_time"),
            max_versions: number(data, "max_versions"),
            cas_required: data.get("cas_required").and_then(Json::as_bool).unwrap_or(false),
            versions,
        }
    }
}

/// KV v2 wants the values under a `data` key, which is the other half of the
/// nesting that makes this engine confusing.
fn wrap<K, V, I>(values: I) -> Json
where
    K: Into<String>,
    V: Into<Json>,
    I: IntoIterator<Item = (K, V)>,
{
    let values = Json::object(values.into_iter().map(|(key, value)| (key.into(), value.into())));
    Json::object([("data", values)])
}

fn versions_body(versions: &[u64]) -> Json {
    Json::object([(
        "versions",
        Json::Array(versions.iter().map(|version| Json::Number(*version as f64)).collect()),
    )])
}

fn number(value: &Json, key: &str) -> u64 {
    value.get(key).and_then(Json::as_i64).unwrap_or(0).max(0) as u64
}

fn text(value: &Json, key: &str) -> String {
    value.get(key).and_then(Json::as_str).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_client::{Fake, FakeResponse};

    /// The exact body OpenBao 2.6.2 returns for `GET /v1/secret/data/myapp`.
    fn myapp() -> FakeResponse {
        FakeResponse::json(
            Json::parse(
                r#"{"request_id":"39f7ee3d","lease_id":"","renewable":false,"lease_duration":0,
                    "data":{"data":{"password":"s3cr3t","username":"app"},
                            "metadata":{"created_time":"2026-08-29T13:43:43Z",
                                        "custom_metadata":null,"deletion_time":"",
                                        "destroyed":false,"version":1}},
                    "wrap_info":null,"warnings":null,"auth":null}"#,
            )
            .unwrap(),
        )
    }

    fn vault(fake: Fake) -> VaultClient {
        VaultClient::new("https://vault.test:8200").faking(fake)
    }

    #[tokio::test]
    async fn reading_a_secret_returns_the_inner_data_not_the_wrapper() {
        // The bug this exists to prevent: `data` is the wrapper, and a caller
        // who stops there gets `{"data": …, "metadata": …}` and a password
        // field that is not there.
        let client = vault(Fake::new().on("secret/data/myapp", myapp()));

        let secret = client.kv().require("myapp").await.unwrap();

        assert_eq!(secret.get("username"), Some("app"));
        assert_eq!(secret.get("password"), Some("s3cr3t"));
        assert_eq!(secret.version, 1);
        assert!(secret.get("metadata").is_none());
        client.fake().unwrap().assert_sent("/v1/secret/data/myapp");
    }

    #[tokio::test]
    async fn a_missing_secret_is_none_and_a_required_one_names_the_path() {
        let client = vault(Fake::new().fallback(FakeResponse::text(r#"{"errors":[]}"#).status(404)));

        assert!(client.kv().read("nope").await.unwrap().is_none());

        // `require` turns the same absence into a failure that names the path,
        // which is the whole reason it exists beside `read`.
        let error = client.kv().require("nope").await.unwrap_err().to_string();
        assert!(error.contains("nothing is stored at"), "{error}");
        assert!(error.contains("secret/data/nope"), "{error}");
    }

    #[tokio::test]
    async fn a_version_can_be_pinned() {
        let client = vault(Fake::new().fallback(myapp()));

        client.kv().read_version("myapp", 2).await.unwrap();

        client.fake().unwrap().assert_sent("/v1/secret/data/myapp?version=2");
    }

    #[tokio::test]
    async fn writing_nests_the_values_under_data_and_reports_the_new_version() {
        // The body OpenBao answers a write with.
        let written = FakeResponse::json(
            Json::parse(
                r#"{"data":{"created_time":"2026-08-29T13:49:07Z","custom_metadata":null,
                            "deletion_time":"","destroyed":false,"version":3}}"#,
            )
            .unwrap(),
        );
        let client = vault(Fake::new().fallback(written));

        let info = client
            .kv()
            .write("myapp", [("username", "app"), ("password", "s3cr3t")])
            .await
            .unwrap();

        assert_eq!(info.version, 3);
        assert!(!info.destroyed);

        let sent = client.fake().unwrap().recorded()[0].json().unwrap();
        assert_eq!(sent.get("data.username").unwrap().as_str(), Some("app"));
        // Not at the top level: Vault would store `{"username": …}` as the
        // options object and write an empty secret.
        assert!(sent.get("username").is_none());
    }

    #[tokio::test]
    async fn patching_declares_the_merge_content_type() {
        let client = vault(Fake::new().fallback(FakeResponse::json(
            Json::parse(r#"{"data":{"version":4}}"#).unwrap(),
        )));

        client.kv().patch("myapp", [("password", "rotated")]).await.unwrap();

        let sent = &client.fake().unwrap().recorded()[0];
        assert_eq!(sent.headers.get("content-type"), Some("application/merge-patch+json"));
        assert!(sent.url.ends_with("/v1/secret/data/myapp"), "{}", sent.url);
    }

    #[tokio::test]
    async fn deleting_and_undeleting_use_the_prefixes_vault_reserves_for_them() {
        let client = vault(Fake::new().fallback(FakeResponse::text("").status(204)));
        let kv = client.kv();

        kv.delete("myapp").await.unwrap();
        kv.delete_versions("myapp", &[1, 2]).await.unwrap();
        kv.undelete("myapp", &[1]).await.unwrap();
        kv.destroy("myapp", &[2]).await.unwrap();
        kv.destroy_all("myapp").await.unwrap();

        let fake = client.fake().unwrap();
        let urls: Vec<String> = fake.recorded().iter().map(|r| r.url.clone()).collect();
        assert!(urls[0].ends_with("/v1/secret/data/myapp"), "{:?}", urls[0]);
        assert!(urls[1].ends_with("/v1/secret/delete/myapp"), "{:?}", urls[1]);
        assert!(urls[2].ends_with("/v1/secret/undelete/myapp"), "{:?}", urls[2]);
        assert!(urls[3].ends_with("/v1/secret/destroy/myapp"), "{:?}", urls[3]);
        assert!(urls[4].ends_with("/v1/secret/metadata/myapp"), "{:?}", urls[4]);

        let versions = fake.recorded()[1].json().unwrap();
        assert_eq!(versions.get("versions.0").unwrap().as_i64(), Some(1));
        assert_eq!(versions.get("versions.1").unwrap().as_i64(), Some(2));
    }

    #[tokio::test]
    async fn listing_goes_to_the_metadata_prefix_and_marks_sub_prefixes() {
        let listing = FakeResponse::json(
            Json::parse(r#"{"data":{"keys":["myapp","team/"]}}"#).unwrap(),
        );
        let client = vault(Fake::new().fallback(listing));

        let keys = client.kv().list("").await.unwrap();

        assert_eq!(keys, vec!["myapp".to_string(), "team/".to_string()]);
        client.fake().unwrap().assert_sent("/v1/secret/metadata?list=true");
    }

    #[tokio::test]
    async fn metadata_reports_the_versions_in_numeric_order() {
        // The keys are strings, so the naive ordering puts version 10 before 2.
        let body = FakeResponse::json(
            Json::parse(
                r#"{"data":{"cas_required":false,"created_time":"2026-08-29T13:49:07Z",
                            "current_version":10,"max_versions":0,"oldest_version":0,
                            "updated_time":"2026-08-29T13:50:00Z",
                            "versions":{"1":{"created_time":"a","deletion_time":"","destroyed":false},
                                        "2":{"created_time":"b","deletion_time":"c","destroyed":false},
                                        "10":{"created_time":"d","deletion_time":"","destroyed":true}}}}"#,
            )
            .unwrap(),
        );
        let client = vault(Fake::new().fallback(body));

        let metadata = client.kv().metadata("myapp").await.unwrap().unwrap();

        assert_eq!(metadata.current_version, 10);
        let numbers: Vec<u64> = metadata.versions.iter().map(|v| v.version).collect();
        assert_eq!(numbers, vec![1, 2, 10]);
        assert_eq!(metadata.versions[1].deletion_time.as_deref(), Some("c"));
        assert!(metadata.versions[2].destroyed);
    }

    #[tokio::test]
    async fn a_kv_version_one_mount_says_so_instead_of_returning_nothing() {
        // A v1 mount answers 200 with the values directly under `data`. Read as
        // v2 that is an empty secret, and the service starts with a blank
        // password rather than failing.
        let v1 = FakeResponse::json(Json::parse(r#"{"data":{"password":"s3cr3t"}}"#).unwrap());
        let client = vault(Fake::new().fallback(v1));

        let error = client.kv().read("myapp").await.unwrap_err().to_string();

        assert!(error.contains("version 1"), "{error}");
    }

    #[tokio::test]
    async fn the_mount_is_configurable() {
        let client = vault(Fake::new().fallback(myapp()));

        client.kv().mount("/team-a/").read("myapp").await.unwrap();

        client.fake().unwrap().assert_sent("/v1/team-a/data/myapp");
    }

    #[test]
    fn a_missing_field_names_the_key_and_lists_what_is_there() {
        let secret = Secret {
            values: Json::parse(r#"{"username":"app","password":"s3cr3t"}"#).unwrap(),
            version: 3,
            created_time: String::new(),
            deletion_time: None,
            destroyed: false,
        };

        let error = secret.require("dsn").unwrap_err().to_string();
        assert!(error.contains("`dsn`"), "{error}");
        assert!(error.contains("username"), "{error}");
        // Naming the keys is the point; naming the values would defeat it.
        assert!(!error.contains("s3cr3t"), "{error}");
    }

    #[test]
    fn debug_on_a_secret_shows_the_keys_and_not_the_values() {
        let secret = Secret {
            values: Json::parse(r#"{"password":"s3cr3t"}"#).unwrap(),
            version: 1,
            created_time: String::new(),
            deletion_time: None,
            destroyed: false,
        };

        let printed = format!("{secret:?}");
        assert!(printed.contains("password"), "{printed}");
        assert!(!printed.contains("s3cr3t"), "{printed}");
    }
}
