//! The book, against a real database. Nothing here can be proved in memory:
//! the guarantees are about what the database does under contention.

use std::sync::Arc;
use std::time::Duration;

use rustlavel_db::{Database, Schema};
use rustlavel_ledger::{
    HoldStatus, Ledger, SYSTEM_CONSUMPTION, SYSTEM_EXPIRY, SYSTEM_TOPUP, TransferKind, create_tables,
    drop_tables,
};

/// Every test drops and recreates the same five tables, so they take turns:
/// the lock is taken before the schema is touched and held until the test
/// ends. Without it, ten tests share one schema and truncate each other's
/// rows mid-assertion — and the concurrency *inside* a test, which is the
/// thing being measured, would be measuring the wrong contention.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A ledger on a schema this test owns until it returns. Derefs to the
/// ledger, so a test reads as though it held one.
struct Fixture {
    ledger: Ledger,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

impl std::ops::Deref for Fixture {
    type Target = Ledger;

    fn deref(&self) -> &Ledger {
        &self.ledger
    }
}

async fn ledger() -> Option<Fixture> {
    let url = std::env::var("LEDGER_TEST_DATABASE_URL").ok()?;
    let guard = SERIAL.lock().await;
    let db = Database::connect(&url).await.expect("the test database is reachable");
    let schema = Schema::new(&db);
    let _ = drop_tables(&schema).await;
    create_tables(&schema).await.expect("the ledger tables");
    Some(Fixture { ledger: Ledger::new(db, "credits"), _guard: guard })
}

macro_rules! ledger_or_skip {
    () => {
        match ledger().await {
            Some(ledger) => ledger,
            None => {
                println!("skipped: set LEDGER_TEST_DATABASE_URL to run the ledger tests");
                return;
            }
        }
    };
}

#[tokio::test]
async fn a_top_up_is_double_entry_and_idempotent() {
    let ledger = ledger_or_skip!();

    let first = ledger.top_up("user:1", 500, "pay_1", None).await.unwrap();
    assert_eq!(first.kind, TransferKind::TopUp);
    assert_eq!(ledger.balance("user:1").await.unwrap().available, 500);

    // The other side of the book.
    assert_eq!(ledger.balance(SYSTEM_TOPUP).await.unwrap().total, -500);

    // The webhook is retried. Same reference, same transfer, nothing moves.
    let again = ledger.top_up("user:1", 500, "pay_1", None).await.unwrap();
    assert_eq!(again.id, first.id);
    assert_eq!(ledger.balance("user:1").await.unwrap().available, 500, "a retried top-up credited twice");

    assert!(ledger.audit().await.unwrap().is_empty());
}

#[tokio::test]
async fn consuming_more_than_available_moves_nothing() {
    let ledger = ledger_or_skip!();
    ledger.top_up("user:2", 100, "pay_2", None).await.unwrap();

    let error = ledger.consume("user:2", 150, "job_2").await.unwrap_err().to_string();
    assert!(error.contains("100 available"), "{error}");
    assert!(error.contains("150"), "{error}");

    assert_eq!(ledger.balance("user:2").await.unwrap().available, 100, "a refused consume changed the balance");
    assert!(ledger.transfer("job_2").await.unwrap().is_none(), "a refused consume was recorded");
    assert!(ledger.history("user:2", 10).await.unwrap().len() == 1);
}

/// **The test this crate exists for.** Twenty tasks each try to spend 10 from
/// a balance of 100. Exactly ten succeed, the balance ends at 0, never below,
/// and the book still balances. A read-then-write implementation lets more
/// through, and the balance goes negative — a double-spend.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_consumers_cannot_overdraw() {
    let ledger = Arc::new(ledger_or_skip!());
    ledger.top_up("user:3", 100, "pay_3", None).await.unwrap();

    let mut racing = Vec::new();
    for n in 0..20 {
        let ledger = Arc::clone(&ledger);
        racing.push(tokio::spawn(async move { ledger.consume("user:3", 10, &format!("job_3_{n}")).await }));
    }

    let mut succeeded = 0;
    for task in racing {
        if task.await.expect("finished").is_ok() {
            succeeded += 1;
        }
    }

    assert_eq!(succeeded, 10, "{succeeded} consumes succeeded against a balance good for 10");
    let balance = ledger.balance("user:3").await.unwrap();
    assert_eq!(balance.available, 0);
    assert!(balance.total >= 0, "the balance went negative: {balance:?}");
    assert_eq!(ledger.balance(SYSTEM_CONSUMPTION).await.unwrap().total, 100);
    assert!(ledger.audit().await.unwrap().is_empty(), "{:?}", ledger.audit().await.unwrap());
}

