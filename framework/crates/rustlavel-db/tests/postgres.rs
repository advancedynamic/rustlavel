//! Integration tests against a real PostgreSQL server.
//!
//! They run only when `DATABASE_URL` is set, so `cargo test` stays green on a
//! machine with no database. Start one with:
//!
//! ```text
//! docker run -d --name rustlavel-pg -e POSTGRES_PASSWORD=secret \
//!   -e POSTGRES_USER=rustlavel -e POSTGRES_DB=rustlavel_test \
//!   -p 55432:5432 postgres:16
//! export DATABASE_URL=postgres://rustlavel:secret@127.0.0.1:55432/rustlavel_test
//! ```

use rustlavel_db::migration;
use rustlavel_db::prelude::*;
use rustlavel_db::{Migration, Value, migration::MigrationReport};

/// Skip the test when no database is configured, saying so out loud.
macro_rules! database {
    () => {
        match std::env::var("DATABASE_URL") {
            Ok(url) if !url.is_empty() => match Database::connect(&url).await {
                Ok(db) => db,
                Err(e) => panic!("DATABASE_URL is set but connecting failed: {e}"),
            },
            _ => {
                eprintln!("skipped: set DATABASE_URL to run the PostgreSQL integration tests");
                return;
            }
        }
    };
}

/// Each test owns a uniquely named table, so they can run concurrently against
/// one database without stepping on each other.
async fn fresh_table(db: &Database, name: &str) -> String {
    let table = format!("t_{name}");
    db.run(&format!("drop table if exists {table} cascade")).await.unwrap();
    table
}

#[tokio::test]
async fn connects_and_runs_a_query() {
    let db = database!();

    let answer = db.scalar::<i64>("select 40 + $1", &[Value::from(2)]).await.unwrap();
    assert_eq!(answer, Some(42));
}

