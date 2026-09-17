//! The loop: raise, wait, extend.

use crate::plan::Plan;
use crate::schema::{INVOICES, REMINDERS, SUBSCRIPTIONS};
use crate::subscription::{
    Invoice, InvoiceStatus, Subscriber, Subscription, SubscriptionStatus, instructions_from_json,
    instructions_to_json,
};
use rustlavel_core::{Error, Json, Result};
use rustlavel_db::{Database, Direction, Row, Value};
use rustlavel_ledger::Ledger;
use rustlavel_payment::{Channel, ChargeRequest, Customer, EventKind, Gateway, Instructions, Money, WebhookEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Subscriptions over a database, a ledger and a payment gateway.
///
/// The plans are given in code, not stored: they are configuration, they
/// change with a deploy, and a subscription refers to one by id. Removing a
/// plan that live subscriptions refer to is refused where it would matter,
/// with the plan's id in the message.
#[derive(Clone)]
pub struct Billing {
    db: Database,
    ledger: Ledger,
    gateway: Arc<dyn Gateway>,
    plans: Vec<Plan>,
    /// How far before a period ends its renewal invoice is raised.
    lead: Duration,
    /// How long before the due date each reminder is emitted.
    remind_before: Vec<Duration>,
    /// How long a first invoice stays payable.
    first_invoice_ttl: Duration,
    /// Whether a plan's credits die with the period they were bought for.
    credits_expire: bool,
}

/// What one run of [`Billing::tick`] did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tick {
    pub invoices_raised: usize,
    /// Charges created for invoices that had none — the gateway was
    /// unreachable when they were raised, or expired the charge since.
    pub charges_created: usize,
    pub moved_to_grace: usize,
    pub suspended: usize,
    pub cancelled: usize,
    /// For the application to send. Each is emitted once, ever, whichever
    /// tick got there first.
    pub reminders: Vec<Reminder>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderKind {
    /// The renewal is due in this many days.
    Due { days_left: u64 },
    /// The period ended and the renewal is unpaid; the customer is in grace.
    Overdue,
    /// Grace ran out.
    Suspended,
}

impl ReminderKind {
    /// The key a reminder is deduplicated on.
    pub fn key(self) -> String {
        match self {
            ReminderKind::Due { days_left } => format!("due:{days_left}"),
            ReminderKind::Overdue => "overdue".to_string(),
            ReminderKind::Suspended => "suspended".to_string(),
        }
    }
}

/// Something to tell the customer. The words are the application's.
#[derive(Debug, Clone, PartialEq)]
pub struct Reminder {
    pub kind: ReminderKind,
    pub subscriber: Subscriber,
    pub subscription_id: String,
    pub plan: Plan,
    pub invoice: Invoice,
}

/// What [`Billing::on_paid`] found.
// The variants differ in size because one carries the result and the others
// carry a verdict; boxing the result to even them out would make the common
// case worse to use.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Paid {
    /// This call paid the invoice; the subscription now runs to `period_end`
    /// and the credits are in the ledger.
    Extended { subscription: Subscription, invoice: Invoice },
    /// The invoice was already paid — a retried webhook. Nothing moved.
    Already,
    /// The invoice is void or expired: the customer paid a charge that had
    /// been withdrawn. Nothing moved; somebody should look, because the
    /// money is at the gateway and the customer has nothing for it.
    Unexpected { invoice: Invoice },
}

impl Billing {
    /// Credits go into `ledger` under the subscriber's id; money is asked for
    /// through `gateway`.
    pub fn new(db: Database, ledger: Ledger, gateway: Arc<dyn Gateway>, plans: Vec<Plan>) -> Billing {
        Billing {
            db,
            ledger,
            gateway,
            plans,
            lead: Duration::from_secs(7 * 86_400),
            remind_before: vec![Duration::from_secs(3 * 86_400), Duration::from_secs(86_400)],
            first_invoice_ttl: Duration::from_secs(86_400),
            credits_expire: true,
        }
    }

    /// Raise renewals this long before the period ends. Default seven days.
    pub fn raise_before(mut self, lead: Duration) -> Billing {
        self.lead = lead;
        self
    }

    /// Emit a `Due` reminder this long before each due date. Default three
    /// days and one day.
    pub fn remind_before(mut self, before: impl IntoIterator<Item = Duration>) -> Billing {
        self.remind_before = before.into_iter().collect();
        self
    }

    /// How long a first invoice stays payable. Default a day.
    pub fn first_invoice_ttl(mut self, ttl: Duration) -> Billing {
        self.first_invoice_ttl = ttl;
        self
    }

    /// Let unused credits carry over instead of expiring with the period.
    pub fn credits_roll_over(mut self) -> Billing {
        self.credits_expire = false;
        self
    }

    pub fn plans(&self) -> &[Plan] {
        &self.plans
    }

