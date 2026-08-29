//! Integration tests against a real SQL Server.
//!
//! They run only when `SQLSERVER_URL` is set, so `cargo test` stays green on a
//! machine with no database. Start one with:
//!
//! ```text
//! docker run -d --name rustlavel-mssql -e ACCEPT_EULA=Y \
//!   -e MSSQL_SA_PASSWORD='Rustlavel!2026' \
//!   -p 51433:1433 mcr.microsoft.com/mssql/server:2022-latest
//! export SQLSERVER_URL='sqlserver://sa:Rustlavel!2026@127.0.0.1:51433/master'
//! ```
//!
//! On an Apple Silicon machine the image is amd64 only, so add
//! `--platform linux/amd64`; it runs, slowly, under emulation.

use rustlavel_db::driver::{Driver, DriverConnection};
use rustlavel_db::prelude::*;
use rustlavel_db::sqlserver::{Encryption, SqlServerDriver, SqlServerOptions};
use rustlavel_db::{DatabaseConfig, Value};

/// Skip the test when no database is configured, saying so out loud.
macro_rules! database {
    () => {
        match std::env::var("SQLSERVER_URL") {
            Ok(url) if !url.is_empty() => match Database::connect(&url).await {
                Ok(db) => db,
                Err(e) => panic!("SQLSERVER_URL is set but connecting failed: {e}"),
            },
            _ => {
                eprintln!("skipped: set SQLSERVER_URL to run the SQL Server integration tests");
                return;
            }
        }
    };
}

/// The same skip, for a test that needs one connection rather than a pool.
macro_rules! config {
    () => {
        match std::env::var("SQLSERVER_URL") {
            Ok(url) if !url.is_empty() => DatabaseConfig::from_url(&url).unwrap(),
            _ => {
                eprintln!("skipped: set SQLSERVER_URL to run the SQL Server integration tests");
                return;
            }
        }
    };
}

/// Each test owns a uniquely named table, so they can run concurrently against
/// one database without stepping on each other.
async fn fresh_table(db: &Database, name: &str) -> String {
    let table = format!("t_ss_{name}");
    db.run(&format!("drop table if exists {table}")).await.unwrap();
    table
}

#[tokio::test]
async fn connects_and_runs_a_query() {
    let db = database!();

    let answer = db.scalar::<i64>("select 40 + @P1", &[Value::from(2)]).await.unwrap();
    assert_eq!(answer, Some(42));
}

