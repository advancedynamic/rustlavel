//! The loop, against a real database and the fake gateway. What is measured
//! here is what happens under retries and under two schedulers.

use std::sync::Arc;
use std::time::Duration;

use rustlavel_db::{Database, Schema};
use rustlavel_ledger::Ledger;
use rustlavel_payment::{Bank, Channel, FakeGateway, Gateway, Money};
use rustlavel_billing::{
    Billing, Cycle, InvoiceStatus, Paid, Plan, ReminderKind, Subscriber, SubscriptionStatus, create_tables,
    drop_tables,
};

const DAY: u64 = 86_400;

struct Fixture {
    billing: Billing,
    gateway: Arc<FakeGateway>,
    ledger: Ledger,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

/// One schema, dropped and created per test, so the tests take turns.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn fixture() -> Option<Fixture> {
    let url = std::env::var("BILLING_TEST_DATABASE_URL").ok()?;
    let guard = SERIAL.lock().await;
    let db = Database::connect(&url).await.expect("the test database is reachable");
    let schema = Schema::new(&db);
    let _ = drop_tables(&schema).await;
    let _ = rustlavel_ledger::drop_tables(&schema).await;
    create_tables(&schema).await.expect("the billing tables");
    rustlavel_ledger::create_tables(&schema).await.expect("the ledger tables");

    let gateway = Arc::new(FakeGateway::new("secret"));
    let ledger = Ledger::new(db.clone(), "credits");
    let plans = vec![
        Plan::new("starter", "Starter", Money::idr(49_000), 100),
        Plan::new("pro", "Pro", Money::idr(149_000), 500).grace(Duration::from_secs(3 * DAY)),
        Plan::new("weekly", "Weekly", Money::idr(15_000), 20).cycle(Cycle::Days(7)).grace(Duration::from_secs(DAY)),
    ];
    let billing = Billing::new(db, ledger.clone(), gateway.clone(), plans);
    Some(Fixture { billing, gateway, ledger, _guard: guard })
}

macro_rules! fixture_or_skip {
    () => {
        match fixture().await {
            Some(fixture) => fixture,
            None => {
                println!("skipped: set BILLING_TEST_DATABASE_URL to run the billing tests");
                return;
            }
        }
    };
}

