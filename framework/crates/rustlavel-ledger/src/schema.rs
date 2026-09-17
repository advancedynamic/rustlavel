//! The five tables.

use rustlavel_core::Result;
use rustlavel_db::{Schema, Table};

pub const ACCOUNTS: &str = "ledger_accounts";
pub const TRANSFERS: &str = "ledger_transfers";
pub const ENTRIES: &str = "ledger_entries";
pub const HOLDS: &str = "ledger_holds";
pub const LOTS: &str = "ledger_lots";

pub fn define_accounts(t: &mut Table) {
    t.id();
    t.string("account_id").unique();
    t.string("owner");
    t.string("unit");
    t.big_integer("balance").default_int(0);
    t.big_integer("held").default_int(0);
    t.big_integer("created_at");
    // One account per owner per unit. The index is the rule.
    t.unique(&["owner", "unit"]);
}

pub fn define_transfers(t: &mut Table) {
    t.id();
    t.string("transfer_id").unique();
    // Unique: this index is idempotency. A second transfer under a reference
    // already used cannot be inserted, whatever else is racing.
    t.string("reference").unique();
    t.string("kind");
    t.string("from_account");
    t.string("to_account");
    t.big_integer("amount");
    t.big_integer("created_at");
    t.json("metadata").nullable();
}

pub fn define_entries(t: &mut Table) {
    t.id();
    t.string("transfer_id");
    t.string("account_id");
    t.big_integer("amount");
    t.big_integer("balance_after");
    t.string("kind");
    t.string("reference");
    t.big_integer("created_at");
    t.index(&["account_id", "id"]);
}

pub fn define_holds(t: &mut Table) {
    t.id();
    t.string("hold_id").unique();
    t.string("account_id");
    t.big_integer("amount");
    // This column is the lock: capture and release are
    // `UPDATE … WHERE status = 'held'`, and one row changed means this call
    // won.
    t.string("status");
    t.string("reference").unique();
    t.big_integer("created_at");
    t.big_integer("expires_at");
    t.index(&["status", "expires_at"]);
}

pub fn define_lots(t: &mut Table) {
    t.id();
    t.string("lot_id").unique();
    t.string("account_id");
    t.string("transfer_id");
    t.big_integer("initial");
    t.big_integer("remaining");
    // Null never expires.
    t.big_integer("expires_at").nullable();
    t.big_integer("created_at");
    t.index(&["account_id", "expires_at"]);
}

pub async fn create_tables(schema: &Schema<'_>) -> Result<()> {
    schema.create(ACCOUNTS, define_accounts).await?;
    schema.create(TRANSFERS, define_transfers).await?;
    schema.create(ENTRIES, define_entries).await?;
    schema.create(HOLDS, define_holds).await?;
    schema.create(LOTS, define_lots).await
}

pub async fn drop_tables(schema: &Schema<'_>) -> Result<()> {
    schema.drop(LOTS).await?;
    schema.drop(HOLDS).await?;
    schema.drop(ENTRIES).await?;
    schema.drop(TRANSFERS).await?;
    schema.drop(ACCOUNTS).await
}

rustlavel_db::migration!(
    CreateLedgerTables,
    "2026_09_17_000100_create_ledger_tables",
    up: |schema| { crate::schema::create_tables(schema).await },
    down: |schema| { crate::schema::drop_tables(schema).await },
);
