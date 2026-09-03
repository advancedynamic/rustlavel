//! The cache against a real database.
//!
//! The unit tests prove the plumbing: keys, fingerprints, the JSON round trip.
//! None of them can tell you the thing that actually matters — that a second
//! read does not reach the database, and that a write makes it reach the
//! database again. That needs a database and a way to count queries, which is
//! what these do.
//!
//! They run only when `DATABASE_URL` is set:
//!
//! ```text
//! docker run -d --name rustlavel-mc-pg -e POSTGRES_PASSWORD=secret \
//!   -e POSTGRES_USER=rustlavel -e POSTGRES_DB=rustlavel_test \
//!   -p 5432:5432 postgres:16-alpine
//! export DATABASE_URL='postgres://rustlavel:secret@127.0.0.1:5432/rustlavel_test'
//! cargo test -p rustlavel-model-cache
//! ```
//!
//! Nothing here is PostgreSQL-specific — the fixtures go through the schema
//! and query builders — so the same suite is worth pointing at the other two,
//! and it has been. All six pass unchanged against PostgreSQL 16, MySQL 8.4
//! and Azure SQL Edge.
//!
//! That is what caught the one genuinely portable-looking bug: MySQL has no
//! boolean, so a `bool` field came back as an integer and no model with a flag
//! on it could be read there at all. The fix is in `FromValue for bool`, not
//! here.
//!
//! Every test owns a table named for itself, because the suite runs
//! concurrently and two tests sharing one would pass or fail depending on
//! which finished first.

use rustlavel_cache::MemoryStore;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use rustlavel_db::prelude::*;
use rustlavel_db::schema::Schema;
use rustlavel_db::{Database, Value};
use rustlavel_model_cache::{ModelCache, Region};
use std::time::Duration;

macro_rules! database_url {
    () => {
        match std::env::var("DATABASE_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => {
                eprintln!("skipping: DATABASE_URL is not set");
                return;
            }
        }
    };
}

#[derive(Model, Default, Debug, Clone, PartialEq)]
#[model(table = "mc_widgets", crate = "rustlavel_db")]
struct Widget {
    #[model(primary_key, generated)]
    id: i64,
    name: String,
    kind: String,
}

/// A table of this name, empty, with three rows in it.
///
/// Built through the schema and query builders rather than raw SQL, so the
/// same fixture is correct on PostgreSQL, MySQL and SQL Server — and so the
/// key is generated the way the ORM assumes it is.
async fn fixture(url: &str, table: &str) -> Database {
    let db = Database::connect(url).await.expect("DATABASE_URL should be reachable");
    let schema = Schema::new(&db);

    // Dropped first as well as created, so a run that panicked half way
    // through last time does not poison the next one.
    schema.drop(table).await.ok();
    schema
        .create(table, |t| {
            t.id();
            t.string("name");
            t.string("kind");
        })
        .await
        .expect("creating the fixture table");

    for (name, kind) in [("one", "a"), ("two", "a"), ("three", "b")] {
        db.table(table)
            .insert(&db, &[("name", Value::from(name)), ("kind", Value::from(kind))])
            .await
            .expect("seeding");
    }
    db
}

fn cache() -> ModelCache {
    ModelCache::new(MemoryStore::new()).region::<Widget>(Region::new().ttl(Duration::from_secs(60)))
}

/// The second `find` must not reach the database. Proved by deleting the row
/// out from under the cache: a read that still returns it came from the cache,
/// and a read that returns nothing did not.
#[tokio::test]
async fn a_second_find_is_served_from_the_cache() {
    let url = database_url!();
    let db = fixture(&url, "mc_widgets").await;
    let cache = cache();

    let one = cache
        .first::<Widget>(&db, Widget::query().filter("name", "one"))
        .await
        .expect("finding the first row")
        .expect("row `one` exists");
    let id = one.id;
    cache.stats().reset();

    let first = cache.find::<Widget>(&db, id).await.expect("first read").expect("row exists");
    assert_eq!(first.name, "one");
    assert_eq!(cache.stats().for_table("mc_widgets").entity_misses, 1);

    // Behind the cache's back, so only the cache can still have it.
    db.table("mc_widgets").filter("id", id).delete(&db).await.expect("deleting");

    let second = cache.find::<Widget>(&db, id).await.expect("second read");
    assert_eq!(second.map(|w| w.name), Some("one".to_string()), "the second read hit the database");
    assert_eq!(cache.stats().for_table("mc_widgets").entity_hits, 1);

    // And once the entity is dropped, the truth comes back.
    cache.forget::<Widget>(id).await.expect("forgetting");
    assert!(cache.find::<Widget>(&db, id).await.expect("third read").is_none());
}