fn ada() -> Subscriber {
    Subscriber::new("user:ada", "Ada").email("ada@example.test")
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

#[tokio::test]
async fn subscribing_raises_an_invoice_with_a_way_to_pay_and_grants_nothing_yet() {
    let f = fixture_or_skip!();

    let invoice = f.billing.subscribe(ada(), "pro", Channel::VirtualAccount(Bank::Bca)).await.unwrap();
    assert_eq!(invoice.number, 1);
    assert_eq!(invoice.amount, Money::idr(149_000));
    assert!(invoice.is_payable(), "no charge was created: {invoice:?}");
    assert!(invoice.instructions.account_number.is_some(), "a VA charge has no account number");

    let subscription = f.billing.for_customer("user:ada").await.unwrap().unwrap();
    assert_eq!(subscription.status, SubscriptionStatus::Pending);
    assert!(!f.billing.is_served("user:ada").await.unwrap(), "served before paying");
    assert_eq!(f.ledger.balance("user:ada").await.unwrap().available, 0, "credited before paying");

    // The charge at the gateway carries the invoice as its reference: that
    // is how the webhook finds its way back.
    let charge = f.gateway.charges().into_iter().next().unwrap();
    assert_eq!(charge.reference, invoice.id);

    // One live subscription per customer.
    let error = f.billing.subscribe(ada(), "starter", Channel::Qris).await.unwrap_err().to_string();
    assert!(error.contains("already has subscription"), "{error}");
}

#[tokio::test]
async fn paying_extends_access_by_one_cycle_and_credits_the_plan_once() {
    let f = fixture_or_skip!();
    let invoice = f.billing.subscribe(ada(), "pro", Channel::Qris).await.unwrap();
    let before = now();

    let paid = f.billing.on_paid(&invoice.id).await.unwrap();
    let Paid::Extended { subscription, invoice: paid_invoice } = paid else {
        panic!("first payment was not an extension: {paid:?}");
    };
    assert_eq!(subscription.status, SubscriptionStatus::Active);
    assert!(subscription.period_start >= before);
    assert_eq!(subscription.period_end, subscription.period_start + 30 * DAY);
    assert_eq!(paid_invoice.status, InvoiceStatus::Paid);
    assert_eq!(paid_invoice.period_end, subscription.period_end);
    assert!(f.billing.is_served("user:ada").await.unwrap());

    let balance = f.ledger.balance("user:ada").await.unwrap();
    assert_eq!(balance.available, 500);

    // The webhook is retried. Same invoice: nothing moves.
    assert_eq!(f.billing.on_paid(&invoice.id).await.unwrap(), Paid::Already);
    assert_eq!(f.ledger.balance("user:ada").await.unwrap().available, 500, "a retried payment credited twice");
    let refreshed = f.billing.subscription(&subscription.id).await.unwrap().unwrap();
    assert_eq!(refreshed.period_end, subscription.period_end, "a retried payment extended twice");
}

/// Sixteen deliveries of the same "paid" webhook at once. One extension, one
/// top-up. The invoice's `open → paid` flip and the ledger's unique reference
/// are the two guards; both are needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sixteen_concurrent_paid_webhooks_extend_once() {
    let f = fixture_or_skip!();
    let invoice = f.billing.subscribe(ada(), "pro", Channel::Qris).await.unwrap();
    let billing = Arc::new(f.billing.clone());

    let mut racing = Vec::new();
    for _ in 0..16 {
        let billing = Arc::clone(&billing);
        let id = invoice.id.clone();
        racing.push(tokio::spawn(async move { billing.on_paid(&id).await }));
    }
    let mut extended = 0;
    for task in racing {
        if let Paid::Extended { .. } = task.await.unwrap().unwrap() {
            extended += 1;
        }
    }
    assert_eq!(extended, 1, "{extended} webhooks each extended the subscription");
    assert_eq!(f.ledger.balance("user:ada").await.unwrap().available, 500);
    assert!(f.ledger.audit().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_renewal_is_raised_ahead_reminded_then_grace_then_suspended() {
    let f = fixture_or_skip!();
    let first = f.billing.subscribe(ada(), "pro", Channel::Qris).await.unwrap();
    f.billing.on_paid(&first.id).await.unwrap();
    let subscription = f.billing.for_customer("user:ada").await.unwrap().unwrap();
    let end = subscription.period_end;

    // Twenty days in: too early for anything.
    let tick = f.billing.tick_at(end - 10 * DAY).await.unwrap();
    assert_eq!(tick.invoices_raised, 0);
    assert!(tick.reminders.is_empty());

    // Seven days before the end: the renewal is raised, for the next period,
    // with a charge the customer can pay now.
    let tick = f.billing.tick_at(end - 7 * DAY).await.unwrap();
    assert_eq!(tick.invoices_raised, 1);
    let renewal = f.billing.open_invoice(&subscription.id).await.unwrap().unwrap();
    assert_eq!(renewal.number, 2);
    assert_eq!(renewal.period_start, end);
    assert_eq!(renewal.due_at, end);
    assert!(renewal.is_payable());

    // Raised once: the next tick does not raise it again.
    let tick = f.billing.tick_at(end - 6 * DAY).await.unwrap();
    assert_eq!(tick.invoices_raised, 0);
    assert_eq!(f.billing.invoices(&subscription.id).await.unwrap().len(), 2);

    // Three days out: one reminder. Once.
    let tick = f.billing.tick_at(end - 3 * DAY + 60).await.unwrap();
    assert_eq!(tick.reminders.len(), 1, "{:?}", tick.reminders);
    assert_eq!(tick.reminders[0].kind, ReminderKind::Due { days_left: 3 });
    assert_eq!(tick.reminders[0].subscriber.email.as_deref(), Some("ada@example.test"));
    assert_eq!(tick.reminders[0].plan.id, "pro");
    assert!(f.billing.tick_at(end - 2 * DAY).await.unwrap().reminders.is_empty(), "the same reminder twice");

    // One day out: the second.
    let tick = f.billing.tick_at(end - DAY + 60).await.unwrap();
    assert_eq!(tick.reminders.len(), 1);
    assert_eq!(tick.reminders[0].kind, ReminderKind::Due { days_left: 1 });

    // The period ends unpaid: grace, still served, told so.
    let tick = f.billing.tick_at(end + 60).await.unwrap();
    assert_eq!(tick.moved_to_grace, 1);
    assert_eq!(tick.reminders.len(), 1);
    assert_eq!(tick.reminders[0].kind, ReminderKind::Overdue);
    let subscription = f.billing.subscription(&subscription.id).await.unwrap().unwrap();
    assert_eq!(subscription.status, SubscriptionStatus::Grace);
    assert!(subscription.is_served_at(end + DAY, 3 * DAY));
    assert!(!subscription.is_served_at(end + 4 * DAY, 3 * DAY), "served past the grace");

    // Grace runs out: suspended, the invoice expired, the charge withdrawn.
    let tick = f.billing.tick_at(end + 3 * DAY + 60).await.unwrap();
    assert_eq!(tick.suspended, 1);
    assert_eq!(tick.reminders.len(), 1);
    assert_eq!(tick.reminders[0].kind, ReminderKind::Suspended);
    let subscription = f.billing.subscription(&subscription.id).await.unwrap().unwrap();
    assert_eq!(subscription.status, SubscriptionStatus::Suspended);
    let renewal = f.billing.invoice(&renewal.id).await.unwrap().unwrap();
    assert_eq!(renewal.status, InvoiceStatus::Expired);
    assert!(f.billing.open_invoice(&subscription.id).await.unwrap().is_none());

    // Nothing more happens to it on later ticks.
    let tick = f.billing.tick_at(end + 30 * DAY).await.unwrap();
    assert_eq!(tick, rustlavel_billing::Tick::default());
}

#[tokio::test]
async fn paying_the_renewal_in_grace_starts_where_the_last_period_ended() {
    let f = fixture_or_skip!();
    let first = f.billing.subscribe(ada(), "pro", Channel::Qris).await.unwrap();
    f.billing.on_paid(&first.id).await.unwrap();
    let subscription = f.billing.for_customer("user:ada").await.unwrap().unwrap();
    let end = subscription.period_end;

    f.billing.tick_at(end - 7 * DAY).await.unwrap();
    f.billing.tick_at(end + DAY).await.unwrap();
    assert_eq!(
        f.billing.subscription(&subscription.id).await.unwrap().unwrap().status,
        SubscriptionStatus::Grace
    );

    let renewal = f.billing.open_invoice(&subscription.id).await.unwrap().unwrap();
    let Paid::Extended { subscription, .. } = f.billing.on_paid(&renewal.id).await.unwrap() else {
        panic!("the renewal did not extend");
    };
    assert_eq!(subscription.status, SubscriptionStatus::Active);
    assert_eq!(subscription.period_start, end, "grace days were charged twice");
    assert_eq!(subscription.period_end, end + 30 * DAY);
    assert_eq!(f.ledger.balance("user:ada").await.unwrap().available, 1000);
}

#[tokio::test]
async fn a_suspended_subscription_is_renewed_on_demand_and_starts_now() {
    let f = fixture_or_skip!();
    let first = f.billing.subscribe(ada(), "weekly", Channel::Qris).await.unwrap();
    f.billing.on_paid(&first.id).await.unwrap();
    let subscription = f.billing.for_customer("user:ada").await.unwrap().unwrap();
    let end = subscription.period_end;

    f.billing.tick_at(end + 2 * DAY).await.unwrap(); // grace
    f.billing.tick_at(end + 2 * DAY).await.unwrap(); // suspended (grace is a day)
    assert_eq!(
        f.billing.subscription(&subscription.id).await.unwrap().unwrap().status,
        SubscriptionStatus::Suspended
    );
    assert!(!f.billing.is_served("user:ada").await.unwrap());

    // `renew` on an active one is refused with directions.
    let renewal = f.billing.renew(&subscription.id, Channel::VirtualAccount(Bank::Mandiri)).await.unwrap();
    assert!(renewal.is_payable());
    assert_eq!(renewal.channel, Channel::VirtualAccount(Bank::Mandiri));
    // Asked twice, the same open invoice comes back.
    assert_eq!(f.billing.renew(&subscription.id, Channel::Qris).await.unwrap().id, renewal.id);

    let before = now();
    let Paid::Extended { subscription, .. } = f.billing.on_paid(&renewal.id).await.unwrap() else {
        panic!("the renewal did not extend");
    };
    assert!(subscription.period_start >= before, "a suspended customer was charged for the time they were out");
    assert_eq!(subscription.status, SubscriptionStatus::Active);
    assert!(f.billing.is_served("user:ada").await.unwrap());

    let error = f.billing.renew(&subscription.id, Channel::Qris).await.unwrap_err().to_string();
    assert!(error.contains("is active"), "{error}");
}

#[tokio::test]
async fn cancelling_stops_the_renewal_and_ends_at_the_period_end() {
    let f = fixture_or_skip!();
    let first = f.billing.subscribe(ada(), "pro", Channel::Qris).await.unwrap();
    f.billing.on_paid(&first.id).await.unwrap();
    let subscription = f.billing.for_customer("user:ada").await.unwrap().unwrap();
    let end = subscription.period_end;

    f.billing.tick_at(end - 7 * DAY).await.unwrap();
    let renewal = f.billing.open_invoice(&subscription.id).await.unwrap().unwrap();

    let cancelled = f.billing.cancel(&subscription.id).await.unwrap();
    assert_eq!(cancelled.status, SubscriptionStatus::Active, "cancelling cut access the customer paid for");
    assert!(cancelled.cancel_at_period_end);
    assert_eq!(f.billing.invoice(&renewal.id).await.unwrap().unwrap().status, InvoiceStatus::Void);
    assert!(f.billing.is_served("user:ada").await.unwrap());

    // No renewal is raised again.
    let tick = f.billing.tick_at(end - 5 * DAY).await.unwrap();
    assert_eq!(tick.invoices_raised, 0);
    assert!(tick.reminders.is_empty());

    let tick = f.billing.tick_at(end + 60).await.unwrap();
    assert_eq!(tick.cancelled, 1);
    assert_eq!(tick.moved_to_grace, 0);
    assert!(!f.billing.is_served("user:ada").await.unwrap());
    assert!(f.billing.for_customer("user:ada").await.unwrap().is_none());

    // And the customer may start over.
    f.billing.subscribe(ada(), "starter", Channel::Qris).await.unwrap();
}

#[tokio::test]
async fn changing_plan_reissues_the_renewal_and_takes_effect_when_paid() {
    let f = fixture_or_skip!();
    let first = f.billing.subscribe(ada(), "starter", Channel::Qris).await.unwrap();
    f.billing.on_paid(&first.id).await.unwrap();
    let subscription = f.billing.for_customer("user:ada").await.unwrap().unwrap();
    let end = subscription.period_end;

    f.billing.tick_at(end - 7 * DAY).await.unwrap();
    let old = f.billing.open_invoice(&subscription.id).await.unwrap().unwrap();
    assert_eq!(old.amount, Money::idr(49_000));

    let changed = f.billing.change_plan(&subscription.id, "pro").await.unwrap();
    assert_eq!(changed.plan_id, "starter", "the plan changed before it was paid for");
    assert_eq!(changed.next_plan_id.as_deref(), Some("pro"));
    assert_eq!(f.billing.invoice(&old.id).await.unwrap().unwrap().status, InvoiceStatus::Void);

    let renewal = f.billing.open_invoice(&subscription.id).await.unwrap().unwrap();
    assert_eq!(renewal.amount, Money::idr(149_000));
    assert_eq!(renewal.plan_id, "pro");
    assert_eq!(renewal.number, 3);
    assert!(renewal.is_payable());

    let Paid::Extended { subscription, .. } = f.billing.on_paid(&renewal.id).await.unwrap() else {
        panic!("the renewal did not extend");
    };
    assert_eq!(subscription.plan_id, "pro");
    assert_eq!(subscription.next_plan_id, None);
    assert_eq!(f.ledger.balance("user:ada").await.unwrap().available, 600, "starter's 100 then pro's 500");
}

#[tokio::test]
async fn an_unpaid_first_invoice_closes_the_subscription() {
    let f = fixture_or_skip!();
    let invoice = f.billing.subscribe(ada(), "pro", Channel::Qris).await.unwrap();

    assert_eq!(f.billing.tick_at(now() + 3600).await.unwrap().cancelled, 0);
    let tick = f.billing.tick_at(now() + 2 * DAY).await.unwrap();
    assert_eq!(tick.cancelled, 1);
    assert_eq!(f.billing.invoice(&invoice.id).await.unwrap().unwrap().status, InvoiceStatus::Expired);
    assert!(f.billing.for_customer("user:ada").await.unwrap().is_none());

    // Money arriving for it now is flagged, not applied.
    assert!(matches!(f.billing.on_paid(&invoice.id).await.unwrap(), Paid::Unexpected { .. }));
    assert_eq!(f.ledger.balance("user:ada").await.unwrap().available, 0);
}

#[tokio::test]
async fn a_lost_charge_on_a_renewal_is_recreated_by_the_next_tick() {
    let f = fixture_or_skip!();
    let first = f.billing.subscribe(ada(), "pro", Channel::Qris).await.unwrap();
    f.billing.on_paid(&first.id).await.unwrap();
    let subscription = f.billing.for_customer("user:ada").await.unwrap().unwrap();
    let end = subscription.period_end;

    f.billing.tick_at(end - 7 * DAY).await.unwrap();
    let renewal = f.billing.open_invoice(&subscription.id).await.unwrap().unwrap();
    let old_charge = renewal.charge_id.clone().unwrap();

    // The gateway expires the charge and says so.
    f.gateway.mark_expired(&old_charge).unwrap();
    f.billing.on_charge_lost(&renewal.id).await.unwrap();
    let renewal = f.billing.invoice(&renewal.id).await.unwrap().unwrap();
    assert_eq!(renewal.status, InvoiceStatus::Open, "a renewal whose charge expired is still owed");
    assert!(!renewal.is_payable());

    let tick = f.billing.tick_at(end - 6 * DAY).await.unwrap();
    assert_eq!(tick.charges_created, 1);
    assert_eq!(tick.invoices_raised, 0, "a second renewal was raised instead of a second charge");
    let renewal = f.billing.invoice(&renewal.id).await.unwrap().unwrap();
    assert!(renewal.is_payable());
    assert_ne!(renewal.charge_id.as_deref(), Some(old_charge.as_str()));
}

#[tokio::test]
async fn reissuing_moves_the_invoice_to_another_channel() {
    let f = fixture_or_skip!();
    let invoice = f.billing.subscribe(ada(), "pro", Channel::Qris).await.unwrap();
    assert!(invoice.instructions.qr_string.is_some());

    let reissued = f.billing.reissue(&invoice.id, Channel::VirtualAccount(Bank::Bni)).await.unwrap();
    assert_eq!(reissued.id, invoice.id);
    assert!(reissued.instructions.account_number.is_some());
    assert_ne!(reissued.charge_id, invoice.charge_id);

    let subscription = f.billing.for_customer("user:ada").await.unwrap().unwrap();
    assert_eq!(subscription.channel, Channel::VirtualAccount(Bank::Bni), "the renewal would use the old channel");

    // The old charge is cancelled at the gateway; paying the new one pays
    // the invoice.
    let old = f.gateway.charges().into_iter().find(|c| Some(&c.id) == invoice.charge_id.as_ref()).unwrap();
    assert_eq!(old.status, rustlavel_payment::ChargeStatus::Cancelled);
    assert!(matches!(f.billing.on_paid(&invoice.id).await.unwrap(), Paid::Extended { .. }));
}

/// **Two schedulers.** Eight ticks at the same moment over five active
/// subscriptions: five renewals, five charges, and — a day before the due
/// date — five reminders. Not forty. The invoice number and the reminder key
/// are each claimed through a unique index; a read-then-insert would raise
/// every renewal several times and mail every customer eight reminders.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn eight_concurrent_ticks_raise_each_renewal_and_each_reminder_once() {
    let f = fixture_or_skip!();
    let mut ends = Vec::new();
    let mut first_invoices = Vec::new();
    for n in 0..5 {
        let subscriber = Subscriber::new(format!("user:{n}"), format!("User {n}"));
        let invoice = f.billing.subscribe(subscriber, "pro", Channel::Qris).await.unwrap();
        f.billing.on_paid(&invoice.id).await.unwrap();
        first_invoices.push(invoice.id);
        ends.push(f.billing.for_customer(&format!("user:{n}")).await.unwrap().unwrap().period_end);
    }
    let at = ends.iter().max().unwrap() - DAY + 60;
    let billing = Arc::new(f.billing.clone());

    let mut racing = Vec::new();
    for _ in 0..8 {
        let billing = Arc::clone(&billing);
        racing.push(tokio::spawn(async move { billing.tick_at(at).await.unwrap() }));
    }
    let mut raised = 0;
    let mut charges = 0;
    let mut reminders = Vec::new();
    for task in racing {
        let tick = task.await.unwrap();
        raised += tick.invoices_raised;
        charges += tick.charges_created;
        reminders.extend(tick.reminders);
    }
    assert_eq!(raised, 5, "{raised} renewals were raised for 5 subscriptions");
    // A tick may see another's fresh invoice before its charge is stored and
    // create one too. That is allowed — the surplus is cancelled below — but
    // it is never more than one per invoice.
    assert!(charges <= 5, "{charges} charges were added to invoices that had none");

    for n in 0..5 {
        let subscription = f.billing.for_customer(&format!("user:{n}")).await.unwrap().unwrap();
        let invoices = f.billing.invoices(&subscription.id).await.unwrap();
        assert_eq!(invoices.len(), 2, "user:{n} has {} invoices", invoices.len());
        assert!(invoices[0].is_payable());
    }

    // Every renewal was raised in this same tick, a day before it is due, so
    // both the three-day and the one-day reminder fall due at once — ten in
    // all, each exactly once.
    assert_eq!(reminders.len(), 10, "{} reminders for 5 customers × 2 kinds", reminders.len());
    let mut keys: Vec<String> = reminders.iter().map(|r| format!("{}:{}", r.invoice.id, r.kind.key())).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), 10, "a reminder was emitted twice");

    // One charge per invoice at the gateway: nothing surplus was left open,
    // and the one each invoice points at is the live one.
    // (The first invoices were paid straight through `on_paid`, so the fake
    // gateway still shows their charges pending; only renewals count here.)
    let charges = f.gateway.charges();
    let open = charges
        .iter()
        .filter(|c| c.status == rustlavel_payment::ChargeStatus::Pending && !first_invoices.contains(&c.reference))
        .count();
    assert_eq!(open, 5, "{open} charges open at the gateway for 5 renewals");
    for n in 0..5 {
        let subscription = f.billing.for_customer(&format!("user:{n}")).await.unwrap().unwrap();
        let renewal = f.billing.open_invoice(&subscription.id).await.unwrap().unwrap();
        let charge = charges.iter().find(|c| Some(&c.id) == renewal.charge_id.as_ref()).unwrap();
        assert_eq!(charge.status, rustlavel_payment::ChargeStatus::Pending, "user:{n}'s invoice points at a cancelled charge");
    }
}

#[tokio::test]
async fn a_webhook_about_something_else_is_left_alone() {
    let f = fixture_or_skip!();
    let invoice = f.billing.subscribe(ada(), "pro", Channel::Qris).await.unwrap();

    // A top-up charge the application made itself, through the same gateway.
    let other = f
        .gateway
        .create_charge(&rustlavel_payment::ChargeRequest::new("topup_9", Money::idr(10_000), Channel::Qris))
        .await
        .unwrap();
    f.gateway.mark_paid(&other.id).unwrap();
    let (headers, body) = f.gateway.webhook_for(&other.id, "evt_1").unwrap();
    let event = f.gateway.verify_webhook(&headers, &body).unwrap();
    assert_eq!(f.billing.on_event(&event).await.unwrap(), None);

    // Ours, through the same door.
    f.gateway.mark_paid(invoice.charge_id.as_deref().unwrap()).unwrap();
    let (headers, body) = f.gateway.webhook_for(invoice.charge_id.as_deref().unwrap(), "evt_2").unwrap();
    let event = f.gateway.verify_webhook(&headers, &body).unwrap();
    assert!(matches!(f.billing.on_event(&event).await.unwrap(), Some(Paid::Extended { .. })));
}
