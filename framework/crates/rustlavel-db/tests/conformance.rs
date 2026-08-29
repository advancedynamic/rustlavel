//! Does the SQL we generate actually work?
//!
//! The dialect tests assert what the strings look like. These run those exact
//! strings against a real server, which is a different question — and the only
//! one that matters. Every check here exists because a live database rejected
//! something the unit tests were perfectly happy with:
//!
//! - MySQL refuses an `auto_increment` column that is not a key.
//! - SQL Server's `OUTPUT` goes after the column list, not before it.
//! - SQL Server rejects `add column` while requiring `drop column`.
//! - MySQL parses an inline `references` clause and silently creates no
//!   foreign key at all, then accepts orphan rows.
//!
//! Set any of `DATABASE_URL`, `MYSQL_URL` or `SQLSERVER_URL` and the matching
//! database is checked; unset ones are skipped with a note.

use rustlavel_db::prelude::*;
use rustlavel_db::{Migration, Migrator, migration};

/// Every configured database, with a prefix so two of them can share a server.
async fn databases() -> Vec<(String, Database)> {
    let mut out = Vec::new();

    for variable in ["DATABASE_URL", "MYSQL_URL", "SQLSERVER_URL"] {
        let Ok(url) = std::env::var(variable) else {
            eprintln!("skipped {variable}: not set");
            continue;
        };
        if url.is_empty() {
            continue;
        }

        match Database::connect(&url).await {
            Ok(db) => out.push((db.dialect().name().to_string(), db)),
            Err(e) => panic!("{variable} is set but connecting failed: {e}"),
        }
    }

    if out.is_empty() {
        eprintln!("skipped: set DATABASE_URL, MYSQL_URL or SQLSERVER_URL to run the conformance tests");
    }
    out
}

/// A table name nothing else in the suite uses.
fn table(test: &str, dialect: &str) -> String {
    format!("conf_{test}_{dialect}")
}

async fn drop_if_present(db: &Database, name: &str) {
    // Straight through the dialect, so this is itself a check that dropping works.
    let sql = db.dialect().drop_table_sql(name);
    db.run(&sql).await.ok();
}

#[tokio::test]
async fn the_generated_create_table_is_accepted_and_round_trips() {
    for (dialect, db) in databases().await {
        let name = table("create", &dialect);
        drop_if_present(&db, &name).await;

        Schema::new(&db)
            .create(&name, |t| {
                t.id();
                t.string("email").unique();
                t.text("bio").nullable();
                t.integer("visits").default_int(0);
                t.boolean("active").default_bool(true);
                t.decimal("balance", 12, 2).default_int(0);
                t.timestamps();
            })
            .await
            .unwrap_or_else(|e| panic!("{dialect}: create failed: {e}"));

        db.table(&name)
            .insert_without_id(&db, &[("email", Value::from("ada@example.com"))])
            .await
            .unwrap_or_else(|e| panic!("{dialect}: insert failed: {e}"));

        let rows = db.table(&name).get(&db).await.unwrap();
        assert_eq!(rows.len(), 1, "{dialect}");
        assert_eq!(rows[0].get::<String>("email").unwrap(), "ada@example.com", "{dialect}");

        // The defaults have to have been applied by the database, not by us.
        assert_eq!(rows[0].get::<i64>("visits").unwrap(), 0, "{dialect}");
        assert!(!rows[0].value("created_at").unwrap().is_null(), "{dialect}: no timestamp");

        drop_if_present(&db, &name).await;
    }
}

#[tokio::test]
async fn a_unique_constraint_is_real() {
    for (dialect, db) in databases().await {
        let name = table("unique", &dialect);
        drop_if_present(&db, &name).await;

        Schema::new(&db)
            .create(&name, |t| {
                t.id();
                t.string("email").unique();
            })
            .await
            .unwrap();

        let insert = |value: &'static str| {
            let db = db.clone();
            let name = name.clone();
            async move {
                db.table(&name).insert_without_id(&db, &[("email", Value::from(value))]).await
            }
        };

        insert("taken@example.com").await.unwrap();
        assert!(
            insert("taken@example.com").await.is_err(),
            "{dialect}: a duplicate was accepted, so the constraint is not there"
        );

        drop_if_present(&db, &name).await;
    }
}