/// The half that is hard: a write to *any* row makes every cached query for
/// the table a miss, without the cache knowing which queries exist.
#[tokio::test]
async fn a_write_invalidates_every_cached_query_for_the_table() {
    let url = database_url!();
    let db = fixture(&url, "mc_widgets2").await;
    let cache = ModelCache::new(MemoryStore::new())
        .region::<Widget2>(Region::new().ttl(Duration::from_secs(60)));

    let kind_a = Widget2::query().filter("kind", "a");
    let first = cache.get::<Widget2>(&db, kind_a.clone()).await.expect("first query");
    assert_eq!(first.len(), 2);

    // Served from the cache: the row is gone from the database and still here.
    db.table("mc_widgets2").filter("name", "two").delete(&db).await.expect("deleting");
    let second = cache.get::<Widget2>(&db, kind_a.clone()).await.expect("second query");
    assert_eq!(second.len(), 2, "the second query did not hit the cache");
    assert_eq!(cache.stats().for_table("mc_widgets2").query_hits, 1);

    // A write through the cache bumps the generation, and the same query —
    // which the cache has never been told anything about — is a miss.
    let mut fresh = Widget2 { id: 0, name: "nine".into(), kind: "a".into() };
    cache.insert(&db, &mut fresh).await.expect("inserting");

    let third = cache.get::<Widget2>(&db, kind_a).await.expect("third query");
    assert_eq!(third.len(), 2, "one deleted, one added: {third:?}");
    assert!(third.iter().any(|w| w.name == "nine"), "the new row is missing: {third:?}");
    assert_eq!(cache.stats().for_table("mc_widgets2").query_misses, 2);
}

#[derive(Model, Default, Debug, Clone, PartialEq)]
#[model(table = "mc_widgets2", crate = "rustlavel_db")]
struct Widget2 {
    #[model(primary_key, generated)]
    id: i64,
    name: String,
    kind: String,
}

/// Two different filters must not share a cache entry.
#[tokio::test]
async fn two_queries_that_differ_only_in_a_binding_do_not_share_an_entry() {
    let url = database_url!();
    let db = fixture(&url, "mc_widgets3").await;
    let cache = ModelCache::new(MemoryStore::new())
        .region::<Widget3>(Region::new().ttl(Duration::from_secs(60)));

    let a = cache.get::<Widget3>(&db, Widget3::query().filter("kind", "a")).await.expect("a");
    let b = cache.get::<Widget3>(&db, Widget3::query().filter("kind", "b")).await.expect("b");

    assert_eq!(a.len(), 2);
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].name, "three");

    // And each is still itself on the way back out of the cache.
    let a_again = cache.get::<Widget3>(&db, Widget3::query().filter("kind", "a")).await.expect("a");
    assert_eq!(a_again.len(), 2, "the two filters shared an entry");
    assert_eq!(cache.stats().for_table("mc_widgets3").query_hits, 1);
}

#[derive(Model, Default, Debug, Clone, PartialEq)]
#[model(table = "mc_widgets3", crate = "rustlavel_db")]
struct Widget3 {
    #[model(primary_key, generated)]
    id: i64,
    name: String,
    kind: String,
}

/// A model with no region is not cached at all, which is what makes the
/// package safe to register in an application that has not thought about it.
#[tokio::test]
async fn an_unregistered_model_is_not_cached() {
    let url = database_url!();
    let db = fixture(&url, "mc_widgets4").await;
    let cache = ModelCache::new(MemoryStore::new());

    let one = cache
        .first::<Widget4>(&db, Widget4::query().filter("name", "one"))
        .await
        .expect("finding")
        .expect("row `one` exists");

    let first = cache.find::<Widget4>(&db, one.id).await.expect("first").expect("row");
    assert_eq!(first.name, "one");

    db.table("mc_widgets4").filter("id", one.id).delete(&db).await.expect("deleting");

    let second = cache.find::<Widget4>(&db, one.id).await.expect("second");
    assert!(second.is_none(), "an unregistered model was cached anyway");
    assert_eq!(cache.stats().for_table("mc_widgets4").entity_hits, 0);
}

#[derive(Model, Default, Debug, Clone, PartialEq)]
#[model(table = "mc_widgets4", crate = "rustlavel_db")]
struct Widget4 {
    #[model(primary_key, generated)]
    id: i64,
    name: String,
    kind: String,
}

