//! rustlavel-billing: subscriptions where the customer pays, rather than is
//! charged.
//!
//! In a market where cards are rare and virtual accounts, QRIS and e-wallets
//! are how people pay, a subscription cannot pull money. It can only *ask*:
//! each cycle it raises an invoice, presents a way to pay it, and waits. This
//! crate is that loop.
//!
//! # The cycle
//!
//! 1. [`Billing::subscribe`] creates the subscription, its first invoice, and
//!    a charge at the payment gateway — the VA number or QR the customer pays.
//! 2. The gateway's webhook says paid. The application's receiver calls
//!    [`Billing::on_paid`], which marks the invoice paid, extends the
//!    subscription by one cycle, and credits the plan's allowance through the
//!    ledger — idempotently, under the invoice's id, so a retried webhook
//!    credits once.
//! 3. [`Billing::tick`] runs on a schedule. When a period ends it raises the
//!    next invoice and charge; while an invoice is unpaid past the period's
//!    end the subscription is in grace; when grace runs out it is suspended.
//!    `tick` returns the [`Reminder`]s that fell due, and the application
//!    sends them — this crate does not know what the mail should say.
//!
//! # What this does not do
//!
//! Overage. A customer who wants more credits than the plan gives buys a
//! top-up, which is one charge and one `Ledger::top_up` and needs no machinery
//! here. Proration on plan change is a policy decision this crate does not
//! make for you: [`Billing::change_plan`] takes effect at the next cycle.

pub mod billing;
pub mod plan;
pub mod schema;
pub mod subscription;

pub use billing::{Billing, Paid, Reminder, ReminderKind, Tick};
pub use plan::{Cycle, Plan};
pub use schema::{CreateBillingTables, create_tables, drop_tables};
pub use subscription::{Invoice, InvoiceStatus, Subscriber, Subscription, SubscriptionStatus};
pub use rustlavel_payment::{Channel, Instructions, Money};

pub use rustlavel_core::{Error, Result};