    pub fn plan(&self, id: &str) -> Result<&Plan> {
        self.plans.iter().find(|plan| plan.id == id).ok_or_else(|| {
            Error::msg(format!(
                "no plan `{id}` is configured; a subscription refers to it, so it cannot be removed while that is so"
            ))
        })
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    // ------------------------------------------------------------------
    // Reading
    // ------------------------------------------------------------------

    pub async fn subscription(&self, id: &str) -> Result<Option<Subscription>> {
        let row = self.db.table(SUBSCRIPTIONS).filter("subscription_id", id).first(&self.db).await?;
        row.as_ref().map(subscription_from).transpose()
    }

    /// The customer's live subscription — anything not cancelled — if any.
    pub async fn for_customer(&self, customer: &str) -> Result<Option<Subscription>> {
        let row = self
            .db
            .table(SUBSCRIPTIONS)
            .filter("customer", customer)
            .filter_op("status", "<>", SubscriptionStatus::Cancelled.as_str())
            .order_by("id", Direction::Desc)
            .first(&self.db)
            .await?;
        row.as_ref().map(subscription_from).transpose()
    }

    /// Is the customer served right now? Reads the subscription, not just its
    /// status, so a scheduler that has not run since grace ended does not
    /// keep a lapsed customer in.
    pub async fn is_served(&self, customer: &str) -> Result<bool> {
        let Some(subscription) = self.for_customer(customer).await? else {
            return Ok(false);
        };
        let grace = self.plan(&subscription.plan_id)?.grace.as_secs();
        Ok(subscription.is_served_at(now(), grace))
    }

    pub async fn invoice(&self, id: &str) -> Result<Option<Invoice>> {
        let row = self.db.table(INVOICES).filter("invoice_id", id).first(&self.db).await?;
        row.as_ref().map(invoice_from).transpose()
    }

    /// Every invoice of a subscription, newest first.
    pub async fn invoices(&self, subscription_id: &str) -> Result<Vec<Invoice>> {
        let rows = self
            .db
            .table(INVOICES)
            .filter("subscription_id", subscription_id)
            .order_by("number", Direction::Desc)
            .get(&self.db)
            .await?;
        rows.iter().map(invoice_from).collect()
    }

    /// The invoice the customer should pay now, if there is one.
    pub async fn open_invoice(&self, subscription_id: &str) -> Result<Option<Invoice>> {
        let row = self
            .db
            .table(INVOICES)
            .filter("subscription_id", subscription_id)
            .filter("status", InvoiceStatus::Open.as_str())
            .order_by("number", Direction::Desc)
            .first(&self.db)
            .await?;
        row.as_ref().map(invoice_from).transpose()
    }

    // ------------------------------------------------------------------
    // Starting and changing
    // ------------------------------------------------------------------

    /// Subscribe, and raise the first invoice with its charge. The customer
    /// has nothing until the invoice is paid — [`Billing::on_paid`] is what
    /// starts the clock.
    ///
    /// One live subscription per customer; a second is refused.
    pub async fn subscribe(&self, subscriber: Subscriber, plan_id: &str, channel: Channel) -> Result<Invoice> {
        let plan = self.plan(plan_id)?.clone();
        if let Some(existing) = self.for_customer(&subscriber.id).await? {
            return Err(Error::msg(format!(
                "{} already has subscription `{}` ({}); change or cancel it rather than adding another",
                subscriber.id,
                existing.id,
                existing.status.as_str()
            )));
        }

        let now = now();
        let subscription = Subscription {
            id: id("sub"),
            subscriber,
            plan_id: plan.id.clone(),
            next_plan_id: None,
            status: SubscriptionStatus::Pending,
            channel,
            period_start: 0,
            period_end: 0,
            cancel_at_period_end: false,
            created_at: now,
            updated_at: now,
        };
        self.db
            .table(SUBSCRIPTIONS)
            .insert(
                &self.db,
                &[
                    ("subscription_id", subscription.id.as_str().into()),
                    ("customer", subscription.subscriber.id.as_str().into()),
                    ("customer_name", subscription.subscriber.name.as_str().into()),
                    ("customer_email", opt(&subscription.subscriber.email)),
                    ("customer_phone", opt(&subscription.subscriber.phone)),
                    ("plan_id", plan.id.as_str().into()),
                    ("next_plan_id", Value::Null),
                    ("status", SubscriptionStatus::Pending.as_str().into()),
                    ("channel", channel.code().into()),
                    ("period_start", 0.into()),
                    ("period_end", 0.into()),
                    ("cancel_at_period_end", false.into()),
                    ("created_at", (now as i64).into()),
                    ("updated_at", (now as i64).into()),
                ],
            )
            .await?;

        let due = now + self.first_invoice_ttl.as_secs();
        let invoice = self
            .raise(&subscription, &plan, 1, 0, 0, due, now)
            .await?
            .ok_or_else(|| Error::msg("the first invoice of a brand-new subscription already existed"))?;
        Ok(self.charge_for(invoice, &subscription, &plan, now).await)
    }

    /// Take effect at the next cycle: the renewal is raised for the new plan
    /// and the subscription moves onto it when that invoice is paid. A
    /// renewal already raised for the old plan is voided and raised again.
    ///
    /// No proration. What a mid-cycle upgrade is worth is the application's
    /// policy; the honest primitive is a top-up through the ledger.
    pub async fn change_plan(&self, subscription_id: &str, plan_id: &str) -> Result<Subscription> {
        let plan = self.plan(plan_id)?.clone();
        let subscription = self.require(subscription_id).await?;
        if subscription.status == SubscriptionStatus::Cancelled {
            return Err(Error::msg(format!("subscription `{subscription_id}` is cancelled")));
        }
        let now = now();

        match subscription.status {
            // Nothing has been paid: the change is to the plan itself, and
            // the first invoice is raised again for the new price.
            SubscriptionStatus::Pending => {
                self.void_open_invoices(&subscription).await?;
                self.db
                    .table(SUBSCRIPTIONS)
                    .filter("subscription_id", subscription_id)
                    .update(
                        &self.db,
                        &[("plan_id", plan.id.as_str().into()), ("updated_at", (now as i64).into())],
                    )
                    .await?;
                let number = self.next_number(subscription_id).await?;
                let due = now + self.first_invoice_ttl.as_secs();
                if let Some(invoice) = self.raise(&subscription, &plan, number, 0, 0, due, now).await? {
                    self.charge_for(invoice, &subscription, &plan, now).await;
                }
            }
            _ => {
                let next = if plan.id == subscription.plan_id { Value::Null } else { plan.id.as_str().into() };
                self.db
                    .table(SUBSCRIPTIONS)
                    .filter("subscription_id", subscription_id)
                    .update(&self.db, &[("next_plan_id", next), ("updated_at", (now as i64).into())])
                    .await?;
                // A renewal already on the table is for the old plan.
                if self.void_open_invoices(&subscription).await? > 0 {
                    let mut refreshed = self.require(subscription_id).await?;
                    refreshed.next_plan_id = if plan.id == subscription.plan_id { None } else { Some(plan.id.clone()) };
                    self.raise_renewal(&refreshed, now).await?;
                }
            }
        }
        self.require(subscription_id).await
    }

    /// Stop at the end of the paid period. The renewal, if raised, is voided;
    /// access lasts until `period_end` and no longer. A subscription that
    /// has nothing paid — pending, or suspended — is cancelled at once.
    pub async fn cancel(&self, subscription_id: &str) -> Result<Subscription> {
        let subscription = self.require(subscription_id).await?;
        let now = now();
        self.void_open_invoices(&subscription).await?;
        let values: Vec<(&str, Value)> = match subscription.status {
            SubscriptionStatus::Active | SubscriptionStatus::Grace => {
                vec![("cancel_at_period_end", true.into()), ("updated_at", (now as i64).into())]
            }
            _ => vec![
                ("status", SubscriptionStatus::Cancelled.as_str().into()),
                ("cancel_at_period_end", true.into()),
                ("updated_at", (now as i64).into()),
            ],
        };
        self.db.table(SUBSCRIPTIONS).filter("subscription_id", subscription_id).update(&self.db, &values).await?;
        self.require(subscription_id).await
    }

    /// Stop now. Access ends immediately; nothing is refunded here.
    pub async fn cancel_now(&self, subscription_id: &str) -> Result<Subscription> {
        let subscription = self.require(subscription_id).await?;
        self.void_open_invoices(&subscription).await?;
        self.db
            .table(SUBSCRIPTIONS)
            .filter("subscription_id", subscription_id)
            .update(
                &self.db,
                &[
                    ("status", SubscriptionStatus::Cancelled.as_str().into()),
                    ("cancel_at_period_end", true.into()),
                    ("updated_at", (now() as i64).into()),
                ],
            )
            .await?;
        self.require(subscription_id).await
    }

    /// Bring a suspended subscription back: raise the invoice whose payment
    /// reactivates it. The new period starts when it is paid, not where the
    /// old one ended — the customer was not served in between.
    pub async fn renew(&self, subscription_id: &str, channel: Channel) -> Result<Invoice> {
        let subscription = self.require(subscription_id).await?;
        if subscription.status != SubscriptionStatus::Suspended {
            return Err(Error::msg(format!(
                "subscription `{subscription_id}` is {}; `renew` is for a suspended one. An active \
                 subscription's renewal is raised by `tick`, and a pending one has its first invoice",
                subscription.status.as_str()
            )));
        }
        if let Some(open) = self.open_invoice(subscription_id).await? {
            return Ok(open);
        }
        let now = now();
        self.set_channel(subscription_id, channel, now).await?;
        let plan = self.plan(subscription.next_plan_id.as_deref().unwrap_or(&subscription.plan_id))?.clone();
        let number = self.next_number(subscription_id).await?;
        let due = now + self.first_invoice_ttl.as_secs();
        let subscription = Subscription { channel, ..subscription };
        match self.raise(&subscription, &plan, number, 0, 0, due, now).await? {
            Some(invoice) => Ok(self.charge_for(invoice, &subscription, &plan, now).await),
            None => self
                .open_invoice(subscription_id)
                .await?
                .ok_or_else(|| Error::msg("the renewal was raised by another call and is not there")),
        }
    }

    /// Offer the same invoice through another channel: the customer chose a
    /// VA and now wants QRIS. The old charge is cancelled at the gateway.
    pub async fn reissue(&self, invoice_id: &str, channel: Channel) -> Result<Invoice> {
        let invoice = self.require_invoice(invoice_id).await?;
        if invoice.status != InvoiceStatus::Open {
            return Err(Error::msg(format!("invoice `{invoice_id}` is {}, not open", invoice.status.as_str())));
        }
        let subscription = self.require(&invoice.subscription_id).await?;
        let plan = self.plan(&invoice.plan_id)?.clone();
        let now = now();

        if let Some(charge_id) = &invoice.charge_id {
            // Best effort: a charge the gateway will not cancel expires on
            // its own, and paying it still pays this invoice.
            let _ = self.gateway.cancel_charge(charge_id).await;
        }
        self.set_channel(&subscription.id, channel, now).await?;
        self.db
            .table(INVOICES)
            .filter("invoice_id", invoice_id)
            .update(
                &self.db,
                &[("charge_id", Value::Null), ("channel", channel.code().into()), ("instructions", Value::Null)],
            )
            .await?;
        let invoice = Invoice { channel, charge_id: None, instructions: Instructions::default(), ..invoice };
        let subscription = Subscription { channel, ..subscription };
        Ok(self.charge_for(invoice, &subscription, &plan, now).await)
    }

    // ------------------------------------------------------------------
    // Money arriving
    // ------------------------------------------------------------------

    /// Hand a verified webhook event over. `Ok(None)` when it is not about an
    /// invoice of this crate's — a top-up, a transfer — so one receiver can
    /// serve both.
    pub async fn on_event(&self, event: &WebhookEvent) -> Result<Option<Paid>> {
        let Some(charge) = &event.charge else {
            return Ok(None);
        };
        if self.invoice(&charge.reference).await?.is_none() {
            return Ok(None);
        }
        match event.kind {
            EventKind::ChargePaid => self.on_paid(&charge.reference).await.map(Some),
            EventKind::ChargeExpired | EventKind::ChargeFailed => {
                self.on_charge_lost(&charge.reference).await?;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// An invoice was paid. Extend the subscription by one cycle and credit
    /// the plan's allowance.
    ///
    /// **Idempotent.** The credits are a ledger top-up under the invoice's id,
    /// and the invoice flips `open → paid` with the status in the `WHERE`; a
    /// retried webhook finds the top-up already there and the update
    /// changing nothing. The credits go in first, so a crash between the two
    /// leaves a state a retry completes rather than one it doubles.
    pub async fn on_paid(&self, invoice_id: &str) -> Result<Paid> {
        let invoice = self.require_invoice(invoice_id).await?;
        match invoice.status {
            InvoiceStatus::Open => {}
            InvoiceStatus::Paid => return Ok(Paid::Already),
            InvoiceStatus::Void | InvoiceStatus::Expired => return Ok(Paid::Unexpected { invoice }),
        }
        let subscription = self.require(&invoice.subscription_id).await?;
        let plan = self.plan(&invoice.plan_id)?.clone();
        let now = now();

        // Where the new period starts. A renewal paid early or in grace
        // starts where the last one ended — the customer was served
        // throughout. A first payment, or one that ends a suspension, starts
        // now: nothing was served before it.
        let start = if invoice.number == 1
            || invoice.period_start == 0
            || matches!(subscription.status, SubscriptionStatus::Pending | SubscriptionStatus::Suspended)
        {
            now
        } else {
            invoice.period_start
        };
        let end = start + plan.cycle.length().as_secs();

        if plan.credits_per_cycle > 0 {
            let lifetime = self.credits_expire.then(|| Duration::from_secs(end.saturating_sub(now).max(1)));
            self.ledger.top_up(&subscription.subscriber.id, plan.credits_per_cycle, &invoice.id, lifetime).await?;
        }

        let mut tx = self.db.begin().await?;
        let won = self
            .db
            .table(INVOICES)
            .filter("invoice_id", invoice_id)
            .filter("status", InvoiceStatus::Open.as_str())
            .update_in(
                &mut tx,
                &[
                    ("status", InvoiceStatus::Paid.as_str().into()),
                    ("period_key", closed_key(InvoiceStatus::Paid, invoice_id).into()),
                    ("paid_at", (now as i64).into()),
                    ("period_start", (start as i64).into()),
                    ("period_end", (end as i64).into()),
                ],
            )
            .await?;
        if won == 0 {
            tx.rollback().await?;
            return Ok(Paid::Already);
        }
        self.db
            .table(SUBSCRIPTIONS)
            .filter("subscription_id", subscription.id.as_str())
            .update_in(
                &mut tx,
                &[
                    ("status", SubscriptionStatus::Active.as_str().into()),
                    ("plan_id", plan.id.as_str().into()),
                    ("next_plan_id", Value::Null),
                    ("period_start", (start as i64).into()),
                    ("period_end", (end as i64).into()),
                    ("updated_at", (now as i64).into()),
                ],
            )
            .await?;
        tx.commit().await?;

        let subscription = self.require(&subscription.id).await?;
        let invoice = self.require_invoice(invoice_id).await?;
        Ok(Paid::Extended { subscription, invoice })
    }

    /// The gateway expired or failed the charge. The invoice stays open and
    /// loses its charge; the next tick creates a fresh one, so a renewal
    /// keeps a live way to pay for as long as it is payable. A first invoice
    /// is different: nothing was ever paid, and the subscription is closed.
    pub async fn on_charge_lost(&self, invoice_id: &str) -> Result<()> {
        let invoice = self.require_invoice(invoice_id).await?;
        if invoice.status != InvoiceStatus::Open {
            return Ok(());
        }
        let subscription = self.require(&invoice.subscription_id).await?;
        if subscription.status == SubscriptionStatus::Pending {
            self.expire_invoice(&invoice.id).await?;
            self.db
                .table(SUBSCRIPTIONS)
                .filter("subscription_id", subscription.id.as_str())
                .filter("status", SubscriptionStatus::Pending.as_str())
                .update(
                    &self.db,
                    &[
                        ("status", SubscriptionStatus::Cancelled.as_str().into()),
                        ("updated_at", (now() as i64).into()),
                    ],
                )
                .await?;
            return Ok(());
        }
        self.db
            .table(INVOICES)
            .filter("invoice_id", invoice_id)
            .filter("status", InvoiceStatus::Open.as_str())
            .update(&self.db, &[("charge_id", Value::Null), ("instructions", Value::Null)])
            .await?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // The schedule
    // ------------------------------------------------------------------

    /// Run the loop once. Schedule this — every few minutes is plenty.
    ///
    /// Safe to run from two places at once: every transition is a guarded
    /// `UPDATE`, every invoice number and every reminder is claimed through a
    /// unique index, and whichever tick loses a claim skips the item.
    pub async fn tick(&self) -> Result<Tick> {
        self.tick_at(now()).await
    }

    /// [`Billing::tick`] as of a given moment. For tests, and for replaying
    /// a schedule that was down.
    pub async fn tick_at(&self, now: u64) -> Result<Tick> {
        let mut tick = Tick::default();

        // 1. Renewals for periods about to end.
        let horizon = now + self.lead.as_secs();
        let rows = self
            .db
            .table(SUBSCRIPTIONS)
            .filter("status", SubscriptionStatus::Active.as_str())
            .filter("cancel_at_period_end", false)
            .filter_op("period_end", "<=", horizon as i64)
            .get(&self.db)
            .await?;
        for row in &rows {
            let subscription = subscription_from(row)?;
            if self.raise_renewal(&subscription, now).await?.is_some() {
                tick.invoices_raised += 1;
            }
        }

        // 2. Open invoices with no charge — the gateway was down, or expired
        //    the last one.
        let rows = self
            .db
            .table(INVOICES)
            .filter("status", InvoiceStatus::Open.as_str())
            .filter_null("charge_id")
            .get(&self.db)
            .await?;
        for row in &rows {
            let invoice = invoice_from(row)?;
            let Some(subscription) = self.subscription(&invoice.subscription_id).await? else { continue };
            let Ok(plan) = self.plan(&invoice.plan_id) else { continue };
            let plan = plan.clone();
            if self.attach_charge(invoice, &subscription, &plan, now).await.1 {
                tick.charges_created += 1;
            }
        }

        // 3. Periods that ended: grace, or the end the customer asked for.
        let rows = self
            .db
            .table(SUBSCRIPTIONS)
            .filter("status", SubscriptionStatus::Active.as_str())
            .filter_op("period_end", "<=", now as i64)
            .get(&self.db)
            .await?;
        for row in &rows {
            let subscription = subscription_from(row)?;
            let to = if subscription.cancel_at_period_end {
                SubscriptionStatus::Cancelled
            } else {
                SubscriptionStatus::Grace
            };
            if !self.move_status(&subscription.id, SubscriptionStatus::Active, to, now).await? {
                continue;
            }
            match to {
                SubscriptionStatus::Cancelled => tick.cancelled += 1,
                _ => {
                    tick.moved_to_grace += 1;
                    if let Some(invoice) = self.open_invoice(&subscription.id).await? {
                        self.remind(&mut tick, ReminderKind::Overdue, &subscription, invoice, now).await?;
                    }
                }
            }
        }

        // 4. Grace that ran out.
        let rows = self
            .db
            .table(SUBSCRIPTIONS)
            .filter("status", SubscriptionStatus::Grace.as_str())
            .get(&self.db)
            .await?;
        for row in &rows {
            let subscription = subscription_from(row)?;
            let grace = self.plan(&subscription.plan_id)?.grace.as_secs();
            if now < subscription.period_end.saturating_add(grace) {
                continue;
            }
            let to = if subscription.cancel_at_period_end {
                SubscriptionStatus::Cancelled
            } else {
                SubscriptionStatus::Suspended
            };
            if !self.move_status(&subscription.id, SubscriptionStatus::Grace, to, now).await? {
                continue;
            }
            let open = self.open_invoice(&subscription.id).await?;
            if let Some(invoice) = &open {
                self.expire_invoice(&invoice.id).await?;
            }
            match to {
                SubscriptionStatus::Cancelled => tick.cancelled += 1,
                _ => {
                    tick.suspended += 1;
                    if let Some(invoice) = open {
                        let invoice = Invoice { status: InvoiceStatus::Expired, ..invoice };
                        self.remind(&mut tick, ReminderKind::Suspended, &subscription, invoice, now).await?;
                    }
                }
            }
        }

        // 5. First invoices nobody paid.
        let rows = self
            .db
            .table(SUBSCRIPTIONS)
            .filter("status", SubscriptionStatus::Pending.as_str())
            .get(&self.db)
            .await?;
        for row in &rows {
            let subscription = subscription_from(row)?;
            let Some(invoice) = self.open_invoice(&subscription.id).await? else {
                // Voided and never re-raised, or cancelled mid-way: close it.
                if self.move_status(&subscription.id, SubscriptionStatus::Pending, SubscriptionStatus::Cancelled, now).await? {
                    tick.cancelled += 1;
                }
                continue;
            };
            if invoice.due_at > now {
                continue;
            }
            if self.move_status(&subscription.id, SubscriptionStatus::Pending, SubscriptionStatus::Cancelled, now).await? {
                self.expire_invoice(&invoice.id).await?;
                tick.cancelled += 1;
            }
        }

        // 6. Renewals coming due.
        let rows = self
            .db
            .table(INVOICES)
            .filter("status", InvoiceStatus::Open.as_str())
            .filter_op("number", ">", 1)
            .filter_op("due_at", ">", now as i64)
            .get(&self.db)
            .await?;
        for row in &rows {
            let invoice = invoice_from(row)?;
            let Some(subscription) = self.subscription(&invoice.subscription_id).await? else { continue };
            if subscription.status != SubscriptionStatus::Active {
                continue;
            }
            let left = invoice.due_at - now;
            for before in &self.remind_before {
                if left <= before.as_secs() {
                    let kind = ReminderKind::Due { days_left: before.as_secs() / 86_400 };
                    self.remind(&mut tick, kind, &subscription, invoice.clone(), now).await?;
                }
            }
        }

        Ok(tick)
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    async fn require(&self, subscription_id: &str) -> Result<Subscription> {
        self.subscription(subscription_id)
            .await?
            .ok_or_else(|| Error::msg(format!("no subscription `{subscription_id}`")))
    }

    async fn require_invoice(&self, invoice_id: &str) -> Result<Invoice> {
        self.invoice(invoice_id).await?.ok_or_else(|| Error::msg(format!("no invoice `{invoice_id}`")))
    }

    async fn next_number(&self, subscription_id: &str) -> Result<i64> {
        let latest = self
            .db
            .table(INVOICES)
            .filter("subscription_id", subscription_id)
            .order_by("number", Direction::Desc)
            .first(&self.db)
            .await?;
        Ok(latest.map(|row| row.get::<i64>("number").unwrap_or(0)).unwrap_or(0) + 1)
    }

    /// Raise the renewal for the period after the current one, unless it is
    /// already there. `None` when it was.
    async fn raise_renewal(&self, subscription: &Subscription, now: u64) -> Result<Option<Invoice>> {
        let plan = self.plan(subscription.next_plan_id.as_deref().unwrap_or(&subscription.plan_id))?.clone();
        let already = self
            .db
            .table(INVOICES)
            .filter("subscription_id", subscription.id.as_str())
            .filter("period_start", subscription.period_end as i64)
            .filter_in(
                "status",
                vec![InvoiceStatus::Open.as_str().into(), InvoiceStatus::Paid.as_str().into()],
            )
            .exists(&self.db)
            .await?;
        if already {
            return Ok(None);
        }
        let number = self.next_number(&subscription.id).await?;
        let start = subscription.period_end;
        let end = start + plan.cycle.length().as_secs();
        let Some(invoice) = self.raise(subscription, &plan, number, start, end, start, now).await? else {
            return Ok(None);
        };
        Ok(Some(self.charge_for(invoice, subscription, &plan, now).await))
    }

    /// Insert the invoice row. `None` when another call raised an invoice
    /// for the same period first — the unique index on
    /// `(subscription_id, period_key)` decided. Found by racing eight ticks:
    /// a check on "is there one already" followed by an insert let a sixth
    /// renewal through for five subscriptions, because the check ran before
    /// the other tick's insert landed.
    #[allow(clippy::too_many_arguments)]
    async fn raise(
        &self,
        subscription: &Subscription,
        plan: &Plan,
        number: i64,
        period_start: u64,
        period_end: u64,
        due_at: u64,
        now: u64,
    ) -> Result<Option<Invoice>> {
        let invoice = Invoice {
            id: id("inv"),
            subscription_id: subscription.id.clone(),
            customer: subscription.subscriber.id.clone(),
            plan_id: plan.id.clone(),
            number,
            amount: plan.price.clone(),
            credits: plan.credits_per_cycle,
            period_start,
            period_end,
            status: InvoiceStatus::Open,
            charge_id: None,
            channel: subscription.channel,
            instructions: Instructions::default(),
            due_at,
            paid_at: None,
            created_at: now,
        };
        let inserted = self
            .db
            .table(INVOICES)
            .insert(
                &self.db,
                &[
                    ("invoice_id", invoice.id.as_str().into()),
                    ("subscription_id", invoice.subscription_id.as_str().into()),
                    ("customer", invoice.customer.as_str().into()),
                    ("plan_id", invoice.plan_id.as_str().into()),
                    ("number", number.into()),
                    ("amount_minor", invoice.amount.minor.into()),
                    ("currency", invoice.amount.currency.as_str().into()),
                    ("credits", invoice.credits.into()),
                    ("period_start", (period_start as i64).into()),
                    ("period_end", (period_end as i64).into()),
                    ("period_key", period_start.to_string().into()),
                    ("status", InvoiceStatus::Open.as_str().into()),
                    ("charge_id", Value::Null),
                    ("channel", invoice.channel.code().into()),
                    ("instructions", Value::Null),
                    ("due_at", (due_at as i64).into()),
                    ("paid_at", Value::Null),
                    ("created_at", (now as i64).into()),
                ],
            )
            .await;
        match inserted {
            Ok(_) => Ok(Some(invoice)),
            Err(_) => Ok(None),
        }
    }

    /// Create the charge at the gateway and store how to pay. On failure the
    /// invoice keeps `charge_id = NULL`, which the next tick retries; the
    /// invoice is returned as it stands, and `is_payable()` says which.
    async fn charge_for(&self, invoice: Invoice, subscription: &Subscription, plan: &Plan, now: u64) -> Invoice {
        self.attach_charge(invoice, subscription, plan, now).await.0
    }

    /// [`Billing::charge_for`], also saying whether *this call* stored the
    /// charge. `false` covers both a gateway failure and losing to another
    /// tick that stored its own — in which case ours is cancelled and the
    /// invoice comes back carrying theirs.
    async fn attach_charge(
        &self,
        invoice: Invoice,
        subscription: &Subscription,
        plan: &Plan,
        now: u64,
    ) -> (Invoice, bool) {
        let grace = plan.grace.as_secs();
        let payable_for = (invoice.due_at + grace).saturating_sub(now).max(600);
        let request = ChargeRequest::new(invoice.id.clone(), invoice.amount.clone(), invoice.channel)
            .expires_in(Duration::from_secs(payable_for))
            .customer(Customer {
                name: subscription.subscriber.name.clone(),
                email: subscription.subscriber.email.clone(),
                phone: subscription.subscriber.phone.clone(),
            })
            .description(format!("{} — invoice #{}", plan.name, invoice.number))
            .metadata(Json::object([
                ("subscription_id", Json::from(subscription.id.as_str())),
                ("invoice_id", Json::from(invoice.id.as_str())),
                ("plan", Json::from(plan.id.as_str())),
            ]));
        let Ok(charge) = self.gateway.create_charge(&request).await else {
            return (invoice, false);
        };
        let stored = self
            .db
            .table(INVOICES)
            .filter("invoice_id", invoice.id.as_str())
            .filter("status", InvoiceStatus::Open.as_str())
            .filter_null("charge_id")
            .update(
                &self.db,
                &[
                    ("charge_id", charge.id.as_str().into()),
                    ("instructions", Value::from(instructions_to_json(&charge.instructions))),
                ],
            )
            .await;
        match stored {
            // Another tick got its charge in first: ours is surplus.
            Ok(0) | Err(_) => {
                let _ = self.gateway.cancel_charge(&charge.id).await;
                (self.invoice(&invoice.id).await.ok().flatten().unwrap_or(invoice), false)
            }
            Ok(_) => (Invoice { charge_id: Some(charge.id), instructions: charge.instructions, ..invoice }, true),
        }
    }

    /// Void every open invoice of a subscription, cancelling their charges.
    /// Returns how many.
    async fn void_open_invoices(&self, subscription: &Subscription) -> Result<usize> {
        let rows = self
            .db
            .table(INVOICES)
            .filter("subscription_id", subscription.id.as_str())
            .filter("status", InvoiceStatus::Open.as_str())
            .get(&self.db)
            .await?;
        let mut voided = 0;
        for row in &rows {
            let invoice = invoice_from(row)?;
            let won = self
                .db
                .table(INVOICES)
                .filter("invoice_id", invoice.id.as_str())
                .filter("status", InvoiceStatus::Open.as_str())
                .update(
                    &self.db,
                    &[
                        ("status", InvoiceStatus::Void.as_str().into()),
                        ("period_key", closed_key(InvoiceStatus::Void, &invoice.id).into()),
                    ],
                )
                .await?;
            if won == 1 {
                voided += 1;
                if let Some(charge_id) = &invoice.charge_id {
                    let _ = self.gateway.cancel_charge(charge_id).await;
                }
            }
        }
        Ok(voided)
    }

    async fn expire_invoice(&self, invoice_id: &str) -> Result<()> {
        let Some(invoice) = self.invoice(invoice_id).await? else { return Ok(()) };
        let won = self
            .db
            .table(INVOICES)
            .filter("invoice_id", invoice_id)
            .filter("status", InvoiceStatus::Open.as_str())
            .update(
                &self.db,
                &[
                    ("status", InvoiceStatus::Expired.as_str().into()),
                    ("period_key", closed_key(InvoiceStatus::Expired, invoice_id).into()),
                ],
            )
            .await?;
        if won == 1 && let Some(charge_id) = &invoice.charge_id {
            let _ = self.gateway.cancel_charge(charge_id).await;
        }
        Ok(())
    }

    /// `from → to`, if it is still `from`. `true` when this call moved it.
    async fn move_status(
        &self,
        subscription_id: &str,
        from: SubscriptionStatus,
        to: SubscriptionStatus,
        now: u64,
    ) -> Result<bool> {
        let won = self
            .db
            .table(SUBSCRIPTIONS)
            .filter("subscription_id", subscription_id)
            .filter("status", from.as_str())
            .update(&self.db, &[("status", to.as_str().into()), ("updated_at", (now as i64).into())])
            .await?;
        Ok(won == 1)
    }

    async fn set_channel(&self, subscription_id: &str, channel: Channel, now: u64) -> Result<()> {
        self.db
            .table(SUBSCRIPTIONS)
            .filter("subscription_id", subscription_id)
            .update(&self.db, &[("channel", channel.code().into()), ("updated_at", (now as i64).into())])
            .await?;
        Ok(())
    }

    /// Emit a reminder if nobody has. The insert into the reminders table is
    /// the claim.
    async fn remind(
        &self,
        tick: &mut Tick,
        kind: ReminderKind,
        subscription: &Subscription,
        invoice: Invoice,
        now: u64,
    ) -> Result<()> {
        let claimed = self
            .db
            .table(REMINDERS)
            .insert(
                &self.db,
                &[
                    ("invoice_id", invoice.id.as_str().into()),
                    ("kind", kind.key().into()),
                    ("sent_at", (now as i64).into()),
                ],
            )
            .await;
        if claimed.is_err() {
            return Ok(());
        }
        let plan = self.plan(&invoice.plan_id)?.clone();
        tick.reminders.push(Reminder {
            kind,
            subscriber: subscription.subscriber.clone(),
            subscription_id: subscription.id.clone(),
            plan,
            invoice,
        });
        Ok(())
    }
}

fn subscription_from(row: &Row) -> Result<Subscription> {
    let status: String = row.get("status")?;
    let channel: String = row.get("channel")?;
    Ok(Subscription {
        id: row.get("subscription_id")?,
        subscriber: Subscriber {
            id: row.get("customer")?,
            name: row.get("customer_name")?,
            email: row.get::<String>("customer_email").ok(),
            phone: row.get::<String>("customer_phone").ok(),
        },
        plan_id: row.get("plan_id")?,
        next_plan_id: row.get::<String>("next_plan_id").ok(),
        status: SubscriptionStatus::parse(&status)
            .ok_or_else(|| Error::msg(format!("a subscription row holds the status `{status}`, which this crate does not know")))?,
        channel: Channel::parse(&channel)
            .ok_or_else(|| Error::msg(format!("a subscription row holds the channel `{channel}`, which this crate does not know")))?,
        period_start: unsigned(row.get("period_start")?),
        period_end: unsigned(row.get("period_end")?),
        cancel_at_period_end: row.get("cancel_at_period_end")?,
        created_at: unsigned(row.get("created_at")?),
        updated_at: unsigned(row.get("updated_at")?),
    })
}

fn invoice_from(row: &Row) -> Result<Invoice> {
    let status: String = row.get("status")?;
    let channel: String = row.get("channel")?;
    Ok(Invoice {
        id: row.get("invoice_id")?,
        subscription_id: row.get("subscription_id")?,
        customer: row.get("customer")?,
        plan_id: row.get("plan_id")?,
        number: row.get("number")?,
        amount: Money::new(row.get("amount_minor")?, row.get::<String>("currency")?),
        credits: row.get("credits")?,
        period_start: unsigned(row.get("period_start")?),
        period_end: unsigned(row.get("period_end")?),
        status: InvoiceStatus::parse(&status)
            .ok_or_else(|| Error::msg(format!("an invoice row holds the status `{status}`, which this crate does not know")))?,
        charge_id: row.get::<String>("charge_id").ok(),
        channel: Channel::parse(&channel)
            .ok_or_else(|| Error::msg(format!("an invoice row holds the channel `{channel}`, which this crate does not know")))?,
        instructions: row.get::<Json>("instructions").map(|json| instructions_from_json(&json)).unwrap_or_default(),
        due_at: unsigned(row.get("due_at")?),
        paid_at: row.get::<i64>("paid_at").ok().map(unsigned),
        created_at: unsigned(row.get("created_at")?),
    })
}

/// The period key of an invoice that is no longer open: unique to it, so it
/// holds no period against the invoice raised in its place.
fn closed_key(status: InvoiceStatus, invoice_id: &str) -> String {
    format!("{}:{invoice_id}", status.as_str())
}

fn unsigned(value: i64) -> u64 {
    value.max(0) as u64
}

fn opt(value: &Option<String>) -> Value {
    value.as_deref().map_or(Value::Null, Value::from)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Ids that sort by time and never collide within a process: time, a
/// counter, the pid. Not secrets, so not from the CSPRNG.
fn id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{prefix}_{nanos:x}{:x}{:x}", COUNTER.fetch_add(1, Ordering::Relaxed), std::process::id())
}