#[tokio::test]
async fn a_hold_reserves_and_capture_spends_exactly_once() {
    let ledger = ledger_or_skip!();
    ledger.top_up("user:4", 100, "pay_4", None).await.unwrap();

    let hold = ledger.hold("user:4", 60, "render_4", Duration::from_secs(600)).await.unwrap();
    assert_eq!(hold.status, HoldStatus::Held);

    let balance = ledger.balance("user:4").await.unwrap();
    assert_eq!(balance.total, 100, "a hold moved the balance");
    assert_eq!(balance.held, 60);
    assert_eq!(balance.available, 40);

    // The reserved credits cannot be spent by anything else.
    assert!(ledger.consume("user:4", 50, "other_4").await.is_err(), "held credits were spent");
    assert!(ledger.consume("user:4", 40, "other_4b").await.is_ok());

    // Holding again for the same job is the same hold.
    let same = ledger.hold("user:4", 60, "render_4", Duration::from_secs(600)).await.unwrap();
    assert_eq!(same.id, hold.id);

    let captured = ledger.capture(&hold.id).await.unwrap();
    assert_eq!(captured.kind, TransferKind::Capture);
    assert_eq!(captured.amount, 60);
    let balance = ledger.balance("user:4").await.unwrap();
    assert_eq!(balance.total, 0);
    assert_eq!(balance.held, 0);

    // Once. The second capture is an error that says what happened, not a
    // second charge.
    let error = ledger.capture(&hold.id).await.unwrap_err().to_string();
    assert!(error.contains("already captured"), "{error}");
    assert_eq!(ledger.balance("user:4").await.unwrap().total, 0);
    assert!(ledger.audit().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_released_hold_gives_the_credits_back_and_writes_nothing() {
    let ledger = ledger_or_skip!();
    ledger.top_up("user:5", 100, "pay_5", None).await.unwrap();
    let hold = ledger.hold("user:5", 70, "job_5", Duration::from_secs(600)).await.unwrap();

    ledger.release(&hold.id).await.unwrap();
    assert_eq!(ledger.balance("user:5").await.unwrap().available, 100);
    assert_eq!(ledger.history("user:5", 10).await.unwrap().len(), 1, "a release wrote to the history");

    assert!(ledger.release(&hold.id).await.is_err(), "a hold was released twice");
    assert!(ledger.capture(&hold.id).await.is_err(), "a released hold was captured");
}

/// Two workers finish the same job at once and both try to capture. One
/// charge, not two.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_captures_of_one_hold_charge_once() {
    let ledger = Arc::new(ledger_or_skip!());
    ledger.top_up("user:6", 100, "pay_6", None).await.unwrap();
    let hold = ledger.hold("user:6", 30, "job_6", Duration::from_secs(600)).await.unwrap();

    let mut racing = Vec::new();
    for _ in 0..16 {
        let ledger = Arc::clone(&ledger);
        let id = hold.id.clone();
        racing.push(tokio::spawn(async move { ledger.capture(&id).await }));
    }
    let mut won = 0;
    for task in racing {
        if task.await.expect("finished").is_ok() {
            won += 1;
        }
    }
    assert_eq!(won, 1, "{won} captures of one hold succeeded");
    assert_eq!(ledger.balance("user:6").await.unwrap().total, 70);
}

#[tokio::test]
async fn credits_expire_by_batch_and_the_oldest_are_spent_first() {
    let ledger = ledger_or_skip!();

    // A batch that has already expired, and one that has not.
    ledger.top_up("user:7", 50, "pay_7_old", Some(Duration::ZERO)).await.unwrap();
    ledger.top_up("user:7", 100, "pay_7_new", Some(Duration::from_secs(3600))).await.unwrap();
    assert_eq!(ledger.balance("user:7").await.unwrap().available, 150);

    let sweep = ledger.sweep().await.unwrap();
    assert_eq!(sweep.lots_expired, 1);
    assert_eq!(sweep.amount_expired, 50);

    assert_eq!(ledger.balance("user:7").await.unwrap().available, 100, "the live batch was touched");
    assert_eq!(ledger.balance(SYSTEM_EXPIRY).await.unwrap().total, 50);
    let history = ledger.history("user:7", 10).await.unwrap();
    assert_eq!(history[0].kind, TransferKind::Expire, "expiry did not show in the history");
    assert!(ledger.audit().await.unwrap().is_empty());

    // Sweeping again finds nothing.
    assert_eq!(ledger.sweep().await.unwrap().lots_expired, 0);
}

/// A job that dies without releasing must not keep its credits reserved
/// forever.
#[tokio::test]
async fn a_stale_hold_is_released_by_the_sweep() {
    let ledger = ledger_or_skip!();
    ledger.top_up("user:8", 100, "pay_8", None).await.unwrap();
    ledger.hold("user:8", 80, "dead_job", Duration::ZERO).await.unwrap();
    assert_eq!(ledger.balance("user:8").await.unwrap().available, 20);

    let sweep = ledger.sweep().await.unwrap();
    assert_eq!(sweep.holds_released, 1);
    assert_eq!(ledger.balance("user:8").await.unwrap().available, 100);
}

/// The history carries its running balance, so it can be checked line by line.
#[tokio::test]
async fn the_history_reads_like_a_statement() {
    let ledger = ledger_or_skip!();
    ledger.top_up("user:9", 100, "p1", None).await.unwrap();
    ledger.consume("user:9", 30, "j1").await.unwrap();
    ledger.top_up("user:9", 50, "p2", None).await.unwrap();

    let history = ledger.history("user:9", 10).await.unwrap();
    let lines: Vec<(i64, i64)> = history.iter().rev().map(|e| (e.amount, e.balance_after)).collect();
    assert_eq!(lines, [(100, 100), (-30, 70), (50, 120)]);
}

/// Two first-time callers for one owner race to create the account, and the
/// unique index lets one through. Both must get the same account.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_first_use_creates_one_account() {
    let ledger = Arc::new(ledger_or_skip!());
    let mut racing = Vec::new();
    for n in 0..8 {
        let ledger = Arc::clone(&ledger);
        racing.push(tokio::spawn(async move { ledger.top_up("user:10", 10, &format!("p_{n}"), None).await }));
    }
    for task in racing {
        task.await.expect("finished").expect("each top-up succeeded");
    }
    assert_eq!(ledger.balance("user:10").await.unwrap().total, 80);
    assert!(ledger.audit().await.unwrap().is_empty());
}