#[tokio::test]
async fn round_trips_every_supported_type() {
    let db = database!();

    let row = db
        .select_one(
            "select @P1 as flag, @P2 as [count], @P3 as ratio, @P4 as label, \
             @P5 as payload, @P6 as blob, cast(null as nvarchar(10)) as missing",
            &[
                Value::from(true),
                Value::from(9_000_000_000i64),
                Value::from(1.5),
                Value::from("hello"),
                Value::from(Json::parse(r#"{"a":[1,2]}"#).unwrap()),
                Value::from(vec![0xde, 0xad, 0xbe, 0xef]),
            ],
        )
        .await
        .unwrap()
        .expect("one row");

    // `bit` is what the dialect maps `Boolean` onto, and the driver converts it
    // back so this reads the same as it does on PostgreSQL.
    assert!(row.get::<bool>("flag").unwrap());
    assert_eq!(row.get::<i64>("count").unwrap(), 9_000_000_000);
    assert_eq!(row.get::<f64>("ratio").unwrap(), 1.5);
    assert_eq!(row.get::<String>("label").unwrap(), "hello");
    assert_eq!(row.get::<Json>("payload").unwrap().get("a.1").unwrap().as_i64(), Some(2));
    assert_eq!(row.get::<Vec<u8>>("blob").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(row.get::<Option<String>>("missing").unwrap(), None);
}

#[tokio::test]
async fn the_exact_types_stay_text_so_their_precision_survives() {
    let db = database!();

    let row = db
        .select_one(
            "select cast('2026-08-29' as date) as [date], \
             cast('01:02:03.1234567' as time) as [time], \
             cast('2026-08-29 01:02:03.123' as datetime2(3)) as [stamp], \
             cast('2026-08-29 01:02:03 -05:30' as datetimeoffset(0)) as [zoned], \
             cast('12345678901234.567890' as decimal(38, 6)) as [exact], \
             cast('12345678-1234-5678-9ABC-DEF012345678' as uniqueidentifier) as [id], \
             cast(12.34 as money) as [amount]",
            &[],
        )
        .await
        .unwrap()
        .expect("one row");

    assert_eq!(row.get::<String>("date").unwrap(), "2026-08-29");
    assert_eq!(row.get::<String>("time").unwrap(), "01:02:03.1234567");
    assert_eq!(row.get::<String>("stamp").unwrap(), "2026-08-29 01:02:03.123");
    assert_eq!(row.get::<String>("zoned").unwrap(), "2026-08-29 01:02:03 -05:30");
    // Not an f64: a decimal that survives the trip is the whole point of one.
    assert_eq!(row.get::<String>("exact").unwrap(), "12345678901234.567890");
    assert_eq!(row.get::<String>("id").unwrap(), "12345678-1234-5678-9ABC-DEF012345678");
    assert_eq!(row.get::<String>("amount").unwrap(), "12.3400");
}

#[tokio::test]
async fn a_parameter_can_never_become_sql() {
    let db = database!();
    let table = fresh_table(&db, "injection").await;

    db.run(&format!(
        "create table {table} (id bigint identity(1,1) primary key, name nvarchar(400) not null)"
    ))
    .await
    .unwrap();

    let hostile = "'; drop table ".to_string() + &table + "; --";
    db.table(&table)
        .insert_without_id(&db, &[("name", Value::from(hostile.as_str()))])
        .await
        .unwrap();

    // The table still exists and holds the string verbatim, because the value
    // travelled as an argument to sp_executesql rather than as SQL text.
    let stored = db.scalar::<String>(&format!("select name from {table}"), &[]).await.unwrap();
    assert_eq!(stored.as_deref(), Some(hostile.as_str()));

    db.run(&format!("drop table {table}")).await.unwrap();
}

#[tokio::test]
async fn a_table_can_be_created_written_to_and_dropped() {
    let db = database!();
    let table = fresh_table(&db, "schema").await;

    let schema = Schema::new(&db);
    schema
        .create(&table, |t| {
            t.id();
            t.string("name");
            t.integer("age");
            t.boolean("active").default_bool(true);
        })
        .await
        .unwrap();

    assert!(schema.has_table(&table).await.unwrap());
    assert!(schema.has_column(&table, "active").await.unwrap());

    for (name, age) in [("Ada", 36), ("Grace", 45), ("Alan", 41)] {
        db.table(&table)
            .insert(&db, &[("name", Value::from(name)), ("age", Value::from(age))])
            .await
            .unwrap();
    }

    let adults = db.table(&table).filter_op("age", ">", 40).latest("age").get(&db).await.unwrap();
    assert_eq!(adults.len(), 2);
    assert_eq!(adults[0].get::<String>("name").unwrap(), "Grace");
    assert!(adults[0].get::<bool>("active").unwrap());

    assert_eq!(db.table(&table).count(&db).await.unwrap(), 3);

    let updated =
        db.table(&table).filter("name", "Ada").update(&db, &[("age", Value::from(37))]).await.unwrap();
    assert_eq!(updated, 1);

    let deleted = db.table(&table).filter("name", "Alan").delete(&db).await.unwrap();
    assert_eq!(deleted, 1);

    schema.drop(&table).await.unwrap();
    assert!(!schema.has_table(&table).await.unwrap());
}

#[tokio::test]
async fn an_insert_reads_its_generated_key_back_through_output_inserted() {
    let db = database!();
    let table = fresh_table(&db, "identity").await;

    db.run(&format!(
        "create table {table} (id bigint identity(1,1) primary key, name nvarchar(100) not null)"
    ))
    .await
    .unwrap();

    let first = db.table(&table).insert(&db, &[("name", Value::from("Ada"))]).await.unwrap();
    let second = db.table(&table).insert(&db, &[("name", Value::from("Grace"))]).await.unwrap();

    assert!(first > 0, "the database should have assigned an id");
    assert_eq!(second, first + 1);

    let name = db
        .scalar::<String>(&format!("select name from {table} where id = @P1"), &[Value::from(second)])
        .await
        .unwrap();
    assert_eq!(name.as_deref(), Some("Grace"));

    db.run(&format!("drop table {table}")).await.unwrap();
}

#[tokio::test]
async fn a_transaction_rolls_back_as_a_unit() {
    let config = config!();
    let table = "t_ss_tx";

    // One connection throughout: a transaction that spans two connections is
    // not a transaction. The framework's own `Database::begin` sends `begin`,
    // which is not T-SQL, so the driver is exercised directly here.
    let driver = SqlServerDriver::new(config);
    let mut connection = driver.connect().await.unwrap();

    connection.simple_query(&format!("drop table if exists {table}")).await.unwrap();
    connection
        .simple_query(&format!(
            "create table {table} (id bigint identity(1,1) primary key, name nvarchar(100) not null)"
        ))
        .await
        .unwrap();

    assert!(!connection.in_transaction());
    connection.simple_query("begin transaction").await.unwrap();
    // The driver learns this from the ENVCHANGE the server volunteers, which is
    // what keeps a half-finished transaction out of the pool.
    assert!(connection.in_transaction());

    connection
        .query(
            &format!("insert into {table} (name) values (@P1)"),
            &[Value::from("discarded")],
        )
        .await
        .unwrap();
    assert_eq!(count(&mut connection, table).await, 1);

    connection.simple_query("rollback transaction").await.unwrap();
    assert!(!connection.in_transaction());
    assert_eq!(count(&mut connection, table).await, 0);

    // And the same again, committed this time.
    connection.simple_query("begin transaction").await.unwrap();
    let affected = connection
        .query(&format!("insert into {table} (name) values (@P1)"), &[Value::from("kept")])
        .await
        .unwrap()
        .affected;
    assert_eq!(affected, 1);
    connection.simple_query("commit transaction").await.unwrap();

    assert!(!connection.in_transaction());
    assert_eq!(count(&mut connection, table).await, 1);

    connection.simple_query(&format!("drop table {table}")).await.unwrap();
    connection.close().await;
}

async fn count(connection: &mut Box<dyn DriverConnection>, table: &str) -> i64 {
    connection
        .query(&format!("select count(*) from {table}"), &[])
        .await
        .unwrap()
        .rows[0]
        .get_at::<i64>(0)
        .unwrap()
}

#[tokio::test]
async fn a_broken_statement_reports_its_error_number_and_the_sql() {
    let db = database!();

    let error = db.select("select * from a_table_that_is_not_there", &[]).await.unwrap_err();
    let text = error.to_string();

    // 208 is "Invalid object name".
    assert!(text.contains("208"), "should carry the error number: {text}");
    assert!(text.contains("severity"), "{text}");
    assert!(text.contains("SQL: select * from a_table_that_is_not_there"), "{text}");
}

#[tokio::test]
async fn a_wrong_password_is_reported_with_somewhere_to_look() {
    let mut config = config!();
    config.password = "definitely-not-the-password".into();

    let error = match SqlServerDriver::new(config).connect().await {
        Err(error) => error.to_string(),
        Ok(_) => panic!("a wrong password should not connect"),
    };

    assert!(error.contains("18456"), "{error}");
    assert!(error.contains("DATABASE_URL"), "{error}");
    assert!(!error.contains("definitely-not-the-password"), "the password leaked: {error}");
}

#[tokio::test]
async fn a_statement_larger_than_one_packet_is_split_and_reassembled() {
    let db = database!();

    // Comfortably past the 4096-byte default packet size, twice over once it is
    // encoded as UTF-16.
    let long = "x".repeat(9_000);
    let echoed = db.scalar::<String>("select @P1", &[Value::from(long.as_str())]).await.unwrap();

    assert_eq!(echoed.as_deref(), Some(long.as_str()));
}

#[tokio::test]
async fn a_login_only_encrypted_session_still_works() {
    let config = config!();

    // ENCRYPT_OFF: TLS wraps the login packet and is then torn down, so
    // everything after this point is plaintext TDS on the same socket.
    let driver = SqlServerDriver::with_options(
        config,
        SqlServerOptions { encryption: Encryption::LoginOnly, ..SqlServerOptions::default() },
    );

    let mut connection = driver.connect().await.unwrap();
    let rows = connection.query("select @P1 + 1", &[Value::from(41)]).await.unwrap().rows;

    assert_eq!(rows[0].get_at::<i64>(0).unwrap(), 42);
    connection.close().await;
}

#[tokio::test]
async fn the_pool_reuses_connections() {
    let db = database!();

    for _ in 0..5 {
        db.scalar::<i64>("select 1", &[]).await.unwrap();
        // Give the connection time to be handed back before the next round.
        tokio::task::yield_now().await;
    }

    assert!(db.pool().idle_count().await >= 1, "a connection should have returned to the pool");
}

#[tokio::test]
async fn paging_walks_a_table_that_has_no_ordering_of_its_own() {
    let db = database!();
    let table = fresh_table(&db, "paging").await;

    Schema::new(&db)
        .create(&table, |t| {
            t.id();
            t.string("title");
        })
        .await
        .unwrap();

    for index in 1..=25 {
        db.table(&table)
            .insert_without_id(&db, &[("title", Value::from(format!("post {index}")))])
            .await
            .unwrap();
    }

    let second = db.table(&table).paginate(&db, 2, 10).await.unwrap();
    assert_eq!(second.total, 25);
    assert_eq!(second.rows.len(), 10);
    assert_eq!(second.last_page(), 3);
    assert!(second.has_more());

    let last = db.table(&table).paginate(&db, 3, 10).await.unwrap();
    assert_eq!(last.rows.len(), 5);
    assert!(!last.has_more());

    Schema::new(&db).drop(&table).await.unwrap();
}
