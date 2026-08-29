//! Integration tests against a real MySQL server.
//!
//! They run only when `MYSQL_URL` is set, so `cargo test` stays green on a
//! machine with no database. Start one with:
//!
//! ```text
//! docker run -d --name rustlavel-mysql -e MYSQL_ROOT_PASSWORD=secret \
//!   -e MYSQL_DATABASE=rustlavel_test -p 53306:3306 mysql:8
//! export MYSQL_URL=mysql://root:secret@127.0.0.1:53306/rustlavel_test
//! ```
//!
//! MySQL 8 authenticates with `caching_sha2_password`, whose fast path needs
//! the account to already be in the server's cache. A container that has never
//! seen the account demands the full path, which sends the password itself and
//! so needs an encrypted connection — add `?sslmode=require` to `MYSQL_URL` and
//! the driver takes that path happily. Warming the cache with the official
//! client also works, and is what this was before the driver spoke TLS:
//!
//! ```text
//! IP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' rustlavel-mysql)
//! docker exec rustlavel-mysql mysql -h $IP -uroot -psecret --get-server-public-key -e 'select 1'
//! ```
//!
//! To cover `mysql_native_password` as well, start the server with
//! `--mysql-native-password=ON` (MySQL 8.4 has it off by default), create an
//! account that uses it, and set `MYSQL_NATIVE_URL`:
//!
//! ```text
//! docker exec rustlavel-mysql mysql -uroot -psecret -e \
//!   "create user 'nativeuser'@'%' identified with mysql_native_password by 'secret';
//!    grant all on rustlavel_test.* to 'nativeuser'@'%'"
//! export MYSQL_NATIVE_URL=mysql://nativeuser:secret@127.0.0.1:53306/rustlavel_test
//! ```
//!
//! These drive the driver directly — `MySqlDriver` and `MySqlConnection` — so a
//! failure points at the wire protocol rather than at the layers above it.
//! `tests/conformance.rs` covers the same server through `Database`, the query
//! builder and the migrator.

use rustlavel_db::driver::{Driver, DriverConnection};
use rustlavel_db::mysql::{MySqlConnection, MySqlDriver};
use rustlavel_db::pool::Pool;
use rustlavel_db::prelude::Json;
use rustlavel_db::{FromValue, Value};
use std::sync::Arc;

/// Skip the test when no database is configured, saying so out loud.
macro_rules! mysql {
    () => {
        match std::env::var("MYSQL_URL") {
            Ok(url) if !url.is_empty() => match connect(&url).await {
                Ok(connection) => connection,
                Err(e) => panic!("MYSQL_URL is set but connecting failed: {e}"),
            },
            _ => {
                eprintln!("skipped: set MYSQL_URL to run the MySQL integration tests");
                return;
            }
        }
    };
}

async fn connect(url: &str) -> rustlavel_db::Result<Box<dyn DriverConnection>> {
    MySqlDriver::from_url(url)?.connect().await
}

/// Each test owns a uniquely named table, so they can run concurrently against
/// one database without stepping on each other.
async fn fresh_table(connection: &mut Box<dyn DriverConnection>, name: &str) -> String {
    let table = format!("t_{name}");
    connection
        .simple_query(&format!("drop table if exists {table}"))
        .await
        .expect("dropping a table that may not exist should succeed");
    table
}

/// Whether the current database holds a table by this name.
async fn table_exists(connection: &mut Box<dyn DriverConnection>, table: &str) -> Option<i64> {
    scalar::<i64>(
        connection,
        "select count(*) from information_schema.tables \
         where table_schema = database() and table_name = ?",
        &[Value::from(table)],
    )
    .await
}

/// Read one value out of the first column of the first row.
async fn scalar<T: FromValue>(
    connection: &mut Box<dyn DriverConnection>,
    sql: &str,
    params: &[Value],
) -> Option<T> {
    let result = connection.query(sql, params).await.expect("the query should succeed");
    result.rows.first().map(|row| row.get_at::<T>(0).expect("the column should convert"))
}

#[tokio::test]
async fn connects_and_runs_a_query() {
    let mut connection = mysql!();

    let answer = scalar::<i64>(&mut connection, "select 40 + ?", &[Value::from(2)]).await;
    assert_eq!(answer, Some(42));
}

