# rustlavel-billing

Plans and subscriptions for the [Rustlavel](https://github.com/advancedynamic/rustlavel) framework, for a market where a subscription cannot pull money. It can only ask: each cycle it raises an invoice, presents a virtual account or a QR, and waits.

## The loop

```rust
let plans = vec![
    Plan::new("starter", "Starter", Money::idr(49_000), 100),   // 100 credits a cycle
    Plan::new("pro", "Pro", Money::idr(149_000), 500).grace(Duration::from_secs(3 * 86_400)),
];
let billing = Billing::new(db.clone(), ledger.clone(), gateway.clone(), plans);

// 1. Subscribe. The first invoice comes back with its charge — the VA number
//    or QR the customer pays. Nothing is granted yet.
let invoice = billing.subscribe(Subscriber::new("user:42", "Ada"), "pro", Channel::Qris).await?;
invoice.instructions.qr_string   // render this

// 2. The gateway's webhook says paid. Your receiver hands the event over.
match billing.on_event(&event).await? {
    Some(Paid::Extended { subscription, .. }) => …, // served until subscription.period_end; 500 credits in the ledger
    Some(Paid::Already) => …,                        // a retried webhook; nothing moved
    Some(Paid::Unexpected { invoice }) => …,         // paid a withdrawn charge; somebody should look
    None => …,                                       // not about an invoice — a top-up, say
}

// 3. On a schedule — every few minutes, from a queue job.
let tick = billing.tick().await?;
for reminder in tick.reminders {
    mail.send(renewal_reminder(&reminder))?;         // your words; this crate has none
}
```

`tick` raises the renewal seven days before a period ends, emits a `Due` reminder at three days and one day, moves an unpaid subscription into **grace** when the period ends (still served, told `Overdue`), and **suspends** it when the grace runs out (`Suspended`; the invoice expires and its charge is withdrawn). A first invoice nobody paid closes the subscription after a day. All of it is configurable on the builder.

## Paying

`on_paid` is idempotent twice over. The credits are a ledger top-up under the invoice's id, so a retried webhook finds them already there; the invoice flips `open → paid` with the status in the `WHERE`, so the second webhook changes nothing and is told `Already`. Sixteen deliveries of one "paid" webhook at once — measured against PostgreSQL — extend once and credit once.

Where the new period starts is a policy, and it is this one: a renewal paid early or **in grace starts where the last period ended** — the customer was served throughout, and grace days are not charged twice. A first payment, or one that ends a suspension, starts **now** — nothing was served before it.

## Changing

- `change_plan(sub, "pro")` takes effect at the next cycle. A renewal already raised for the old plan is voided and raised again for the new one. No proration: what a mid-cycle upgrade is worth is your policy, and the honest primitive is a top-up through the ledger.
- `cancel(sub)` stops the renewal; access lasts to the end of the paid period and no longer. `cancel_now` ends it today.
- `renew(sub, channel)` brings a suspended subscription back with a fresh invoice.
- `reissue(invoice, channel)` offers the same invoice through another channel — chose a VA, wants QRIS — and cancels the old charge.

A charge the gateway expires under an open renewal is recreated by the next tick, so a renewal keeps a live way to pay for as long as it is payable.

## Two schedulers

`tick` is safe to run from two places at once. Every transition is a guarded `UPDATE … WHERE status = ?`; every reminder is claimed by an insert into a table with a unique index on `(invoice, kind)`; every renewal by a unique index on `(subscription, period)`. Eight ticks at the same moment over five subscriptions: five renewals, ten reminders. Remove the reminder index and it is eighty. The first version claimed renewals by checking for one and inserting — eight ticks raised six for five subscriptions, because the check ran before the other tick's insert landed; the index on the period is what closed it.

## What it does not do

Overage — buy a top-up, which is one charge and one `Ledger::top_up`. Proration. Cards and auto-debit. Mail — it tells you what fell due; the words and the sending are yours.

## Tables

`create_tables(schema)` or the `CreateBillingTables` migration: `billing_subscriptions`, `billing_invoices`, `billing_reminders`. Needs the ledger's tables too.

## Licence

MIT.