/// A result set over the row limit runs and is not stored, so one report does
/// not push everything else out of Redis.
#[tokio::test]
async fn a_result_set_over_the_limit_is_not_stored() {
    let url = database_url!();
    let db = fixture(&url, "mc_widgets5").await;
    let cache = ModelCache::new(MemoryStore::new())
        .region::<Widget5>(Region::new().max_rows(1));

    let all = cache.get::<Widget5>(&db, Widget5::query()).await.expect("first");
    assert_eq!(all.len(), 3);
    assert_eq!(cache.stats().for_table("mc_widgets5").too_large, 1);

    db.table("mc_widgets5").filter("name", "one").delete(&db).await.expect("deleting");
    let again = cache.get::<Widget5>(&db, Widget5::query()).await.expect("second");
    assert_eq!(again.len(), 2, "an over-large result set was cached after all");
}

#[derive(Model, Default, Debug, Clone, PartialEq)]
#[model(table = "mc_widgets5", crate = "rustlavel_db")]
struct Widget5 {
    #[model(primary_key, generated)]
    id: i64,
    name: String,
    kind: String,
}

/// The `Value` variants have to survive the cache exactly, or a cached row
/// hands the caller a different type than the database did.
#[tokio::test]
async fn every_column_type_comes_back_as_itself() {
    let url = database_url!();
    let db = Database::connect(&url).await.expect("reachable");

    let schema = Schema::new(&db);
    schema.drop("mc_types").await.ok();
    schema
        .create("mc_types", |t| {
            t.id();
            t.string("name");
            t.float("score");
            t.boolean("ok");
            t.string("missing").nullable();
        })
        .await
        .expect("creating");

    let id = db
        .table("mc_types")
        .insert(
            &db,
            &[
                ("name", Value::from("ada")),
                ("score", Value::from(1.5f64)),
                ("ok", Value::from(true)),
                ("missing", Value::Null),
            ],
        )
        .await
        .expect("seeding");

    let cache = ModelCache::new(MemoryStore::new())
        .region::<Typed>(Region::new().ttl(Duration::from_secs(60)));

    let direct = cache.find::<Typed>(&db, id).await.expect("first").expect("row");
    // Second read comes from the cache, and must be identical.
    let cached = cache.find::<Typed>(&db, id).await.expect("second").expect("row");

    assert_eq!(cache.stats().for_table("mc_types").entity_hits, 1, "the second read was not a hit");
    assert_eq!(direct, cached, "the cached row differs from the database's");
    assert_eq!(cached.score, 1.5);
    assert!(cached.ok);
    assert_eq!(cached.missing, None);
}

#[derive(Model, Default, Debug, Clone, PartialEq)]
#[model(table = "mc_types", crate = "rustlavel_db")]
struct Typed {
    #[model(primary_key, generated)]
    id: i64,
    name: String,
    score: f64,
    ok: bool,
    missing: Option<String>,
}


/// **A cache miss must cost one query, not two.**
///
/// It cost two: `find` called `Model::find` and then read the row again to
/// cache it, because `Model::to_json` cannot be rehydrated — it drops the
/// columns the database maintains. That made a cold read more expensive than
/// no cache at all, which is the one thing a cache may not do.
///
/// Counted off the framework's own `db.query` events, filtered to this test's
/// table so the concurrent suite does not contaminate the count.
#[tokio::test]
async fn a_cold_read_costs_one_query_and_a_warm_read_costs_none() {
    let url = database_url!();
    let db = fixture(&url, "mc_widgets6").await;
    let cache = ModelCache::new(MemoryStore::new())
        .region::<Widget6>(Region::new().ttl(Duration::from_secs(60)));

    let one = cache
        .first::<Widget6>(&db, Widget6::query().filter("name", "one"))
        .await
        .expect("finding")
        .expect("row `one` exists");

    // Subscribed after the fixture, so only the reads below are counted.
    let queries = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&queries);
    rustlavel_core::events::subscribe(move |event: &rustlavel_core::Event| {
        let touches_this_table = event
            .fields
            .get("sql")
            .and_then(rustlavel_core::Json::as_str)
            .is_some_and(|sql| sql.contains("mc_widgets6"));
        if event.kind == "db.query" && touches_this_table {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });

    let cold = cache.find::<Widget6>(&db, one.id).await.expect("cold").expect("row");
    assert_eq!(cold.name, "one");
    assert_eq!(queries.load(Ordering::SeqCst), 1, "a cache miss ran more than one query");

    let warm = cache.find::<Widget6>(&db, one.id).await.expect("warm").expect("row");
    assert_eq!(warm.name, "one");
    assert_eq!(queries.load(Ordering::SeqCst), 1, "a cache hit reached the database");
    assert_eq!(cache.stats().for_table("mc_widgets6").entity_hits, 1);
}

#[derive(Model, Default, Debug, Clone, PartialEq)]
#[model(table = "mc_widgets6", crate = "rustlavel_db")]
struct Widget6 {
    #[model(primary_key, generated)]
    id: i64,
    name: String,
    kind: String,
}
