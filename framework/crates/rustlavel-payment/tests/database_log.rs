//! The table-backed webhook log, against a real database.
//!
//! The claim under test is that `record` is atomic: sixteen deliveries of one
//! event racing each other, and exactly one is told it was first. A read
//! followed by a write passes every single-threaded test and lets several
//! through — and several through is the customer credited several times.

#![cfg(feature = "db")]

use std::sync::Arc;

use rustlavel_core::Json;
use rustlavel_db::Database;
use rustlavel_payment::database::{DatabaseWebhookLog, drop_schema, schema};
use rustlavel_payment::{EventKind, WebhookEvent, WebhookLog};

/// Every test drops and recreates the same tables, so they take turns: the
/// lock is taken before the schema is touched and held until the test ends.
/// Sharing one URL is what makes them *not* serial on their own — ten tests
/// truncate each other's rows mid-assertion — and the contention each test
/// sets up deliberately is the thing being measured.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A database on a schema this test owns until it returns. Derefs to the
/// handle, so a test reads as though it held one.
struct Fixture {
    db: Database,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

impl std::ops::Deref for Fixture {
    type Target = Database;

    fn deref(&self) -> &Database {
        &self.db
    }
}

async fn database() -> Option<Fixture> {
    let url = std::env::var("PAYMENT_TEST_DATABASE_URL").ok()?;
    let guard = SERIAL.lock().await;
    let db = Database::connect(&url).await.expect("the test database is reachable");
    let builder = rustlavel_db::Schema::new(&db);
    let _ = drop_schema(&builder).await;
    schema(&builder).await.expect("the schema is created");
    Some(Fixture { db, _guard: guard })
}

fn event(id: &str) -> WebhookEvent {
    WebhookEvent {
        id: id.into(),
        kind: EventKind::ChargePaid,
        charge: None,
        transfer: None,
        raw: Json::object([("event_id", Json::from(id))]),
    }
}

#[tokio::test]
async fn an_event_is_recorded_once_and_its_body_kept() {
    let Some(db) = database().await else {
        println!("skipped: set PAYMENT_TEST_DATABASE_URL to run the database log tests");
        return;
    };
    let log = DatabaseWebhookLog::new(db.clone(), "fake");

    let e = event("evt_1");
    assert!(log.record(&e).await.unwrap());
    assert!(!log.record(&e).await.unwrap(), "a duplicate was recorded");
    assert_eq!(log.raw(&e.dedup_key()).await.unwrap(), Some(e.raw.clone()));

    log.withdraw(&e.dedup_key()).await.unwrap();
    assert!(log.record(&e).await.unwrap(), "a withdrawn event could not be recorded again");
}

/// The test this file exists for.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn only_one_of_many_simultaneous_deliveries_is_first() {
    let Some(db) = database().await else {
        println!("skipped: set PAYMENT_TEST_DATABASE_URL to run the database log tests");
        return;
    };
    let log = Arc::new(DatabaseWebhookLog::new(db.clone(), "fake"));

    let mut racing = Vec::new();
    for _ in 0..16 {
        let log = Arc::clone(&log);
        racing.push(tokio::spawn(async move { log.record(&event("evt_race")).await }));
    }

    let mut first = 0;
    for task in racing {
        if task.await.expect("finished").expect("the log answered") {
            first += 1;
        }
    }
    assert_eq!(first, 1, "{first} deliveries were told they were first; exactly one may be");
}
