//! Against a real OpenBao server.
//!
//! The unit tests assert what this client *sends* and how it reads a body it
//! was handed. These assert that a server accepts those requests and answers
//! the way the fakes claim — a distinction this project has already paid for
//! once, when a full suite of green unit tests hid eight bugs that only
//! appeared against a live database.
//!
//! They run only when `VAULT_ADDR` (or `BAO_ADDR`) and a token are set, so
//! `cargo test` stays green on a machine with no server.
//!
//! ```text
//! # A dev-mode OpenBao. Everything is in memory and unsealed, which is fine
//! # for a test and unacceptable anywhere else.
//! docker run -d --name rustlavel-openbao -p 18200:8200 \
//!   -e BAO_DEV_ROOT_TOKEN_ID=root-token \
//!   -e BAO_DEV_LISTEN_ADDRESS=0.0.0.0:8200 \
//!   openbao/openbao:latest server -dev
//!
//! export VAULT_ADDR=http://127.0.0.1:18200
//! export VAULT_TOKEN=root-token
//! ```
//!
//! The dynamic-credentials tests need a database for OpenBao to create accounts
//! in, and the whole setup for one. The connection URL is written from
//! OpenBao's point of view, so it reaches the host rather than its own
//! container; on Linux add `--add-host=host.docker.internal:host-gateway` to
//! the OpenBao container.
//!
//! ```text
//! docker run -d --name rustlavel-vault-pg -p 55433:5432 \
//!   -e POSTGRES_PASSWORD=rootpass -e POSTGRES_DB=appdb postgres:16
//!
//! BAO="curl -s -H x-vault-token:root-token http://127.0.0.1:18200/v1"
//!
//! $BAO/sys/mounts/database -d '{"type":"database"}'
//!
//! $BAO/database/config/appdb -d '{
//!   "plugin_name": "postgresql-database-plugin",
//!   "allowed_roles": ["app-readwrite"],
//!   "connection_url": "postgresql://{{username}}:{{password}}@host.docker.internal:55433/appdb?sslmode=disable",
//!   "username": "postgres",
//!   "password": "rootpass" }'
//!
//! $BAO/database/roles/app-readwrite -d '{
//!   "db_name": "appdb",
//!   "default_ttl": "1h",
//!   "max_ttl": "24h",
//!   "creation_statements": ["CREATE ROLE \"{{name}}\" WITH LOGIN PASSWORD '"'"'{{password}}'"'"' VALID UNTIL '"'"'{{expiration}}'"'"'; GRANT ALL PRIVILEGES ON ALL TABLES IN SCHEMA public TO \"{{name}}\";"] }'
//!
//! export VAULT_TEST_DATABASE_ROLE=app-readwrite
//! export VAULT_TEST_DATABASE_HOST=127.0.0.1:55433/appdb
//! ```
//!
//! Tests run concurrently, so each one owns the paths, roles and users it
//! touches — named after itself — and cleans them up. Nothing here writes to a
//! shared path.

use rustlavel_core::Json;
use rustlavel_vault::{AppRole, Token, UserPass, VaultClient};
use std::time::Duration;

/// A client authenticated as the operator, or a skip.
macro_rules! vault {
    () => {
        match server() {
            Some(client) => client,
            None => {
                eprintln!("skipping: VAULT_ADDR/BAO_ADDR and VAULT_TOKEN/BAO_TOKEN are not set");
                return;
            }
        }
    };
}

/// The name of a configured database role, or a skip.
macro_rules! database_role {
    () => {
        match std::env::var("VAULT_TEST_DATABASE_ROLE") {
            Ok(role) if !role.trim().is_empty() => role,
            _ => {
                eprintln!("skipping: VAULT_TEST_DATABASE_ROLE is not set");
                return;
            }
        }
    };
}

/// `host:port/database`, as the *test process* reaches it, or a skip.
macro_rules! database_host {
    () => {
        match std::env::var("VAULT_TEST_DATABASE_HOST") {
            Ok(host) if !host.trim().is_empty() => host,
            _ => {
                eprintln!("skipping: VAULT_TEST_DATABASE_HOST is not set");
                return;
            }
        }
    };
}

