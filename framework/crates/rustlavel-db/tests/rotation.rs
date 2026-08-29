//! Rotating a dynamic database credential without restarting the process.
//!
//! This is the test the design was written against, and it needs both a real
//! database and a real secret store, because the interesting question is what
//! *PostgreSQL* does — not what this driver believes it does.
//!
//! What it establishes, in order: a pool built on a credential the store issued
//! works; a second credential can replace the first while the pool is live; the
//! old connections are retired rather than reused; and revoking the first lease
//! afterwards does not disturb the process at all, because nothing is using it
//! any more. That last step is the whole point — it is the difference between a
//! credential that expires safely and one that takes production down with it.
//!
//! Runs only when `VAULT_ADDR` and `ROTATION_DB_URL` are both set.
//!
//! ```text
//! docker run -d --name rustlavel-vault-pg -e POSTGRES_PASSWORD=rootpass \
//!   -e POSTGRES_DB=appdb -p 55433:5432 postgres:16
//! docker run -d --name rustlavel-openbao -p 18200:8200 \
//!   -e BAO_DEV_ROOT_TOKEN_ID=root-token -e BAO_DEV_LISTEN_ADDRESS=0.0.0.0:8200 \
//!   openbao/openbao:latest server -dev
//!
//! V=http://127.0.0.1:18200/v1; H='X-Vault-Token: root-token'
//! curl -s -H "$H" -X POST -d '{"type":"database"}' $V/sys/mounts/database
//! curl -s -H "$H" -X POST -d '{
//!   "plugin_name":"postgresql-database-plugin","allowed_roles":"app",
//!   "connection_url":"postgresql://{{username}}:{{password}}@host.docker.internal:55433/appdb?sslmode=disable",
//!   "username":"postgres","password":"rootpass"}' $V/database/config/appdb
//! curl -s -H "$H" -X POST -d '{"db_name":"appdb","default_ttl":"1h","max_ttl":"24h",
//!   "creation_statements":"CREATE ROLE \"{{name}}\" WITH LOGIN PASSWORD '\''{{password}}'\'' VALID UNTIL '\''{{expiration}}'\''; GRANT USAGE ON SCHEMA public TO \"{{name}}\";"
//!   }' $V/database/roles/app
//!
//! export VAULT_ADDR=http://127.0.0.1:18200 VAULT_TOKEN=root-token
//! export ROTATION_DB_URL='postgres://127.0.0.1:55433/appdb?sslmode=disable'
//! ```

use rustlavel_db::credentials::Credentials;
use rustlavel_db::{DatabaseConfig, pool::Pool, postgres::PostgresDriver};
use std::sync::Arc;

/// One dynamic account, and the lease that will delete it.
struct Issued {
    username: String,
    password: String,
    lease: String,
}

