//! The whole flow, through the fake gateway: what an application's tests will
//! do too, which is why the fake signs its callbacks properly.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustlavel_payment::{
    Channel, ChargeRequest, ChargeStatus, FakeGateway, Gateway, Handled, MemoryWebhookLog, Money,
    Receiver, Wallet,
};

/// A receiver whose handler counts how many times it ran and can be told to
/// fail. The count is the assertion in most tests below.
fn receiver(gateway: Arc<FakeGateway>, fail: Arc<AtomicUsize>) -> (Receiver, Arc<AtomicUsize>) {
    let handled = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&handled);
    let log = Arc::new(MemoryWebhookLog::new());
    let receiver = Receiver::new(gateway, log, move |event| {
        let counter = Arc::clone(&counter);
        let fail = Arc::clone(&fail);
        Box::pin(async move {
            if fail.load(Ordering::SeqCst) > 0 {
                fail.fetch_sub(1, Ordering::SeqCst);
                return Err(rustlavel_core::Error::msg("the ledger is down"));
            }
            assert_eq!(event.charge.as_ref().map(|c| c.status), Some(ChargeStatus::Paid));
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    });
    (receiver, handled)
}

#[tokio::test]
async fn a_charge_is_created_paid_and_credited_once() {
    let gateway = Arc::new(FakeGateway::new("whsec_test"));
    let (receiver, handled) = receiver(Arc::clone(&gateway), Arc::new(AtomicUsize::new(0)));

    let charge = gateway
        .create_charge(&ChargeRequest::new("order-7", Money::idr(150_000), Channel::Qris))
        .await
        .unwrap();
    assert_eq!(charge.status, ChargeStatus::Pending);
    assert!(charge.instructions.qr_string.is_some(), "a QRIS charge came with nothing to scan");

    gateway.mark_paid(&charge.id).expect("pending, so payable");
    let (headers, body) = gateway.webhook_for(&charge.id, "evt_1").unwrap();

    assert_eq!(receiver.deliver(&headers, &body).await, Handled::Processed);
    assert_eq!(handled.load(Ordering::SeqCst), 1);
}

/// The gateway did not hear the acknowledgement and sends the same event
/// again. The customer must not be credited twice.
#[tokio::test]
async fn a_retried_delivery_is_acknowledged_and_not_handled_again() {
    let gateway = Arc::new(FakeGateway::new("whsec_test"));
    let (receiver, handled) = receiver(Arc::clone(&gateway), Arc::new(AtomicUsize::new(0)));

    let charge = gateway.create_charge(&ChargeRequest::new("o", Money::idr(1000), Channel::Qris)).await.unwrap();
    gateway.mark_paid(&charge.id);
    let (headers, body) = gateway.webhook_for(&charge.id, "evt_1").unwrap();

    assert_eq!(receiver.deliver(&headers, &body).await, Handled::Processed);
    assert_eq!(receiver.deliver(&headers, &body).await, Handled::Duplicate);
    assert_eq!(receiver.deliver(&headers, &body).await, Handled::Duplicate);
    assert_eq!(handled.load(Ordering::SeqCst), 1, "a duplicate reached the handler");
}

/// A body that did not come from the gateway is refused before it is parsed —
/// and a body that did, with one byte changed, is the same thing.
#[tokio::test]
async fn a_tampered_or_unsigned_body_is_refused_and_never_handled() {
    let gateway = Arc::new(FakeGateway::new("whsec_test"));
    let (receiver, handled) = receiver(Arc::clone(&gateway), Arc::new(AtomicUsize::new(0)));

    let charge = gateway.create_charge(&ChargeRequest::new("o", Money::idr(1000), Channel::Qris)).await.unwrap();
    gateway.mark_paid(&charge.id);
    let (headers, body) = gateway.webhook_for(&charge.id, "evt_1").unwrap();

    // The amount changed after signing.
    let tampered = String::from_utf8(body.clone()).unwrap().replace("1000", "1000000").into_bytes();
    assert_eq!(receiver.deliver(&headers, &tampered).await, Handled::Refused);

    // Signed under somebody else's secret.
    let other = FakeGateway::new("whsec_other");
    assert_eq!(receiver.deliver(&other.sign(&body), &body).await, Handled::Refused);

    // No signature at all.
    assert_eq!(receiver.deliver(&rustlavel_http::Headers::new(), &body).await, Handled::Refused);

    assert_eq!(handled.load(Ordering::SeqCst), 0, "a refused body reached the handler");
}

/// The handler crashes — the ledger is down. The gateway must be made to
/// retry, and the retry must be processed rather than dropped as a duplicate
/// of the event nothing handled.
#[tokio::test]
async fn a_failed_handler_is_retried_and_then_succeeds_exactly_once() {
    let gateway = Arc::new(FakeGateway::new("whsec_test"));
    let fail_next = Arc::new(AtomicUsize::new(1));
    let (receiver, handled) = receiver(Arc::clone(&gateway), fail_next);

    let charge = gateway.create_charge(&ChargeRequest::new("o", Money::idr(1000), Channel::Qris)).await.unwrap();
    gateway.mark_paid(&charge.id);
    let (headers, body) = gateway.webhook_for(&charge.id, "evt_1").unwrap();

    assert!(matches!(receiver.deliver(&headers, &body).await, Handled::HandlerFailed(_)));
    assert_eq!(handled.load(Ordering::SeqCst), 0);

    // The gateway retries. This time the ledger is up.
    assert_eq!(receiver.deliver(&headers, &body).await, Handled::Processed, "the retry was dropped as a duplicate");
    assert_eq!(handled.load(Ordering::SeqCst), 1);

    // And a third delivery is now the real duplicate.
    assert_eq!(receiver.deliver(&headers, &body).await, Handled::Duplicate);
    assert_eq!(handled.load(Ordering::SeqCst), 1);
}

/// "paid" and "expired" for one charge are two events, and the second must
/// not be dropped as a duplicate of the first.
#[tokio::test]
async fn two_kinds_of_event_for_one_charge_are_both_delivered() {
    let gateway = Arc::new(FakeGateway::new("whsec_test"));
    let log = Arc::new(MemoryWebhookLog::new());
    let receiver = Receiver::new(Arc::clone(&gateway) as Arc<dyn Gateway>, log, |_| Box::pin(async { Ok(()) }));

    let a = gateway.create_charge(&ChargeRequest::new("a", Money::idr(1000), Channel::Qris)).await.unwrap();
    let b = gateway.create_charge(&ChargeRequest::new("b", Money::idr(1000), Channel::EWallet(Wallet::Dana))).await.unwrap();
    gateway.mark_paid(&a.id);
    gateway.mark_expired(&b.id);

    let (h1, b1) = gateway.webhook_for(&a.id, "evt_a").unwrap();
    let (h2, b2) = gateway.webhook_for(&b.id, "evt_b").unwrap();
    assert_eq!(receiver.deliver(&h1, &b1).await, Handled::Processed);
    assert_eq!(receiver.deliver(&h2, &b2).await, Handled::Processed);
}

#[tokio::test]
async fn a_paid_charge_cannot_be_cancelled_and_a_pending_one_can() {
    let gateway = FakeGateway::new("s");
    let paid = gateway.create_charge(&ChargeRequest::new("p", Money::idr(1000), Channel::Qris)).await.unwrap();
    gateway.mark_paid(&paid.id);
    let error = gateway.cancel_charge(&paid.id).await.unwrap_err().to_string();
    assert!(error.contains("refunded"), "{error}");

    let pending = gateway.create_charge(&ChargeRequest::new("q", Money::idr(1000), Channel::Qris)).await.unwrap();
    assert_eq!(gateway.cancel_charge(&pending.id).await.unwrap().status, ChargeStatus::Cancelled);
    assert!(gateway.mark_paid(&pending.id).is_none(), "a cancelled charge was paid");
}

/// A retry of a transfer under the same reference must not pay twice. The
/// fake refuses it the way a real gateway does, so a test cannot pass while
/// relying on behaviour production would not have.
#[tokio::test]
async fn a_transfer_reference_is_used_once() {
    use rustlavel_payment::{Account, Bank, TransferRequest};
    let gateway = FakeGateway::new("s");
    let to = Account::Bank { bank: Bank::Bca, number: "123".into(), holder: "Ada".into() };
    let request = TransferRequest::new("payout-1", Money::idr(50_000), to);

    assert!(gateway.transfer(&request).await.is_ok());
    let error = gateway.transfer(&request).await.unwrap_err().to_string();
    assert!(error.contains("already exists"), "a second transfer under one reference went through: {error}");
}