#[tokio::test]
async fn the_handshake_learns_who_it_is_talking_to() {
    let url = match std::env::var("MYSQL_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => {
            eprintln!("skipped: set MYSQL_URL to run the MySQL integration tests");
            return;
        }
    };

    let config = MySqlDriver::from_url(&url).unwrap().config().clone();
    let connection = MySqlConnection::connect(&config).await.expect("the handshake should finish");

    assert!(!connection.server_version().is_empty(), "the server introduces itself");
    assert!(connection.connection_id() > 0, "every session gets an id");
    assert!(!connection.in_transaction());
    assert!(!connection.is_broken());
    connection.close().await;
}

#[tokio::test]
async fn round_trips_every_supported_type() {
    let mut connection = mysql!();
    let table = fresh_table(&mut connection, "types").await;

    connection
        .simple_query(&format!(
            "create table {table} (\
               id bigint unsigned not null auto_increment primary key,\
               flag tinyint(1) not null,\
               tally bigint not null,\
               ratio double not null,\
               label varchar(64) not null,\
               payload json not null,\
               picture longblob not null,\
               price decimal(14, 4) not null,\
               made_at datetime(6) not null,\
               born date not null,\
               missing varchar(10) null)"
        ))
        .await
        .unwrap();

    connection
        .query(
            &format!(
                "insert into {table} \
                 (flag, tally, ratio, label, payload, picture, price, made_at, born, missing) \
                 values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
            ),
            &[
                Value::from(true),
                Value::from(9_000_000_000i64),
                Value::from(1.5),
                Value::from("hello"),
                Value::from(Json::parse(r#"{"a":[1,2]}"#).unwrap()),
                Value::from(vec![0xde, 0xad, 0xbe, 0xef]),
                Value::from("12345.6789"),
                Value::from("2026-08-29 10:30:05.123456"),
                Value::from("2026-08-29"),
                Value::Null,
            ],
        )
        .await
        .unwrap();

    let rows = connection.query(&format!("select * from {table}"), &[]).await.unwrap().rows;
    let row = rows.first().expect("one row");

    // `tinyint(1)` is a number, because the MySQL dialect says booleans are
    // integers and nothing on the wire tells a flag from a small counter.
    assert_eq!(row.get::<i64>("flag").unwrap(), 1);
    assert_eq!(row.get::<i64>("tally").unwrap(), 9_000_000_000);
    assert_eq!(row.get::<f64>("ratio").unwrap(), 1.5);
    assert_eq!(row.get::<String>("label").unwrap(), "hello");
    assert_eq!(row.get::<Json>("payload").unwrap().get("a.1").unwrap().as_i64(), Some(2));
    assert_eq!(row.get::<Vec<u8>>("picture").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(row.get::<Option<String>>("missing").unwrap(), None);

    // Kept as text so the precision the column exists for survives the trip.
    assert_eq!(row.get::<String>("price").unwrap(), "12345.6789");
    assert_eq!(row.get::<String>("made_at").unwrap(), "2026-08-29 10:30:05.123456");
    assert_eq!(row.get::<String>("born").unwrap(), "2026-08-29");

    // An unsigned auto_increment key still reads as an integer.
    assert!(row.get::<i64>("id").unwrap() > 0);

    connection.simple_query(&format!("drop table {table}")).await.unwrap();
}

#[tokio::test]
async fn the_text_protocol_decodes_the_same_values_as_the_binary_one() {
    let mut connection = mysql!();
    let table = fresh_table(&mut connection, "textproto").await;

    connection
        .simple_query(&format!(
            "create table {table} (n bigint not null, r double not null, s varchar(20) not null, \
             j json not null, d decimal(8, 2) not null)"
        ))
        .await
        .unwrap();
    connection
        .simple_query(&format!(
            "insert into {table} values (7, 1.5, 'ada', '{{\"k\":1}}', 3.25)"
        ))
        .await
        .unwrap();

    // `simple_query` goes over COM_QUERY, where every column is ASCII text; the
    // decoded values must be indistinguishable from the prepared path's.
    let text = connection.simple_query(&format!("select * from {table}")).await.unwrap();
    let binary = connection.query(&format!("select * from {table}"), &[]).await.unwrap();

    for rows in [&text.rows, &binary.rows] {
        let row = rows.first().expect("one row");
        assert_eq!(row.get::<i64>("n").unwrap(), 7);
        assert_eq!(row.get::<f64>("r").unwrap(), 1.5);
        assert_eq!(row.get::<String>("s").unwrap(), "ada");
        assert_eq!(row.get::<Json>("j").unwrap().get("k").unwrap().as_i64(), Some(1));
        assert_eq!(row.get::<String>("d").unwrap(), "3.25");
    }

    connection.simple_query(&format!("drop table {table}")).await.unwrap();
}

#[tokio::test]
async fn a_parameter_can_never_become_sql() {
    let mut connection = mysql!();
    let table = fresh_table(&mut connection, "injection").await;

    connection
        .simple_query(&format!(
            "create table {table} (id bigint unsigned not null auto_increment primary key, \
             name text not null)"
        ))
        .await
        .unwrap();

    let hostile = format!("'; drop table {table}; --");
    connection
        .query(
            &format!("insert into {table} (name) values (?)"),
            &[Value::from(hostile.as_str())],
        )
        .await
        .unwrap();

    // The table still exists and holds the string verbatim.
    let stored = scalar::<String>(&mut connection, &format!("select name from {table}"), &[]).await;
    assert_eq!(stored.as_deref(), Some(hostile.as_str()));

    let count = scalar::<i64>(&mut connection, &format!("select count(*) from {table}"), &[]).await;
    assert_eq!(count, Some(1));

    connection.simple_query(&format!("drop table {table}")).await.unwrap();
}

#[tokio::test]
async fn a_second_statement_smuggled_into_one_is_refused_by_the_server() {
    let mut connection = mysql!();
    let table = fresh_table(&mut connection, "multi").await;

    connection
        .simple_query(&format!("create table {table} (id bigint not null)"))
        .await
        .unwrap();

    // The driver does not negotiate CLIENT_MULTI_STATEMENTS, so the server
    // itself refuses rather than the framework having to police it.
    let error = connection
        .simple_query(&format!("select 1; drop table {table}"))
        .await
        .expect_err("two statements in one packet should be refused");
    assert!(error.to_string().contains("1064"), "{error}");

    // Still there.
    let survives =
        scalar::<i64>(&mut connection, &format!("select count(*) from {table}"), &[]).await;
    assert_eq!(survives, Some(0));

    connection.simple_query(&format!("drop table {table}")).await.unwrap();
}

#[tokio::test]
async fn a_generated_key_comes_back_from_the_insert_itself() {
    let mut connection = mysql!();
    let table = fresh_table(&mut connection, "keys").await;

    connection
        .simple_query(&format!(
            "create table {table} (id bigint unsigned not null auto_increment primary key, \
             name varchar(40) not null)"
        ))
        .await
        .unwrap();

    let mut generated = Vec::new();
    for name in ["Ada", "Grace", "Alan"] {
        let result = connection
            .query(&format!("insert into {table} (name) values (?)"), &[Value::from(name)])
            .await
            .unwrap();

        assert_eq!(result.affected, 1);
        // MySQL reports the key in the packet that acknowledges the insert, not
        // as a row — which is the whole reason `last_insert_id` exists.
        assert!(result.rows.is_empty());
        generated.push(result.last_insert_id.expect("an auto_increment key"));
    }

    assert_eq!(generated, vec![1, 2, 3]);

    // The dialect's separate query agrees with the key the driver reported.
    let last = scalar::<i64>(&mut connection, "select last_insert_id()", &[]).await;
    assert_eq!(last, Some(3));

    // A statement that generates no key reports none, rather than zero.
    let update = connection
        .query(&format!("update {table} set name = ? where name = ?"), &[
            Value::from("Ada Lovelace"),
            Value::from("Ada"),
        ])
        .await
        .unwrap();
    assert_eq!(update.affected, 1);
    assert_eq!(update.last_insert_id, None);

    connection.simple_query(&format!("drop table {table}")).await.unwrap();
}

#[tokio::test]
async fn a_transaction_rolls_back_as_a_unit() {
    let mut connection = mysql!();
    let table = fresh_table(&mut connection, "tx").await;

    connection
        .simple_query(&format!(
            "create table {table} (id bigint unsigned not null auto_increment primary key, \
             name varchar(40) not null) engine = innodb"
        ))
        .await
        .unwrap();

    connection.simple_query("start transaction").await.unwrap();
    assert!(connection.in_transaction(), "the server reports an open transaction");

    connection
        .query(&format!("insert into {table} (name) values (?)"), &[Value::from("ghost")])
        .await
        .unwrap();
    assert_eq!(
        scalar::<i64>(&mut connection, &format!("select count(*) from {table}"), &[]).await,
        Some(1),
        "the row is visible inside the transaction"
    );

    connection.simple_query("rollback").await.unwrap();
    assert!(!connection.in_transaction(), "and the connection is clean again");

    assert_eq!(
        scalar::<i64>(&mut connection, &format!("select count(*) from {table}"), &[]).await,
        Some(0),
        "nothing was written"
    );

    // The same sequence, committed, does write.
    connection.simple_query("start transaction").await.unwrap();
    connection
        .query(&format!("insert into {table} (name) values (?)"), &[Value::from("kept")])
        .await
        .unwrap();
    connection.simple_query("commit").await.unwrap();

    assert_eq!(
        scalar::<i64>(&mut connection, &format!("select count(*) from {table}"), &[]).await,
        Some(1)
    );

    connection.simple_query(&format!("drop table {table}")).await.unwrap();
}

#[tokio::test]
async fn a_table_can_be_created_and_dropped() {
    let mut connection = mysql!();
    let table = fresh_table(&mut connection, "ddl").await;

    assert_eq!(table_exists(&mut connection, &table).await, Some(0));

    connection
        .simple_query(&format!(
            "create table {table} (id bigint unsigned not null auto_increment primary key, \
             email varchar(255) not null, unique key {table}_email (email))"
        ))
        .await
        .unwrap();
    assert_eq!(table_exists(&mut connection, &table).await, Some(1));

    // The unique index is real: a duplicate is rejected by the database.
    connection
        .query(&format!("insert into {table} (email) values (?)"), &[Value::from("a@b.com")])
        .await
        .unwrap();
    let duplicate = connection
        .query(&format!("insert into {table} (email) values (?)"), &[Value::from("a@b.com")])
        .await;
    assert!(duplicate.is_err(), "a duplicate should be refused");

    connection.simple_query(&format!("drop table {table}")).await.unwrap();
    assert_eq!(table_exists(&mut connection, &table).await, Some(0));
}

#[tokio::test]
async fn a_broken_statement_reports_the_mysql_error_code_and_the_sql() {
    let mut connection = mysql!();

    let error = connection
        .query("select * from a_table_that_is_not_there", &[])
        .await
        .expect_err("the table does not exist");
    let text = error.to_string();

    assert!(text.contains("1146"), "should carry the MySQL error number: {text}");
    assert!(text.contains("42S02"), "should carry the SQL state: {text}");
    assert!(text.contains("SQL: select * from a_table_that_is_not_there"), "{text}");
    assert!(text.contains("migrations"), "should say what to try: {text}");

    // The connection survives a statement error and is still usable.
    assert!(!connection.is_broken());
    assert_eq!(scalar::<i64>(&mut connection, "select 1", &[]).await, Some(1));
}

#[tokio::test]
async fn binding_the_wrong_number_of_parameters_is_caught_before_the_server_sees_it() {
    let mut connection = mysql!();

    let error = connection
        .query("select ?, ?", &[Value::from(1)])
        .await
        .expect_err("two placeholders, one binding");

    assert!(error.to_string().contains("2 parameter(s)"), "{error}");
    assert!(!connection.is_broken(), "a caller's mistake must not kill the connection");
}

#[tokio::test]
async fn a_value_larger_than_a_single_length_byte_survives_the_round_trip() {
    let mut connection = mysql!();
    let table = fresh_table(&mut connection, "big").await;

    connection
        .simple_query(&format!("create table {table} (body longtext not null)"))
        .await
        .unwrap();

    // Long enough to need the three-byte length-encoded form in both the bound
    // parameter and the row that comes back.
    let body = "rustlavel ".repeat(30_000);
    connection
        .query(&format!("insert into {table} (body) values (?)"), &[Value::from(body.as_str())])
        .await
        .unwrap();

    let stored = scalar::<String>(&mut connection, &format!("select body from {table}"), &[]).await;
    assert_eq!(stored.as_deref(), Some(body.as_str()));

    connection.simple_query(&format!("drop table {table}")).await.unwrap();
}

#[tokio::test]
async fn many_statements_run_on_one_connection_without_it_losing_its_place() {
    let mut connection = mysql!();

    // Every command resets the packet sequence, and every prepared statement is
    // closed; a leak in either shows up as a desynchronised connection here.
    for n in 0..40i64 {
        assert_eq!(scalar::<i64>(&mut connection, "select ?", &[Value::from(n)]).await, Some(n));
    }
    assert!(!connection.is_broken());
}

#[tokio::test]
async fn the_pool_hands_out_and_takes_back_mysql_connections() {
    let url = match std::env::var("MYSQL_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => {
            eprintln!("skipped: set MYSQL_URL to run the MySQL integration tests");
            return;
        }
    };

    let driver: Arc<dyn Driver> = Arc::new(MySqlDriver::from_url(&url).unwrap());
    let pool = Pool::new(driver);
    pool.verify().await.expect("the pool should open a connection");

    for _ in 0..5 {
        let mut connection = pool.acquire().await.unwrap();
        connection.query("select 1", &[]).await.unwrap();
        drop(connection);
        // Give the connection time to be handed back before the next round.
        tokio::task::yield_now().await;
    }

    assert!(pool.idle_count().await >= 1, "a connection should have returned to the pool");
    pool.close().await;
}

#[tokio::test]
async fn a_wrong_password_under_caching_sha2_explains_the_secure_channel_it_needs() {
    let url = match std::env::var("MYSQL_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => {
            eprintln!("skipped: set MYSQL_URL to run the MySQL integration tests");
            return;
        }
    };

    let mut config = MySqlDriver::from_url(&url).unwrap().config().clone();
    config.password = "definitely-not-the-password".into();
    // The point of this test is the refusal to send a password in the clear, so
    // it has to be in the clear — `MYSQL_URL` may well ask for TLS, and with a
    // tunnel the driver takes the full path and the server simply says no.
    config.tls_mode = rustlavel_db::tls::TlsMode::Disable;

    let error = match MySqlConnection::connect(&config).await {
        Err(error) => error.to_string(),
        Ok(_) => panic!("a wrong password should not authenticate"),
    };

    // The server cannot tell a wrong password from an account it has not
    // cached, so it asks for the full exchange in both cases. What matters is
    // that the driver refuses to send the password in the clear and says so in
    // terms the developer can act on, rather than hanging or failing blankly.
    assert!(error.contains("caching_sha2_password"), "{error}");
    assert!(error.contains("--get-server-public-key"), "{error}");
    assert!(error.contains("DATABASE_URL"), "{error}");
    assert!(!error.contains("definitely-not-the-password"), "the password must not leak: {error}");
}

#[tokio::test]
async fn a_wrong_password_under_native_password_names_the_error_code() {
    let url = match std::env::var("MYSQL_NATIVE_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => {
            eprintln!(
                "skipped: set MYSQL_NATIVE_URL to a mysql_native_password account to cover the \
                 SHA-1 handshake"
            );
            return;
        }
    };

    let mut config = MySqlDriver::from_url(&url).unwrap().config().clone();
    config.password = "definitely-not-the-password".into();
    // The point of this test is the refusal to send a password in the clear, so
    // it has to be in the clear — `MYSQL_URL` may well ask for TLS, and with a
    // tunnel the driver takes the full path and the server simply says no.
    config.tls_mode = rustlavel_db::tls::TlsMode::Disable;

    let error = match MySqlConnection::connect(&config).await {
        Err(error) => error.to_string(),
        Ok(_) => panic!("a wrong password should not authenticate"),
    };

    assert!(error.contains("1045"), "{error}");
    assert!(error.contains("DATABASE_URL"), "{error}");
    assert!(!error.contains("definitely-not-the-password"), "the password must not leak: {error}");
}

#[tokio::test]
async fn an_unknown_database_is_reported_by_name() {
    let url = match std::env::var("MYSQL_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => {
            eprintln!("skipped: set MYSQL_URL to run the MySQL integration tests");
            return;
        }
    };

    let mut config = MySqlDriver::from_url(&url).unwrap().config().clone();
    config.database = "no_such_database_here".into();

    let error = match MySqlConnection::connect(&config).await {
        Err(error) => error.to_string(),
        Ok(_) => panic!("that database does not exist"),
    };

    assert!(error.contains("1049"), "{error}");
    assert!(error.contains("no_such_database_here"), "{error}");
}

#[tokio::test]
async fn mysql_native_password_authenticates_too() {
    let url = match std::env::var("MYSQL_NATIVE_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => {
            eprintln!(
                "skipped: set MYSQL_NATIVE_URL to a mysql_native_password account to cover the \
                 SHA-1 handshake"
            );
            return;
        }
    };

    let mut connection = connect(&url).await.expect("the native password handshake should finish");
    assert_eq!(scalar::<i64>(&mut connection, "select 40 + ?", &[Value::from(2)]).await, Some(42));
}
