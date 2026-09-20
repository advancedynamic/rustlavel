//! SQLite, exercised for real. No container, no server, no environment
//! variable — which is the whole reason this driver exists.

#![cfg(feature = "sqlite")]

use rustlavel_db::prelude::*;
use rustlavel_db::{Database, DatabaseConfig, Schema};

async fn db() -> Database {
    Database::connect("sqlite://:memory:").await.expect("an in-memory database opens")
}

#[tokio::test]
async fn a_table_is_created_written_and_read_back() {
    let db = db().await;
    let schema = Schema::new(&db);

    schema
        .create("people", |t| {
            t.id();
            t.string("name");
            t.integer("age").nullable();
            t.boolean("active").default_bool(true);
        })
        .await
        .expect("the table is created");

    let id = db
        .table("people")
        .insert(&db, &[("name", "Ada".into()), ("age", 36.into()), ("active", true.into())])
        .await
        .expect("inserted");
    assert!(id > 0, "SQLite did not report the row id it generated");

    let row = db.table("people").filter("name", "Ada").first(&db).await.unwrap().expect("found");
    assert_eq!(row.get::<String>("name").unwrap(), "Ada");
    assert_eq!(row.get::<i64>("age").unwrap(), 36);
    // SQLite has no boolean; the dialect says so and the binding has to agree.
    assert!(row.get::<bool>("active").unwrap(), "a boolean did not survive the round trip");

    assert_eq!(db.table("people").count(&db).await.unwrap(), 1);
}

/// The point of `:memory:` being capped at one connection. Each in-memory
/// SQLite connection is its own blank database, so a pool of several would
/// create a table on one and not find it on the next — intermittently.
#[tokio::test]
async fn an_in_memory_database_is_the_same_database_on_every_acquire() {
    let db = db().await;
    Schema::new(&db)
        .create("notes", |t| {
            t.id();
            t.text("body");
        })
        .await
        .unwrap();

    // Enough round trips that a pool of more than one would certainly have
    // handed out a second, empty database by now.
    for n in 0..25 {
        db.table("notes").insert(&db, &[("body", format!("note {n}").into())]).await.unwrap();
        assert_eq!(db.table("notes").count(&db).await.unwrap(), n + 1);
    }
}

#[tokio::test]
async fn a_transaction_commits_and_rolls_back() {
    let db = db().await;
    Schema::new(&db)
        .create("entries", |t| {
            t.id();
            t.big_integer("amount");
        })
        .await
        .unwrap();

    let mut tx = db.begin().await.unwrap();
    db.table("entries").insert_in(&mut tx, &[("amount", 10.into())]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(db.table("entries").count(&db).await.unwrap(), 1);

    let mut tx = db.begin().await.unwrap();
    db.table("entries").insert_in(&mut tx, &[("amount", 99.into())]).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(db.table("entries").count(&db).await.unwrap(), 1, "a rolled-back insert survived");
}

/// Foreign keys are off by default in SQLite — a promise it made in 2005 and
/// cannot take back. Every other database here enforces them, so a migration
/// proved against SQLite would behave differently on a server unless the
/// driver switches them on.
#[tokio::test]
async fn foreign_keys_are_enforced_rather_than_ignored() {
    let db = db().await;
    let schema = Schema::new(&db);
    schema
        .create("authors", |t| {
            t.id();
            t.string("name");
        })
        .await
        .unwrap();
    schema
        .create("books", |t| {
            t.id();
            t.string("title");
            t.big_integer("author_id").references("authors", "id");
        })
        .await
        .unwrap();

    let error = db
        .table("books")
        .insert(&db, &[("title", "Orphan".into()), ("author_id", 999.into())])
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.to_lowercase().contains("foreign key"),
        "a row pointing at a missing author was accepted: {error}"
    );
}

#[tokio::test]
async fn null_is_null_and_not_an_empty_string() {
    let db = db().await;
    Schema::new(&db)
        .create("readings", |t| {
            t.id();
            t.string("label");
            t.big_integer("value").nullable();
        })
        .await
        .unwrap();

    db.table("readings")
        .insert(&db, &[("label", "missing".into()), ("value", Value::Null)])
        .await
        .unwrap();

    let row = db.table("readings").filter_null("value").first(&db).await.unwrap();
    assert!(row.is_some(), "a null was not stored as a null");
}

/// Binary and text are different SQLite storage classes, and the reader keys
/// on what the value *is* rather than what the column was declared as.
#[tokio::test]
async fn blobs_and_text_come_back_as_what_they_went_in_as() {
    let db = db().await;
    Schema::new(&db)
        .create("files", |t| {
            t.id();
            t.string("name");
            t.binary("body");
        })
        .await
        .unwrap();

    let bytes = vec![0u8, 159, 146, 150, 255];
    db.table("files")
        .insert(&db, &[("name", "logo.png".into()), ("body", bytes.clone().into())])
        .await
        .unwrap();

    let row = db.table("files").first(&db).await.unwrap().unwrap();
    assert_eq!(row.get::<Vec<u8>>("body").unwrap(), bytes, "the blob was mangled on the way back");
    assert_eq!(row.get::<String>("name").unwrap(), "logo.png");
}

#[tokio::test]
async fn a_file_backed_database_persists_across_connections() {
    let dir = std::env::temp_dir().join(format!("rustlavel-sqlite-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("app.db");
    let _ = std::fs::remove_file(&path);
    let url = format!("sqlite://{}", path.display());

    {
        let db = Database::connect(&url).await.expect("opens");
        Schema::new(&db)
            .create("kept", |t| {
                t.id();
                t.string("what");
            })
            .await
            .unwrap();
        db.table("kept").insert(&db, &[("what", "written once".into())]).await.unwrap();
    }

    let db = Database::connect(&url).await.expect("reopens");
    let row = db.table("kept").first(&db).await.unwrap().expect("the row is still there");
    assert_eq!(row.get::<String>("what").unwrap(), "written once");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_url_forms_parse_to_the_paths_they_name() {
    assert_eq!(DatabaseConfig::from_url("sqlite://:memory:").unwrap().database, ":memory:");
    assert_eq!(DatabaseConfig::from_url("sqlite://data/app.db").unwrap().database, "data/app.db");
    // One extra slash is the difference between relative and absolute.
    assert_eq!(DatabaseConfig::from_url("sqlite:///var/lib/app.db").unwrap().database, "/var/lib/app.db");

    let config = DatabaseConfig::from_url("sqlite://:memory:").unwrap();
    assert_eq!(config.driver, "sqlite");
    assert!(config.host.is_empty(), "a file has no host");
    assert_eq!(config.port, 0);
}
