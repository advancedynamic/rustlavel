# rustlavel-payment

Payment gateways behind one trait, for the [Rustlavel](https://github.com/advancedynamic/rustlavel) framework: charges over virtual account, QRIS, e-wallet and retail; transfers out; and a webhook receiver that verifies the signature and refuses to hear the same payment twice.

## The shape

```rust
use rustlavel::payment::{Channel, ChargeRequest, Gateway, Money, Bank};

let charge = gateway
    .create_charge(&ChargeRequest::new("order-7", Money::idr(150_000), Channel::VirtualAccount(Bank::Bca)))
    .await?;
// charge.instructions.account_number is what the customer transfers to.
```

`Money` is an integer of minor units — never a float, because money that is rounded is money that goes missing. `Channel` is an enum, so a typo in a bank code is a compile error rather than a charge nobody can pay.

## The receiver, and why it deduplicates on disk

A gateway retries a webhook until it hears an acknowledgement, and a network that drops the acknowledgement delivers the same payment twice. The receiver does three things in an order that matters:

1. **Verify the signature.** Over the raw bytes, never parsed JSON — re-serialising reorders keys and the signature no longer matches. Anything that fails is `401` and is not parsed further.
2. **Record the event by the gateway's id.** A duplicate is answered `200` without running the handler: the gateway needs to hear the acknowledgement it missed.
3. **Run the handler.** If it fails, answer `500` so the gateway retries — and withdraw the record, so the retry is processed rather than dropped as a duplicate of something nothing handled.

```rust
let receiver = Arc::new(Receiver::new(gateway, log, |event| Box::pin(async move {
    // credit the customer; return Err to be retried
    Ok(())
})));
r.post("/webhooks/payment", move |req| { let r = receiver.clone(); async move { r.handle(req).await } });
```

The record has to survive a restart, because a retry that arrives after a deploy is still a duplicate. `DatabaseWebhookLog` (behind the `db` feature) is one `INSERT` against a unique index — sixteen deliveries racing, exactly one told it was first, measured against PostgreSQL. `MemoryWebhookLog` is for tests. `database::schema` creates the table; call it from a migration.

## No driver is invented

`FakeGateway` is the only driver today. It keeps everything in memory, signs its callbacks the way a real gateway does, and refuses a second transfer under a reference it has seen — so a test cannot pass while relying on behaviour production would not have. `mark_paid` is the customer; `webhook_for` is the callback.

A driver for a real gateway is written against that gateway's published specification. An adapter written from what its competitors do is code that looks finished and fails on the first real callback.

## Licence

MIT.