#[tokio::test]
async fn round_trips_every_supported_type() {
    let db = database!();

    let row = db
        .select_one(
            "select $1::boolean as flag, $2::bigint as count, $3::double precision as ratio, \
             $4::text as label, $5::jsonb as payload, $6::bytea as blob, null::text as missing",
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

    assert!(row.get::<bool>("flag").unwrap());
    assert_eq!(row.get::<i64>("count").unwrap(), 9_000_000_000);
    assert_eq!(row.get::<f64>("ratio").unwrap(), 1.5);
    assert_eq!(row.get::<String>("label").unwrap(), "hello");
    assert_eq!(row.get::<Json>("payload").unwrap().get("a.1").unwrap().as_i64(), Some(2));
    assert_eq!(row.get::<Vec<u8>>("blob").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(row.get::<Option<String>>("missing").unwrap(), None);
}

#[tokio::test]
async fn a_parameter_can_never_become_sql() {
    let db = database!();
    let table = fresh_table(&db, "injection").await;

    db.run(&format!("create table {table} (id bigserial primary key, name text not null)"))
        .await
        .unwrap();

    let hostile = "'; drop table ".to_string() + &table + "; --";
    db.table(&table).insert_without_id(&db, &[("name", Value::from(hostile.as_str()))]).await.unwrap();

    // The table still exists and holds the string verbatim.
    let stored = db.scalar::<String>(&format!("select name from {table}"), &[]).await.unwrap();
    assert_eq!(stored.as_deref(), Some(hostile.as_str()));
}

#[tokio::test]
async fn the_query_builder_reads_and_writes() {
    let db = database!();
    let table = fresh_table(&db, "builder").await;

    Schema::new(&db)
        .create(&table, |t| {
            t.id();
            t.string("name");
            t.integer("age");
            t.boolean("active").default_bool(true);
        })
        .await
        .unwrap();

    for (name, age) in [("Ada", 36), ("Grace", 45), ("Alan", 41)] {
        db.table(&table)
            .insert(&db, &[("name", Value::from(name)), ("age", Value::from(age))])
            .await
            .unwrap();
    }

    let adults = db.table(&table).filter_op("age", ">", 40).latest("age").get(&db).await.unwrap();
    assert_eq!(adults.len(), 2);
    assert_eq!(adults[0].get::<String>("name").unwrap(), "Grace");

    assert_eq!(db.table(&table).count(&db).await.unwrap(), 3);
    assert!(db.table(&table).filter("name", "Ada").exists(&db).await.unwrap());

    let updated = db
        .table(&table)
        .filter("name", "Ada")
        .update(&db, &[("age", Value::from(37))])
        .await
        .unwrap();
    assert_eq!(updated, 1);

    let deleted = db.table(&table).filter("name", "Alan").delete(&db).await.unwrap();
    assert_eq!(deleted, 1);
    assert_eq!(db.table(&table).count(&db).await.unwrap(), 2);
}

#[tokio::test]
async fn a_transaction_commits_or_rolls_back_as_a_unit() {
    let db = database!();
    let table = fresh_table(&db, "tx").await;

    db.run(&format!("create table {table} (id bigserial primary key, name text not null)"))
        .await
        .unwrap();

    // Abandoned without committing: nothing is written.
    {
        let mut tx = db.begin().await.unwrap();
        tx.execute(&format!("insert into {table} (name) values ($1)"), &[Value::from("first")])
            .await
            .unwrap();
        tx.rollback().await.unwrap();
    }
    assert_eq!(db.table(&table).count(&db).await.unwrap(), 0);

    // Committed: it is.
    let mut tx = db.begin().await.unwrap();
    tx.execute(&format!("insert into {table} (name) values ($1)"), &[Value::from("kept")])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(db.table(&table).count(&db).await.unwrap(), 1);
}

#[tokio::test]
async fn dropping_a_transaction_rolls_it_back() {
    let db = database!();
    let table = fresh_table(&db, "txdrop").await;

    db.run(&format!("create table {table} (id bigserial primary key, name text not null)"))
        .await
        .unwrap();

    {
        let mut tx = db.begin().await.unwrap();
        tx.execute(&format!("insert into {table} (name) values ($1)"), &[Value::from("ghost")])
            .await
            .unwrap();
        // No commit: the guard rolls back when it goes out of scope.
    }

    // The rollback is spawned, so give it a moment to reach the server.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(db.table(&table).count(&db).await.unwrap(), 0);
}

#[tokio::test]
async fn a_savepoint_undoes_only_part_of_a_transaction() {
    let db = database!();
    let table = fresh_table(&db, "savepoint").await;

    db.run(&format!("create table {table} (id bigserial primary key, name text not null)"))
        .await
        .unwrap();

    let mut tx = db.begin().await.unwrap();
    tx.execute(&format!("insert into {table} (name) values ($1)"), &[Value::from("keep")])
        .await
        .unwrap();
    tx.savepoint("sp1").await.unwrap();
    tx.execute(&format!("insert into {table} (name) values ($1)"), &[Value::from("discard")])
        .await
        .unwrap();
    tx.rollback_to("sp1").await.unwrap();
    tx.commit().await.unwrap();

    let names = db.table(&table).get(&db).await.unwrap();
    assert_eq!(names.len(), 1);
    assert_eq!(names[0].get::<String>("name").unwrap(), "keep");
}

// --- Migrations ---

migration!(
    CreateWidgets,
    "2026_08_29_000001_create_widgets_table",
    up: |schema| {
        schema
            .create("m_widgets", |t| {
                t.id();
                t.string("name").unique();
                t.timestamps();
            })
            .await
    },
    down: |schema| { schema.drop("m_widgets").await },
);

#[tokio::test]
async fn migrations_apply_are_idempotent_and_roll_back() {
    let db = database!();
    db.run("drop table if exists m_widgets cascade").await.unwrap();
    db.run("delete from rustlavel_migrations where name like '2026_08_29_000001%'")
        .await
        .ok();

    let migrations: Vec<&dyn Migration> = vec![&CreateWidgets];
    let migrator = Migrator::new(&db, migrations);

    let report = migrator.run().await.unwrap();
    assert_eq!(report.applied, vec!["2026_08_29_000001_create_widgets_table"]);
    assert!(Schema::new(&db).has_table("m_widgets").await.unwrap());

    // Running again does nothing, which is what makes deploys safe to repeat.
    let again = migrator.run().await.unwrap();
    assert_eq!(again, MigrationReport { applied: vec![], rolled_back: vec![], skipped: 1 });

    let rolled = migrator.rollback().await.unwrap();
    assert_eq!(rolled.rolled_back, vec!["2026_08_29_000001_create_widgets_table"]);
    assert!(!Schema::new(&db).has_table("m_widgets").await.unwrap());
}

// --- The ORM ---

#[derive(Model, Default, Debug, Clone, PartialEq)]
#[model(table = "orm_authors")]
struct Author {
    #[model(primary_key, generated)]
    id: i64,
    name: String,
    email: Option<String>,
}

#[tokio::test]
async fn a_derived_model_can_be_created_read_updated_and_deleted() {
    let db = database!();
    db.run("drop table if exists orm_authors cascade").await.unwrap();

    Schema::new(&db)
        .create("orm_authors", |t| {
            t.id();
            t.string("name");
            t.string("email").nullable();
        })
        .await
        .unwrap();

    let mut author = Author { name: "Ada".into(), email: Some("ada@example.com".into()), ..Author::default() };
    author.insert(&db).await.unwrap();
    assert!(author.id > 0, "the database should have assigned an id");

    let loaded = Author::find(&db, author.id).await.unwrap().expect("the author exists");
    assert_eq!(loaded, author);

    author.name = "Ada Lovelace".into();
    assert_eq!(author.update(&db).await.unwrap(), 1);
    assert_eq!(Author::find_or_fail(&db, author.id).await.unwrap().name, "Ada Lovelace");

    assert_eq!(Author::count(&db).await.unwrap(), 1);

    author.delete(&db).await.unwrap();
    assert!(Author::find(&db, author.id).await.unwrap().is_none());

    let missing = Author::find_or_fail(&db, 999_999).await.unwrap_err().to_string();
    assert!(missing.contains("no orm_authors with id"));
}

/// A second pair of tables, so the relation test never races the CRUD test for
/// the same schema objects while both run concurrently.
#[derive(Model, Default, Debug, Clone)]
#[model(table = "rel_authors")]
struct RelAuthor {
    #[model(primary_key, generated)]
    id: i64,
    name: String,
}

#[derive(Model, Default, Debug, Clone)]
#[model(table = "rel_books")]
struct RelBook {
    #[model(primary_key, generated)]
    id: i64,
    author_id: i64,
    title: String,
}

#[tokio::test]
async fn relations_load_without_an_n_plus_one() {
    let db = database!();
    db.run("drop table if exists rel_books cascade").await.unwrap();
    db.run("drop table if exists rel_authors cascade").await.unwrap();

    let schema = Schema::new(&db);
    schema
        .create("rel_authors", |t| {
            t.id();
            t.string("name");
        })
        .await
        .unwrap();
    schema
        .create("rel_books", |t| {
            t.id();
            t.big_integer("author_id").references("rel_authors", "id").cascade_on_delete();
            t.string("title");
        })
        .await
        .unwrap();

    let mut ada = RelAuthor { name: "Ada".into(), ..RelAuthor::default() };
    ada.insert(&db).await.unwrap();
    let mut grace = RelAuthor { name: "Grace".into(), ..RelAuthor::default() };
    grace.insert(&db).await.unwrap();

    for title in ["Notes", "Sketch"] {
        let mut book = RelBook { author_id: ada.id, title: title.into(), ..RelBook::default() };
        book.insert(&db).await.unwrap();
    }
    let mut cobol = RelBook { author_id: grace.id, title: "COBOL".into(), ..RelBook::default() };
    cobol.insert(&db).await.unwrap();

    let authors = RelAuthor::all(&db).await.unwrap();
    // Two queries in total, however many authors there are.
    let books = has_many::<RelAuthor, RelBook>(&db, &authors, "author_id").await.unwrap();

    assert_eq!(books.len(), 2);
    assert_eq!(books[0].len(), 2);
    assert_eq!(books[1].len(), 1);
    assert_eq!(books[1][0].title, "COBOL");

    let all_books = RelBook::all(&db).await.unwrap();
    let owners = belongs_to::<RelBook, RelAuthor>(&db, &all_books, "author_id").await.unwrap();

    assert_eq!(owners.len(), 3);
    assert!(owners.iter().all(Option::is_some));
    assert_eq!(owners[2].as_ref().unwrap().name, "Grace");
}

#[tokio::test]
async fn the_schema_builder_creates_real_tables() {
    let db = database!();
    let table = fresh_table(&db, "schema").await;

    let schema = Schema::new(&db);
    schema
        .create(&table, |t| {
            t.id();
            t.string("email").unique();
            t.decimal("balance", 12, 2).default_raw("0");
            t.json("settings").nullable();
            t.timestamps();
            t.soft_deletes();
        })
        .await
        .unwrap();

    assert!(schema.has_table(&table).await.unwrap());
    assert!(schema.has_column(&table, "deleted_at").await.unwrap());
    assert!(!schema.has_column(&table, "nickname").await.unwrap());

    schema.alter(&table, |t| { t.string("nickname").nullable(); }).await.unwrap();
    assert!(schema.has_column(&table, "nickname").await.unwrap());

    // The unique index is real: a duplicate is rejected by the database.
    db.table(&table).insert_without_id(&db, &[("email", Value::from("a@b.com"))]).await.unwrap();
    let duplicate =
        db.table(&table).insert_without_id(&db, &[("email", Value::from("a@b.com"))]).await;
    assert!(duplicate.is_err());

    schema.drop(&table).await.unwrap();
    assert!(!schema.has_table(&table).await.unwrap());
}

#[tokio::test]
async fn a_broken_statement_reports_the_sql() {
    let db = database!();

    let error = db.select("select * from a_table_that_is_not_there", &[]).await.unwrap_err();
    let text = error.to_string();

    assert!(text.contains("42P01"), "should carry the SQLSTATE: {text}");
    assert!(text.contains("SQL: select * from a_table_that_is_not_there"));
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