#[tokio::test]
async fn a_foreign_key_is_enforced_rather_than_merely_written() {
    // The one that matters most: MySQL parses an inline `references` clause,
    // creates nothing, and accepts orphans. Nothing but a real insert catches it.
    for (dialect, db) in databases().await {
        let children = table("fk_children", &dialect);
        let parents = table("fk_parent", &dialect);

        drop_if_present(&db, &children).await;
        drop_if_present(&db, &parents).await;

        let schema = Schema::new(&db);
        schema
            .create(&parents, |t| {
                t.id();
                t.string("name");
            })
            .await
            .unwrap();

        // `foreign_id` names the table by pluralising, so the column is built
        // by hand here to point at this test's own parent table.
        schema
            .create(&children, |t| {
                t.id();
                t.big_integer("parent_id").references(&parents, "id").cascade_on_delete();
            })
            .await
            .unwrap_or_else(|e| panic!("{dialect}: create failed: {e}"));

        let orphan = db
            .table(&children)
            .insert_without_id(&db, &[("parent_id", Value::from(999_999i64))])
            .await;

        assert!(
            orphan.is_err(),
            "{dialect}: a row pointing at a parent that does not exist was accepted — \
             the foreign key was written but not created"
        );

        drop_if_present(&db, &children).await;
        drop_if_present(&db, &parents).await;
    }
}

#[tokio::test]
async fn a_generated_key_comes_back_from_an_insert() {
    // Three different mechanisms: RETURNING, OUTPUT, and last_insert_id.
    for (dialect, db) in databases().await {
        let name = table("keys", &dialect);
        drop_if_present(&db, &name).await;

        Schema::new(&db)
            .create(&name, |t| {
                t.id();
                t.string("name");
            })
            .await
            .unwrap();

        let first =
            db.table(&name).insert(&db, &[("name", Value::from("one"))]).await.unwrap_or_else(
                |e| panic!("{dialect}: insert did not return a key: {e}"),
            );
        let second =
            db.table(&name).insert(&db, &[("name", Value::from("two"))]).await.unwrap();

        assert!(first > 0, "{dialect}: the key was {first}");
        assert!(second > first, "{dialect}: keys did not advance ({first} then {second})");

        drop_if_present(&db, &name).await;
    }
}

#[tokio::test]
async fn a_column_can_be_added_and_dropped() {
    for (dialect, db) in databases().await {
        let name = table("alter", &dialect);
        drop_if_present(&db, &name).await;

        let schema = Schema::new(&db);
        schema
            .create(&name, |t| {
                t.id();
                t.string("keep");
            })
            .await
            .unwrap();

        schema
            .alter(&name, |t| {
                t.string("nickname").nullable();
            })
            .await
            .unwrap_or_else(|e| panic!("{dialect}: adding a column failed: {e}"));
        assert!(schema.has_column(&name, "nickname").await.unwrap(), "{dialect}");

        schema
            .alter(&name, |t| {
                t.drop_column("nickname");
            })
            .await
            .unwrap_or_else(|e| panic!("{dialect}: dropping a column failed: {e}"));
        assert!(!schema.has_column(&name, "nickname").await.unwrap(), "{dialect}");

        drop_if_present(&db, &name).await;
    }
}

#[tokio::test]
async fn paging_returns_the_right_slice() {
    for (dialect, db) in databases().await {
        let name = table("paging", &dialect);
        drop_if_present(&db, &name).await;

        Schema::new(&db)
            .create(&name, |t| {
                t.id();
                t.integer("position");
            })
            .await
            .unwrap();

        for position in 1..=25 {
            db.table(&name)
                .insert_without_id(&db, &[("position", Value::from(position))])
                .await
                .unwrap();
        }

        let second = db
            .table(&name)
            .order_by("position", rustlavel_db::Direction::Asc)
            .page(2, 10)
            .get(&db)
            .await
            .unwrap_or_else(|e| panic!("{dialect}: paging failed: {e}"));

        assert_eq!(second.len(), 10, "{dialect}");
        assert_eq!(second[0].get::<i64>("position").unwrap(), 11, "{dialect}");

        // SQL Server cannot page without an ordering; the builder supplies one,
        // so this must work too.
        let unordered = db.table(&name).limit(5).get(&db).await.unwrap_or_else(|e| {
            panic!("{dialect}: paging without an order by failed: {e}")
        });
        assert_eq!(unordered.len(), 5, "{dialect}");

        drop_if_present(&db, &name).await;
    }
}

migration!(
    ConformanceMigration,
    "2026_08_29_000099_conformance_check",
    up: |schema| {
        schema
            .create("conf_migrated", |t| {
                t.id();
                t.string("name");
                t.timestamps();
            })
            .await
    },
    down: |schema| { schema.drop("conf_migrated").await },
);