/// Talk to the store with curl rather than taking a dependency on the vault
/// package: this crate must not depend on it, and a test that reached for it
/// would be quietly asserting a dependency the design forbids.
fn vault(method: &str, path: &str, body: &str) -> String {
    let mut command = std::process::Command::new("curl");
    command
        .arg("-s")
        .arg("-H")
        .arg("X-Vault-Token: root-token")
        .arg("-X")
        .arg(method);
    if !body.is_empty() {
        command.arg("-d").arg(body);
    }
    let address = std::env::var("VAULT_ADDR").expect("VAULT_ADDR");
    command.arg(format!("{}/v1/{path}", address.trim_end_matches('/')));

    let output = command.output().expect("curl should run");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn issue() -> Issued {
    let body = vault("GET", "database/creds/app", "");
    let json = rustlavel_core::Json::parse(&body)
        .unwrap_or_else(|e| panic!("the store answered with {body:?}: {e}"));

    let field = |path: &str| {
        json.get(path)
            .and_then(rustlavel_core::Json::as_str)
            .unwrap_or_else(|| panic!("no `{path}` in {body}"))
            .to_string()
    };

    Issued {
        username: field("data.username"),
        password: field("data.password"),
        lease: field("lease_id"),
    }
}

fn revoke(lease: &str) {
    vault("PUT", "sys/leases/revoke", &format!(r#"{{"lease_id":"{lease}"}}"#));
}

/// A pool whose credentials can be replaced under it.
fn pool_for(issued: &Issued) -> (Pool, Credentials) {
    let url = std::env::var("ROTATION_DB_URL").expect("ROTATION_DB_URL");
    let mut config = DatabaseConfig::from_url(&url).expect("a usable URL");

    let credentials = Credentials::new(&issued.username, &issued.password);
    config.credentials = Some(credentials.clone());

    (Pool::new(Arc::new(PostgresDriver::new(config))), credentials)
}

/// Which database user a connection is actually authenticated as.
async fn connected_as(pool: &Pool) -> String {
    let mut connection = pool.acquire().await.expect("a connection");
    let result = connection.simple_query("select current_user").await.expect("a query");
    result.rows[0].get_at::<String>(0).expect("a user name")
}

macro_rules! configured {
    () => {
        match (std::env::var("VAULT_ADDR"), std::env::var("ROTATION_DB_URL")) {
            (Ok(address), Ok(url)) if !address.is_empty() && !url.is_empty() => {}
            _ => {
                eprintln!("skipping: set VAULT_ADDR and ROTATION_DB_URL to run the rotation tests");
                return;
            }
        }
    };
}

#[tokio::test]
async fn a_pool_opens_connections_as_the_account_the_store_issued() {
    configured!();
    let issued = issue();
    let (pool, _credentials) = pool_for(&issued);

    assert_eq!(connected_as(&pool).await, issued.username);

    revoke(&issued.lease);
}

#[tokio::test]
async fn rotating_moves_the_pool_onto_the_new_account_without_a_restart() {
    configured!();
    let first = issue();
    let (pool, credentials) = pool_for(&first);

    // A live pool with an idle connection belonging to the first account.
    assert_eq!(connected_as(&pool).await, first.username);
    settle().await;
    assert_eq!(pool.idle_count().await, 1);

    let second = issue();
    assert_ne!(second.username, first.username, "the store issues a new account each time");
    credentials.rotate(&second.username, &second.password);

    // The next connection is the new account, and the old idle one is gone
    // rather than reused.
    assert_eq!(connected_as(&pool).await, second.username);

    revoke(&first.lease);
    revoke(&second.lease);
}

#[tokio::test]
async fn revoking_the_old_lease_after_a_rotation_disturbs_nothing() {
    // The property the whole exercise is for. Once the pool has moved on, the
    // first account can be deleted — which is what the store does when a lease
    // reaches its maximum life — and the process carries on serving.
    configured!();
    let first = issue();
    let (pool, credentials) = pool_for(&first);

    assert_eq!(connected_as(&pool).await, first.username);
    // The pool returns a connection on a spawned task, so the idle set is not
    // populated the instant the borrow ends.
    settle().await;

    let second = issue();
    credentials.rotate(&second.username, &second.password);
    assert_eq!(pool.retire_superseded().await, 1, "the first account's connection went");

    revoke(&first.lease);

    // Several round trips after the first account has been deleted.
    for _ in 0..3 {
        assert_eq!(connected_as(&pool).await, second.username);
    }

    revoke(&second.lease);
}

#[tokio::test]
async fn an_already_open_connection_survives_its_account_being_deleted() {
    // Documenting the measurement the design rests on, so it is a test rather
    // than a claim in a comment. PostgreSQL authenticates once, at connect
    // time, and does not check again per query — which is why a rotation never
    // has to interrupt work in flight, and equally why a pool must retire old
    // connections deliberately rather than trusting the database to do it.
    configured!();
    let issued = issue();
    let (pool, _credentials) = pool_for(&issued);

    let mut borrowed = pool.acquire().await.expect("a connection");
    borrowed.simple_query("select 1").await.expect("works before the revoke");

    revoke(&issued.lease);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    borrowed
        .simple_query("select 1")
        .await
        .expect("an open session keeps working after its role is dropped");

    // A new one, however, is refused outright.
    drop(borrowed);
    let (fresh, _) = pool_for(&issued);
    assert!(
        fresh.acquire().await.is_err(),
        "a connection with a revoked credential must not be accepted"
    );
}

/// The pool returns a connection on a spawned task, so a test has to let the
/// runtime run before asking what is idle.
async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}
