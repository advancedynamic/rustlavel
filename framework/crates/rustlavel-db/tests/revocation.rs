//! What each database does to an open connection when its account is deleted.
//!
//! The pool's rotation logic rests on this, so it is measured rather than
//! assumed — and the three databases do not agree, which is the whole reason
//! this file exists.
//!
//! | | Can the account be dropped while connected? | Does an open connection survive? | Is a new one refused? |
//! |---|---|---|---|
//! | PostgreSQL 16 | yes | **yes** | yes (`28P01`) |
//! | MySQL 8.4 | yes | **yes** | yes |
//! | SQL Server 2022 | **no** (`15434`) | **no**, once the sessions are killed | yes (`18456`) |
//!
//! PostgreSQL and MySQL authenticate once, at connect time, and never look
//! again — so revoking a credential cannot interrupt work in flight. SQL Server
//! refuses to drop a login that is logged in at all, so revoking it *requires*
//! killing the sessions first, and that does interrupt whatever they were doing.
//!
//! The rule that follows, and it is the same rule for all three: **retire the
//! old connections before the old lease is revoked**, not after. On PostgreSQL
//! and MySQL the wrong order is survivable; on SQL Server it is not.
//!
//! Each test runs only when its own URL is set.
//!
//! ```text
//! docker run -d --name rustlavel-vault-pg -e POSTGRES_PASSWORD=rootpass \
//!   -e POSTGRES_DB=appdb -p 55433:5432 postgres:16
//! docker run -d --name rustlavel-mysql -e MYSQL_ROOT_PASSWORD=rootpass \
//!   -e MYSQL_DATABASE=appdb -p 33306:3306 mysql:8.4
//! docker run -d --name rustlavel-mssql --platform linux/amd64 -e ACCEPT_EULA=Y \
//!   -e 'MSSQL_SA_PASSWORD=Rustlavel!2026' -p 51433:1433 \
//!   mcr.microsoft.com/mssql/server:2022-latest
//!
//! export REVOCATION_PG_URL='postgres://postgres:rootpass@127.0.0.1:55433/appdb?sslmode=disable'
//! export REVOCATION_MYSQL_URL='mysql://root:rootpass@127.0.0.1:33306/appdb'
//! export REVOCATION_MSSQL_URL='sqlserver://sa:Rustlavel!2026@127.0.0.1:51433/master'
//! ```

use rustlavel_db::prelude::*;

macro_rules! admin {
    ($variable:literal) => {
        match std::env::var($variable) {
            Ok(url) if !url.is_empty() => match Database::connect(&url).await {
                Ok(db) => (db, url),
                Err(e) => panic!("{} is set but connecting failed: {e}", $variable),
            },
            _ => {
                eprintln!("skipping: {} is not set", $variable);
                return;
            }
        }
    };
}

/// Swap the credentials in a URL for a freshly made account's.
fn as_user(url: &str, user: &str, password: &str) -> String {
    let (scheme, rest) = url.split_once("://").expect("a scheme");
    let authority = rest.rsplit_once('@').map(|(_, host)| host).unwrap_or(rest);
    format!("{scheme}://{user}:{password}@{authority}")
}

#[tokio::test]
async fn postgres_lets_an_open_connection_outlive_its_role() {
    let (admin, url) = admin!("REVOCATION_PG_URL");
    let (user, password) = ("v_probe_pg", "Probe!2026xyz");

    let _ = admin.run(&format!(r#"drop role if exists "{user}""#)).await;
    admin
        .run(&format!(r#"create role "{user}" with login password '{password}'"#))
        .await
        .expect("create the role");

    let mine = Database::connect(&as_user(&url, user, password)).await.expect("connect");
    assert_eq!(mine.scalar::<i64>("select 1", &[]).await.unwrap(), Some(1));

    // PostgreSQL drops a role with an open session without complaint.
    admin.run(&format!(r#"drop role "{user}""#)).await.expect("drop the role");

    assert_eq!(
        mine.scalar::<i64>("select 1", &[]).await.unwrap(),
        Some(1),
        "an open connection must keep working: authentication happened at connect time"
    );
    assert!(
        Database::connect(&as_user(&url, user, password)).await.is_err(),
        "a new connection with a deleted account must be refused"
    );
}

#[tokio::test]
async fn mysql_lets_an_open_connection_outlive_its_user() {
    let (admin, url) = admin!("REVOCATION_MYSQL_URL");
    let (user, password) = ("v_probe_my", "Probe!2026xyz");

    let _ = admin.run(&format!("drop user if exists '{user}'@'%'")).await;
    admin
        .run(&format!("create user '{user}'@'%' identified by '{password}'"))
        .await
        .expect("create the user");
    admin.run(&format!("grant select on appdb.* to '{user}'@'%'")).await.expect("grant");

    // MySQL 8.4 removed `mysql_native_password`, so this account uses
    // caching_sha2_password, whose full path needs an encrypted channel — a
    // brand new account is never in the server's cache.
    let mine = Database::connect(&format!("{}?sslmode=require", as_user(&url, user, password)))
        .await
        .expect("connect");
    assert_eq!(mine.scalar::<i64>("select 1", &[]).await.unwrap(), Some(1));

    admin.run(&format!("drop user '{user}'@'%'")).await.expect("drop the user");

    assert_eq!(
        mine.scalar::<i64>("select 1", &[]).await.unwrap(),
        Some(1),
        "an open connection must keep working, as on PostgreSQL"
    );
    assert!(
        Database::connect(&format!("{}?sslmode=require", as_user(&url, user, password)))
            .await
            .is_err(),
        "a new connection with a deleted account must be refused"
    );
}

#[tokio::test]
async fn sql_server_refuses_to_drop_a_login_that_is_connected() {
    // The difference that stops "rotation never interrupts anything" from being
    // a statement about databases in general. SQL Server will not delete the
    // login at all while a session holds it, so a secret store revoking one has
    // to kill the sessions first — and that does interrupt them.
    let (admin, url) = admin!("REVOCATION_MSSQL_URL");
    let (user, password) = ("v_probe_ms", "Probe!2026xyz");

    let _ = admin.run(&format!("drop login [{user}]")).await;
    admin
        .run(&format!("create login [{user}] with password = '{password}'"))
        .await
        .expect("create the login");

    let mine = Database::connect(&as_user(&url, user, password)).await.expect("connect");
    assert_eq!(mine.scalar::<i64>("select 1", &[]).await.unwrap(), Some(1));

    let refused = admin
        .run(&format!("drop login [{user}]"))
        .await
        .expect_err("SQL Server must refuse while the login is in use");
    assert!(refused.to_string().contains("15434"), "got {refused}");
    assert!(refused.to_string().contains("currently logged in"), "got {refused}");

    // What a secret store has to do instead, and what it costs.
    let sessions = admin
        .select(
            &format!("select session_id from sys.dm_exec_sessions where login_name = '{user}'"),
            &[],
        )
        .await
        .expect("the session list");
    assert!(!sessions.is_empty(), "the connection above should be listed");

    for row in &sessions {
        let id: i64 = row.get("session_id").expect("a session id");
        admin.run(&format!("kill {id}")).await.expect("kill the session");
    }
    admin.run(&format!("drop login [{user}]")).await.expect("now it drops");

    assert!(
        mine.scalar::<i64>("select 1", &[]).await.is_err(),
        "the killed session must be gone — this is the case the other two do not have"
    );
    assert!(
        Database::connect(&as_user(&url, user, password)).await.is_err(),
        "a new connection with a deleted login must be refused"
    );
}