fn server() -> Option<VaultClient> {
    let client = VaultClient::from_env().ok()?;
    client.has_token().then_some(client)
}

/// A second client for the same server, with no token yet.
fn anonymous(operator: &VaultClient) -> VaultClient {
    VaultClient::new(operator.address())
}

/// Enable an auth method, tolerating one that is already there.
///
/// Two of these tests want AppRole and neither should care whether the other,
/// or a previous run, got there first.
async fn enable_auth(operator: &VaultClient, kind: &str) {
    let body = Json::object([("type", Json::from(kind))]);
    if let Err(error) = operator.post(&format!("sys/auth/{kind}"), body).await {
        assert!(
            error.to_string().contains("already in use"),
            "could not enable the {kind} auth method: {error}"
        );
    }
}

#[tokio::test]
async fn an_approle_login_returns_a_token_that_can_read_a_secret() {
    let operator = vault!();
    let role = "rustlavel-test-approle-login";
    enable_auth(&operator, "approle").await;

    operator
        .post(
            &format!("auth/approle/role/{role}"),
            Json::object([("token_policies", Json::from("default")), ("token_ttl", "1h".into())]),
        )
        .await
        .unwrap();

    let role_id = operator
        .get(&format!("auth/approle/role/{role}/role-id"))
        .await
        .unwrap()
        .string("role_id")
        .unwrap();
    let secret_id = operator
        .post(&format!("auth/approle/role/{role}/secret-id"), Json::Null)
        .await
        .unwrap()
        .string("secret_id")
        .unwrap();

    // A fresh client with no token at all, which is the situation a service is
    // in when it starts.
    let service = anonymous(&operator);
    let lease = service.login(&AppRole::new(&role_id, &secret_id)).await.unwrap();

    assert!(service.has_token());
    assert!(lease.duration() >= Duration::from_secs(60), "{lease:?}");
    assert!(lease.renewable());

    // The token is not merely well-formed: it works.
    let whoami = service.lookup_self().await.unwrap();
    assert!(!whoami.never_expires(), "an AppRole token should not be eternal");

    // And a wrong secret id is refused rather than quietly issuing something.
    let refused = anonymous(&operator)
        .login(&AppRole::new(&role_id, "definitely-not-the-secret-id"))
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("invalid role or secret ID"), "{refused}");

    operator.delete(&format!("auth/approle/role/{role}")).await.unwrap();
}

#[tokio::test]
async fn a_userpass_login_puts_the_username_in_the_path() {
    let operator = vault!();
    let user = "rustlavel-test-userpass";
    enable_auth(&operator, "userpass").await;

    operator
        .post(
            &format!("auth/userpass/users/{user}"),
            Json::object([("password", Json::from("hunter2")), ("token_ttl", "1h".into())]),
        )
        .await
        .unwrap();

    let person = anonymous(&operator);
    let lease = person.login(&UserPass::new(user, "hunter2")).await.unwrap();

    assert!(person.has_token());
    assert!(lease.renewable());

    let refused =
        anonymous(&operator).login(&UserPass::new(user, "wrong")).await.unwrap_err();
    assert!(refused.to_string().contains("invalid username or password"), "{refused}");

    operator.delete(&format!("auth/userpass/users/{user}")).await.unwrap();
}

#[tokio::test]
async fn a_kv_secret_reads_back_as_the_inner_data() {
    let operator = vault!();
    let path = "rustlavel-test/inner-data";
    let kv = operator.kv();

    kv.write(path, [("username", "app"), ("password", "s3cr3t")]).await.unwrap();

    let secret = kv.require(path).await.unwrap();
    assert_eq!(secret.get("username"), Some("app"));
    assert_eq!(secret.get("password"), Some("s3cr3t"));
    assert_eq!(secret.require("password").unwrap(), "s3cr3t");
    assert_eq!(secret.version, 1);

    // The wrapper must not leak through: `data` and `metadata` are the
    // envelope, not fields of the secret.
    let mut keys = secret.keys();
    keys.sort_unstable();
    assert_eq!(keys, vec!["password", "username"]);

    kv.destroy_all(path).await.unwrap();
}

