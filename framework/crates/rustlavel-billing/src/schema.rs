//! The three tables.

use rustlavel_core::Result;
use rustlavel_db::{Schema, Table};

pub const SUBSCRIPTIONS: &str = "billing_subscriptions";
pub const INVOICES: &str = "billing_invoices";
pub const REMINDERS: &str = "billing_reminders";

pub fn define_subscriptions(t: &mut Table) {
    t.id();
    t.string("subscription_id").unique();
    t.string("customer");
    t.string("customer_name");
    t.string("customer_email").nullable();
    t.string("customer_phone").nullable();
    t.string("plan_id");
    t.string("next_plan_id").nullable();
    // The status is the lock for every transition: `UPDATE … WHERE status =
    // 'active'` and one row changed means this scheduler moved it, not the
    // other one running at the same moment.
    t.string("status");
    t.string("channel");
    t.big_integer("period_start").default_int(0);
    t.big_integer("period_end").default_int(0);
    t.boolean("cancel_at_period_end").default_bool(false);
    t.big_integer("created_at");
    t.big_integer("updated_at");
    t.index(&["customer"]);
    t.index(&["status", "period_end"]);
}

pub fn define_invoices(t: &mut Table) {
    t.id();
    t.string("invoice_id").unique();
    t.string("subscription_id");
    t.string("customer");
    t.string("plan_id");
    t.big_integer("number");
    t.big_integer("amount_minor");
    t.string("currency");
    t.big_integer("credits");
    t.big_integer("period_start").default_int(0);
    t.big_integer("period_end").default_int(0);
    // The claim. While an invoice is open this is the period it is for, so
    // the unique index below allows one open invoice per period per
    // subscription — two schedulers raising the same renewal both write the
    // same key and one is refused. When the invoice stops being open the key
    // is rewritten to `<status>:<invoice_id>`, which collides with nothing,
    // so a voided renewal does not block the one raised in its place.
    t.string("period_key");
    // `UPDATE … WHERE status = 'open'` on payment: one row changed means this
    // webhook is the one that paid it, and a retry changes nothing.
    t.string("status");
    t.string("charge_id").nullable();
    t.string("channel");
    t.json("instructions").nullable();
    t.big_integer("due_at");
    t.big_integer("paid_at").nullable();
    t.big_integer("created_at");
    t.unique(&["subscription_id", "period_key"]);
    // Numbers are read-then-incremented; the period key is what makes that
    // safe, but a duplicate number would still be a lie on a receipt.
    t.unique(&["subscription_id", "number"]);
    t.index(&["status", "due_at"]);
}

pub fn define_reminders(t: &mut Table) {
    t.id();
    t.string("invoice_id");
    t.string("kind");
    t.big_integer("sent_at");
    // A reminder is emitted once. The insert is the claim; the index is the
    // rule.
    t.unique(&["invoice_id", "kind"]);
}

pub async fn create_tables(schema: &Schema<'_>) -> Result<()> {
    schema.create(SUBSCRIPTIONS, define_subscriptions).await?;
    schema.create(INVOICES, define_invoices).await?;
    schema.create(REMINDERS, define_reminders).await
}

pub async fn drop_tables(schema: &Schema<'_>) -> Result<()> {
    schema.drop(REMINDERS).await?;
    schema.drop(INVOICES).await?;
    schema.drop(SUBSCRIPTIONS).await
}

rustlavel_db::migration!(
    CreateBillingTables,
    "2026_09_17_000200_create_billing_tables",
    up: |schema| { crate::schema::create_tables(schema).await },
    down: |schema| { crate::schema::drop_tables(schema).await },
);