#[tokio::test]
async fn migrations_run_and_roll_back_on_every_database() {
    for (dialect, db) in databases().await {
        let tracking = format!("conf_migrations_{dialect}");

        // Both tables go first. Leaving the tracking table behind would make a
        // second run of this test see the migration as already applied, and
        // then find nothing to roll back.
        drop_if_present(&db, "conf_migrated").await;
        drop_if_present(&db, &tracking).await;

        // Its own tracking table, so this cannot disturb another suite sharing
        // the same server.
        let migrations: Vec<&dyn Migration> = vec![&ConformanceMigration];
        let migrator = Migrator::new(&db, migrations).with_table(&tracking).unwrap();

        // The tracking table's own DDL is the first thing that has to be valid.
        migrator.prepare().await.unwrap_or_else(|e| {
            panic!("{dialect}: the migrations table could not be created: {e}")
        });

        let applied = migrator.run().await.unwrap_or_else(|e| panic!("{dialect}: migrate: {e}"));
        assert_eq!(applied.applied.len(), 1, "{dialect}");
        assert!(Schema::new(&db).has_table("conf_migrated").await.unwrap(), "{dialect}");

        // Running again must do nothing, which is what makes a deploy repeatable.
        assert!(migrator.run().await.unwrap().applied.is_empty(), "{dialect}");

        let rolled = migrator.rollback().await.unwrap_or_else(|e| panic!("{dialect}: rollback: {e}"));
        assert_eq!(rolled.rolled_back.len(), 1, "{dialect}");
        assert!(!Schema::new(&db).has_table("conf_migrated").await.unwrap(), "{dialect}");

        drop_if_present(&db, &tracking).await;
    }
}

#[tokio::test]
async fn a_hostile_value_stays_a_value_on_every_database() {
    for (dialect, db) in databases().await {
        let name = table("injection", &dialect);
        drop_if_present(&db, &name).await;

        Schema::new(&db)
            .create(&name, |t| {
                t.id();
                t.string("note");
            })
            .await
            .unwrap();

        let hostile = format!("'); drop table {name}; --");
        db.table(&name)
            .insert_without_id(&db, &[("note", Value::from(hostile.as_str()))])
            .await
            .unwrap();

        // The table is still there, and holds the string verbatim.
        let rows = db.table(&name).get(&db).await.unwrap();
        assert_eq!(rows.len(), 1, "{dialect}");
        assert_eq!(rows[0].get::<String>("note").unwrap(), hostile, "{dialect}");

        drop_if_present(&db, &name).await;
    }
}

#[tokio::test]
async fn a_transaction_commits_and_rolls_back_on_every_database() {
    // `begin` is a syntax error in T-SQL, so this went untested for exactly as
    // long as it was broken.
    for (dialect, db) in databases().await {
        let name = table("tx", &dialect);
        drop_if_present(&db, &name).await;

        Schema::new(&db)
            .create(&name, |t| {
                t.id();
                t.string("note");
            })
            .await
            .unwrap();

        let insert = format!(
            "insert into {} ({}) values ({})",
            db.dialect().quote(&name),
            db.dialect().quote("note"),
            db.dialect().placeholder(1)
        );

        // Rolled back: nothing is written.
        let mut tx = db.begin().await.unwrap_or_else(|e| panic!("{dialect}: begin failed: {e}"));
        tx.execute(&insert, &[Value::from("discarded")]).await.unwrap();
        tx.rollback().await.unwrap_or_else(|e| panic!("{dialect}: rollback failed: {e}"));
        assert_eq!(db.table(&name).count(&db).await.unwrap(), 0, "{dialect}");

        // Committed: it is.
        let mut tx = db.begin().await.unwrap();
        tx.execute(&insert, &[Value::from("kept")]).await.unwrap();
        tx.commit().await.unwrap_or_else(|e| panic!("{dialect}: commit failed: {e}"));
        assert_eq!(db.table(&name).count(&db).await.unwrap(), 1, "{dialect}");

        drop_if_present(&db, &name).await;
    }
}

#[tokio::test]
async fn a_savepoint_undoes_only_part_of_a_transaction_everywhere() {
    for (dialect, db) in databases().await {
        let name = table("savepoint", &dialect);
        drop_if_present(&db, &name).await;

        Schema::new(&db)
            .create(&name, |t| {
                t.id();
                t.string("note");
            })
            .await
            .unwrap();

        let insert = format!(
            "insert into {} ({}) values ({})",
            db.dialect().quote(&name),
            db.dialect().quote("note"),
            db.dialect().placeholder(1)
        );

        let mut tx = db.begin().await.unwrap();
        tx.execute(&insert, &[Value::from("keep")]).await.unwrap();
        tx.savepoint("sp1").await.unwrap_or_else(|e| panic!("{dialect}: savepoint failed: {e}"));
        tx.execute(&insert, &[Value::from("discard")]).await.unwrap();
        tx.rollback_to("sp1")
            .await
            .unwrap_or_else(|e| panic!("{dialect}: rollback to savepoint failed: {e}"));
        tx.commit().await.unwrap();

        let rows = db.table(&name).get(&db).await.unwrap();
        assert_eq!(rows.len(), 1, "{dialect}");
        assert_eq!(rows[0].get::<String>("note").unwrap(), "keep", "{dialect}");

        drop_if_present(&db, &name).await;
    }
}