#[tokio::test]
async fn a_missing_secret_is_none_rather_than_an_error() {
    let operator = vault!();
    let kv = operator.kv();

    assert!(kv.read("rustlavel-test/never-written").await.unwrap().is_none());

    let error = kv.require("rustlavel-test/never-written").await.unwrap_err();
    assert!(error.to_string().contains("nothing is stored at"), "{error}");
}

#[tokio::test]
async fn a_pinned_version_ignores_everything_written_since() {
    let operator = vault!();
    let path = "rustlavel-test/pinned-version";
    let kv = operator.kv();

    let first = kv.write(path, [("password", "original")]).await.unwrap();
    let second = kv.write(path, [("password", "rotated")]).await.unwrap();
    assert_eq!(first.version, 1);
    assert_eq!(second.version, 2);

    assert_eq!(kv.read(path).await.unwrap().unwrap().get("password"), Some("rotated"));
    let pinned = kv.read_version(path, 1).await.unwrap().unwrap();
    assert_eq!(pinned.get("password"), Some("original"));
    assert_eq!(pinned.version, 1);

    // A merge keeps what it does not mention, unlike a write.
    let third = kv.patch(path, [("username", "app")]).await.unwrap();
    assert_eq!(third.version, 3);
    let merged = kv.read(path).await.unwrap().unwrap();
    assert_eq!(merged.get("password"), Some("rotated"));
    assert_eq!(merged.get("username"), Some("app"));

    kv.destroy_all(path).await.unwrap();
}

#[tokio::test]
async fn a_deleted_secret_comes_back_after_an_undelete() {
    let operator = vault!();
    let path = "rustlavel-test/delete-undelete";
    let kv = operator.kv();

    kv.write(path, [("password", "s3cr3t")]).await.unwrap();

    kv.delete(path).await.unwrap();
    // A soft-deleted version answers 404 with its metadata intact, which is
    // still "nothing there" as far as a caller is concerned.
    assert!(kv.read(path).await.unwrap().is_none());

    kv.undelete(path, &[1]).await.unwrap();
    assert_eq!(kv.read(path).await.unwrap().unwrap().get("password"), Some("s3cr3t"));

    // Destroying is the one that does not come back.
    kv.destroy(path, &[1]).await.unwrap();
    assert!(kv.read(path).await.unwrap().is_none());
    kv.undelete(path, &[1]).await.unwrap();
    assert!(kv.read(path).await.unwrap().is_none());

    let metadata = kv.metadata(path).await.unwrap().unwrap();
    assert_eq!(metadata.current_version, 1);
    assert!(metadata.versions[0].destroyed);

    kv.destroy_all(path).await.unwrap();
    assert!(kv.metadata(path).await.unwrap().is_none());
}

#[tokio::test]
async fn listing_names_the_secrets_and_marks_the_folders() {
    let operator = vault!();
    let prefix = "rustlavel-test-listing";
    let kv = operator.kv();

    kv.write(&format!("{prefix}/first"), [("k", "v")]).await.unwrap();
    kv.write(&format!("{prefix}/nested/second"), [("k", "v")]).await.unwrap();

    let keys = kv.list(prefix).await.unwrap();
    assert!(keys.contains(&"first".to_string()), "{keys:?}");
    // A trailing slash is Vault's only way of saying "this is a folder".
    assert!(keys.contains(&"nested/".to_string()), "{keys:?}");

    kv.destroy_all(&format!("{prefix}/first")).await.unwrap();
    kv.destroy_all(&format!("{prefix}/nested/second")).await.unwrap();

    // Vault has no empty folders, so the prefix is simply gone now — and that
    // is an empty listing rather than a failure.
    assert!(kv.list(prefix).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_tokens_ttl_and_renewability_are_read_from_the_server() {
    let operator = vault!();

    let made = operator
        .post(
            "auth/token/create",
            Json::object([
                ("ttl", Json::from("1h")),
                ("renewable", Json::Bool(true)),
                ("policies", Json::from(vec!["default"])),
            ]),
        )
        .await
        .unwrap();
    let issued = made.auth.get("client_token").and_then(Json::as_str).map(str::to_string).unwrap();

    // A token says nothing about itself; the TTL has to come from the server,
    // and it arrives in `data.ttl` rather than the `lease_duration` every other
    // endpoint uses.
    let service = anonymous(&operator);
    let lease = service.login(&Token::new(&issued)).await.unwrap();

    assert!(lease.renewable());
    assert!(!lease.never_expires());
    assert!(lease.duration() > Duration::from_secs(3000), "{lease:?}");
    assert!(!lease.should_renew());

    let renewed = service.renew_token(Some(Duration::from_secs(7200))).await.unwrap();
    assert!(renewed.duration() >= lease.duration(), "{renewed:?}");

    service.revoke_token().await.unwrap();
    assert!(!service.has_token());

    // The revoked token really is dead, and Vault says so with the same 403 it
    // uses for a policy denial.
    let dead = anonymous(&operator);
    dead.set_token(&issued);
    let error = dead.lookup_self().await.unwrap_err();
    assert!(error.to_string().contains("refused"), "{error}");
}

#[tokio::test]
async fn the_operators_root_token_reports_itself_as_eternal() {
    let operator = vault!();

    let lease = operator.lookup_self().await.unwrap();

    // A dev server's root token has ttl 0. The value of keeping that apart from
    // an expired lease is that a renewal loop given this does not spin.
    assert!(lease.never_expires());
    assert!(!lease.should_renew());
    assert!(!lease.is_expired());
}

#[tokio::test]
async fn dynamic_database_credentials_are_created_on_demand() {
    let operator = vault!();
    let role = database_role!();

    let first = operator.database().credentials(&role).await.unwrap();
    let second = operator.database().credentials(&role).await.unwrap();

    // Every call is a different account. This is the property that makes a
    // leaked credential worth less than a shared one — and the reason a caller
    // must hold the result rather than fetching per query.
    assert_ne!(first.username(), second.username());
    assert!(!first.password().is_empty());
    assert!(first.lease().renewable());
    assert!(first.lease().duration() > Duration::from_secs(60), "{:?}", first.lease());
    assert!(first.lease().id().starts_with(&format!("database/creds/{role}/")));

    // The lease is Vault's, not this client's opinion of it.
    let looked_up = operator.lookup_lease(first.lease().id()).await.unwrap();
    assert!(looked_up.renewable());

    let renewed =
        operator.renew_lease(first.lease(), Some(Duration::from_secs(600))).await.unwrap();
    assert!(renewed.duration() >= Duration::from_secs(600), "{renewed:?}");

    operator.revoke_lease(first.lease()).await.unwrap();
    operator.revoke_lease(second.lease()).await.unwrap();

    // Revoking twice is fine: Vault answers 204 for a lease it has forgotten,
    // so a shutdown path does not have to track what it already released.
    operator.revoke_lease(first.lease()).await.unwrap();

    let gone = operator.lookup_lease(first.lease().id()).await.unwrap_err();
    assert!(gone.to_string().contains("invalid lease"), "{gone}");
}

#[tokio::test]
async fn a_dynamic_credential_logs_in_and_stops_working_when_its_lease_is_revoked() {
    let operator = vault!();
    let role = database_role!();
    let host = database_host!();

    let creds = operator.database().credentials(&role).await.unwrap();
    let url = format!("postgres://{}:{}@{host}", creds.username(), creds.password());

    // The account exists, and it is the one Vault says it is.
    let database = rustlavel_db::Database::connect(&url).await.unwrap();
    let who: String = database
        .select_one("select current_user as who", &[])
        .await
        .unwrap()
        .expect("current_user returns a row")
        .get("who")
        .unwrap();
    assert_eq!(who, creds.username());
    drop(database);

    operator.revoke_lease(creds.lease()).await.unwrap();

    // This is the whole argument for the package: the credential is not merely
    // expired somewhere in Vault's bookkeeping, the database role has been
    // dropped. A copy of this username and password taken from a backup, a log
    // or a leaked environment file is now worth nothing.
    let error = match rustlavel_db::Database::connect(&url).await {
        Err(error) => error.to_string(),
        Ok(database) => match database.select_one("select 1 as one", &[]).await {
            Err(error) => error.to_string(),
            Ok(_) => panic!("a revoked credential still logged in to {host}"),
        },
    };
    assert!(
        error.contains("does not exist") || error.contains("authentication"),
        "the connection failed for the wrong reason: {error}"
    );
}
