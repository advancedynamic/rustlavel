//! The operations. Every one is a single transaction.

use crate::account::{Account, Balance, SYSTEM_CONSUMPTION, SYSTEM_EXPIRY, SYSTEM_TOPUP};
use crate::entry::{Entry, Transfer, TransferKind};
use crate::hold::{Hold, HoldStatus};
use crate::schema::{ACCOUNTS, ENTRIES, HOLDS, LOTS, TRANSFERS};
use rustlavel_core::{Error, Json, Result};
use rustlavel_db::{Database, Row, Transaction, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The book.
#[derive(Clone)]
pub struct Ledger {
    db: Database,
    /// What the accounts count. One ledger per unit; two units are two books.
    unit: String,
}

/// What a sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sweep {
    /// Holds past their lifetime that were released.
    pub holds_released: usize,
    /// Lots past their expiry whose remainder was moved to `system:expiry`.
    pub lots_expired: usize,
    /// The total moved to expiry.
    pub amount_expired: i64,
}

impl Ledger {
    /// A ledger counting `unit` — `"credits"`, `"IDR"`.
    pub fn new(db: Database, unit: impl Into<String>) -> Ledger {
        Ledger { db, unit: unit.into() }
    }

    pub fn unit(&self) -> &str {
        &self.unit
    }

    // ------------------------------------------------------------------
    // Reading
    // ------------------------------------------------------------------

    /// The owner's account, created if this is the first time it is asked for.
    pub async fn account(&self, owner: &str) -> Result<Account> {
        let mut tx = self.db.begin().await?;
        let account = self.account_in(&mut tx, owner).await?;
        tx.commit().await?;
        Ok(account)
    }

    pub async fn balance(&self, owner: &str) -> Result<Balance> {
        Ok(self.account(owner).await?.balance_view())
    }

    /// The most recent movements on an account, newest first.
    pub async fn history(&self, owner: &str, limit: usize) -> Result<Vec<Entry>> {
        let account = self.account(owner).await?;
        let rows = self
            .db
            .table(ENTRIES)
            .filter("account_id", account.id.as_str())
            .order_by("id", rustlavel_db::Direction::Desc)
            .limit(limit.clamp(1, 1000) as i64)
            .get(&self.db)
            .await?;
        rows.iter().map(entry_from).collect()
    }

    pub async fn transfer(&self, reference: &str) -> Result<Option<Transfer>> {
        let row = self.db.table(TRANSFERS).filter("reference", reference).first(&self.db).await?;
        row.as_ref().map(transfer_from).transpose()
    }

    pub async fn hold_by_reference(&self, reference: &str) -> Result<Option<Hold>> {
        let row = self.db.table(HOLDS).filter("reference", reference).first(&self.db).await?;
        row.as_ref().map(hold_from).transpose()
    }

    // ------------------------------------------------------------------
    // Money in
    // ------------------------------------------------------------------

    /// Credit an owner, from `system:topup`.
    ///
    /// **Idempotent on `reference`.** Call it twice with the same reference —
    /// a payment webhook that was retried — and the second call returns the
    /// first transfer and moves nothing. The unique index on the reference is
    /// what enforces this, so it holds under concurrency too.
    ///
    /// `expires_in` gives the credits a lifetime. Consumption draws from the
    /// batch that expires first.
    pub async fn top_up(
        &self,
        owner: &str,
        amount: i64,
        reference: &str,
        expires_in: Option<Duration>,
    ) -> Result<Transfer> {
        positive(amount)?;
        if let Some(existing) = self.transfer(reference).await? {
            return Ok(existing);
        }

        let mut tx = self.db.begin().await?;
        let to = self.account_in(&mut tx, owner).await?;
        let from = self.account_in(&mut tx, SYSTEM_TOPUP).await?;
        let now = now();

        // System accounts may go negative — that is what they are for. Every
        // credit in the book is a debit here, and the sum is zero.
        debit_unchecked(&mut tx, &from.id, amount).await?;
        credit(&mut tx, &to.id, amount).await?;

        let transfer = self
            .record(&mut tx, TransferKind::TopUp, reference, &from.id, &to.id, amount, Json::Null, now)
            .await?;
        let Some(transfer) = transfer else {
            // Lost the race to another call with the same reference. Undo ours
            // and hand back theirs.
            tx.rollback().await?;
            return self.transfer(reference).await?.ok_or_else(|| {
                Error::msg("a top-up lost an idempotency race and the winner is not there")
            });
        };

        self.db
            .table(LOTS)
            .insert_in(
                &mut tx,
                &[
                    ("lot_id", id("lot").as_str().into()),
                    ("account_id", to.id.as_str().into()),
                    ("transfer_id", transfer.id.as_str().into()),
                    ("initial", amount.into()),
                    ("remaining", amount.into()),
                    (
                        "expires_at",
                        expires_in.map_or(Value::Null, |d| Value::from((now + d.as_secs()) as i64)),
                    ),
                    ("created_at", (now as i64).into()),
                ],
            )
            .await?;

        tx.commit().await?;
        Ok(transfer)
    }

    // ------------------------------------------------------------------
    // Money out
    // ------------------------------------------------------------------

    /// Spend, immediately. Fails — and moves nothing — when `available` is
    /// short.
    ///
    /// The debit's `WHERE balance - held >= amount` is both the check and the
    /// lock: two calls racing for the last credit reach the database, the row
    /// lock serialises them, and the second sees the balance the first left.
    /// There is no read before the write, because the gap between them is
    /// where a double-spend lives.
    pub async fn consume(&self, owner: &str, amount: i64, reference: &str) -> Result<Transfer> {
        positive(amount)?;
        if let Some(existing) = self.transfer(reference).await? {
            return Ok(existing);
        }

        let mut tx = self.db.begin().await?;
        let from = self.account_in(&mut tx, owner).await?;
        let to = self.account_in(&mut tx, SYSTEM_CONSUMPTION).await?;

        if !debit_available(&mut tx, &from.id, amount).await? {
            tx.rollback().await?;
            return Err(insufficient(owner, amount, from.available()));
        }
        credit(&mut tx, &to.id, amount).await?;
        draw_lots(&mut tx, &from.id, amount).await?;

        let transfer = self
            .record(&mut tx, TransferKind::Consume, reference, &from.id, &to.id, amount, Json::Null, now())
            .await?;
        let Some(transfer) = transfer else {
            tx.rollback().await?;
            return self.transfer(reference).await?.ok_or_else(|| {
                Error::msg("a consume lost an idempotency race and the winner is not there")
            });
        };

        tx.commit().await?;
        Ok(transfer)
    }

    /// Reserve, for a job that will cost this much and take a while.
    ///
    /// `available` drops by the amount; `balance` does not. The hold lasts
    /// `ttl`; after that [`Ledger::sweep`] releases it, so a job that died
    /// without saying so does not keep credits reserved forever. Idempotent
    /// on `reference`.
    pub async fn hold(&self, owner: &str, amount: i64, reference: &str, ttl: Duration) -> Result<Hold> {
        positive(amount)?;
        if let Some(existing) = self.hold_by_reference(reference).await? {
            return Ok(existing);
        }

        let mut tx = self.db.begin().await?;
        let account = self.account_in(&mut tx, owner).await?;

        // The same guard as a debit, on `held` rather than `balance`.
        let sql = format!(
            "update {ACCOUNTS} set held = held + {amount} where account_id = {} and balance - held >= {amount}",
            tx.dialect().placeholder(1)
        );
        let reserved = tx.execute(&sql, &[Value::from(account.id.as_str())]).await?;
        if reserved == 0 {
            tx.rollback().await?;
            return Err(insufficient(owner, amount, account.available()));
        }

        let now = now();
        let hold = Hold {
            id: id("hold"),
            account_id: account.id.clone(),
            amount,
            status: HoldStatus::Held,
            reference: reference.to_string(),
            created_at: now,
            expires_at: now + ttl.as_secs(),
        };
        let inserted = self
            .db
            .table(HOLDS)
            .insert_in(
                &mut tx,
                &[
                    ("hold_id", hold.id.as_str().into()),
                    ("account_id", hold.account_id.as_str().into()),
                    ("amount", amount.into()),
                    ("status", HoldStatus::Held.as_str().into()),
                    ("reference", reference.into()),
                    ("created_at", (now as i64).into()),
                    ("expires_at", (hold.expires_at as i64).into()),
                ],
            )
            .await;
        if inserted.is_err() {
            // Somebody else held under this reference first. Theirs stands.
            tx.rollback().await?;
            return self.hold_by_reference(reference).await?.ok_or_else(|| {
                Error::msg("a hold lost an idempotency race and the winner is not there")
            });
        }

        tx.commit().await?;
        Ok(hold)
    }

    /// Spend a hold. Exactly once: a second capture of the same hold is an
    /// error, not a second charge.
    pub async fn capture(&self, hold_id: &str) -> Result<Transfer> {
        let mut tx = self.db.begin().await?;

        // The compare-and-set. One row means this call moved it from `held`;
        // zero means it was already captured or released, or never existed.
        let won = self
            .db
            .table(HOLDS)
            .filter("hold_id", hold_id)
            .filter("status", HoldStatus::Held.as_str())
            .update_in(&mut tx, &[("status", HoldStatus::Captured.as_str().into())])
            .await?;
        if won == 0 {
            tx.rollback().await?;
            return Err(hold_not_held(&self.db, hold_id).await);
        }

        let hold = self
            .db
            .table(HOLDS)
            .filter("hold_id", hold_id)
            .first_in(&mut tx)
            .await?
            .ok_or_else(|| Error::msg(format!("hold `{hold_id}` vanished mid-capture")))?;
        let hold = hold_from(&hold)?;
        let to = self.account_in(&mut tx, SYSTEM_CONSUMPTION).await?;

        // The hold covered the amount, so the balance is there by
        // construction; this moves it and lets the reservation go together.
        let sql = format!(
            "update {ACCOUNTS} set balance = balance - {0}, held = held - {0} where account_id = {1}",
            hold.amount,
            tx.dialect().placeholder(1)
        );
        tx.execute(&sql, &[Value::from(hold.account_id.as_str())]).await?;
        credit(&mut tx, &to.id, hold.amount).await?;
        draw_lots(&mut tx, &hold.account_id, hold.amount).await?;

        let transfer = self
            .record(
                &mut tx,
                TransferKind::Capture,
                &format!("capture:{}", hold.reference),
                &hold.account_id,
                &to.id,
                hold.amount,
                Json::object([("hold", Json::from(hold.id.as_str()))]),
                now(),
            )
            .await?
            .ok_or_else(|| Error::msg(format!("hold `{hold_id}` was already captured")))?;

        tx.commit().await?;
        Ok(transfer)
    }

    /// Give a hold back. Exactly once, like capture; nothing is written to the
    /// history because nothing moved.
    pub async fn release(&self, hold_id: &str) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let released = release_in(&self.db, &mut tx, hold_id).await?;
        if !released {
            tx.rollback().await?;
            return Err(hold_not_held(&self.db, hold_id).await);
        }
        tx.commit().await
    }

    // ------------------------------------------------------------------
    // Housekeeping
    // ------------------------------------------------------------------

    /// Release holds past their lifetime and expire lots past theirs.
    ///
    /// Run it on a schedule. An expired lot's remainder goes to
    /// `system:expiry` as an `Expire` transfer, so the history says where the
    /// credits went; an expired hold is released and nothing is written, as
    /// with any release.
    pub async fn sweep(&self) -> Result<Sweep> {
        let now = now();
        let mut sweep = Sweep::default();

        let stale_holds = self
            .db
            .table(HOLDS)
            .filter("status", HoldStatus::Held.as_str())
            .filter_op("expires_at", "<=", now as i64)
            .get(&self.db)
            .await?;
        for row in &stale_holds {
            let hold_id: String = row.get("hold_id")?;
            let mut tx = self.db.begin().await?;
            // Somebody may have captured it between the select and here; the
            // compare-and-set inside says so and this moves on.
            if release_in(&self.db, &mut tx, &hold_id).await? {
                sweep.holds_released += 1;
            }
            tx.commit().await?;
        }

        let dead_lots = self
            .db
            .table(LOTS)
            .filter_not_null("expires_at")
            .filter_op("expires_at", "<=", now as i64)
            .filter_op("remaining", ">", 0)
            .get(&self.db)
            .await?;
        for row in &dead_lots {
            let lot_id: String = row.get("lot_id")?;
            let account_id: String = row.get("account_id")?;
            let mut tx = self.db.begin().await?;

            // Claim the lot first — the same compare-and-set shape. Re-read the
            // remainder inside the claim so a consume that raced us is seen.
            let claimed = self
                .db
                .table(LOTS)
                .filter("lot_id", lot_id.as_str())
                .filter_op("remaining", ">", 0)
                .first_in(&mut tx)
                .await?;
            let Some(lot) = claimed else {
                tx.rollback().await?;
                continue;
            };
            let remaining: i64 = lot.get("remaining")?;

            // The owner may have spent more than the lot since; expire only
            // what is genuinely still there.
            let account = self
                .db
                .table(ACCOUNTS)
                .filter("account_id", account_id.as_str())
                .first_in(&mut tx)
                .await?
                .map(|row| account_from(&row))
                .transpose()?
                .ok_or_else(|| Error::msg(format!("lot `{lot_id}` names an account that is not there")))?;
            let amount = remaining.min(account.available()).max(0);

            self.db
                .table(LOTS)
                .filter("lot_id", lot_id.as_str())
                .update_in(&mut tx, &[("remaining", 0.into())])
                .await?;

            if amount > 0 {
                let to = self.account_in(&mut tx, SYSTEM_EXPIRY).await?;
                if debit_available(&mut tx, &account_id, amount).await? {
                    credit(&mut tx, &to.id, amount).await?;
                    self.record(
                        &mut tx,
                        TransferKind::Expire,
                        &format!("expire:{lot_id}"),
                        &account_id,
                        &to.id,
                        amount,
                        Json::object([("lot", Json::from(lot_id.as_str()))]),
                        now,
                    )
                    .await?;
                    sweep.amount_expired += amount;
                }
            }
            sweep.lots_expired += 1;
            tx.commit().await?;
        }

        Ok(sweep)
    }

    /// Whether the book balances: every account's `balance` equals the sum of
    /// its entries, and the sum of every balance is zero. Returns what is
    /// wrong, or nothing.
    pub async fn audit(&self) -> Result<Vec<String>> {
        let mut problems = Vec::new();
        let accounts = self.db.table(ACCOUNTS).filter("unit", self.unit.as_str()).get(&self.db).await?;
        let mut sum = 0i64;

        for row in &accounts {
            let account = account_from(row)?;
            sum += account.balance;
            let entries = self.db.table(ENTRIES).filter("account_id", account.id.as_str()).get(&self.db).await?;
            let total: i64 = entries.iter().filter_map(|e| e.get::<i64>("amount").ok()).sum();
            if total != account.balance {
                problems.push(format!(
                    "{}: balance is {} but its entries sum to {total}",
                    account.owner, account.balance
                ));
            }
            if account.held < 0 || account.held > account.balance.max(0) && !account.is_system() {
                problems.push(format!("{}: held {} against balance {}", account.owner, account.held, account.balance));
            }
        }
        if sum != 0 {
            problems.push(format!("the book does not balance: all accounts sum to {sum}, not 0"));
        }
        Ok(problems)
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    async fn account_in(&self, tx: &mut Transaction, owner: &str) -> Result<Account> {
        if let Some(row) = self
            .db
            .table(ACCOUNTS)
            .filter("owner", owner)
            .filter("unit", self.unit.as_str())
            .first_in(tx)
            .await?
        {
            return account_from(&row);
        }

        let account = Account {
            id: id("acc"),
            owner: owner.to_string(),
            unit: self.unit.clone(),
            balance: 0,
            held: 0,
            created_at: now(),
        };
        // Inside a savepoint, because PostgreSQL aborts the *whole*
        // transaction on any error — including the unique-index violation this
        // is about to court on purpose — and every statement after it fails
        // with `25P02` until the transaction ends. Rolling back to the
        // savepoint undoes only the failed insert and leaves the transaction
        // usable. MySQL would have carried on regardless; this is written for
        // the strictest of the three, and it was found by racing eight
        // first-time callers.
        tx.savepoint("account").await?;
        let inserted = self
            .db
            .table(ACCOUNTS)
            .insert_in(
                tx,
                &[
                    ("account_id", account.id.as_str().into()),
                    ("owner", owner.into()),
                    ("unit", self.unit.as_str().into()),
                    ("balance", 0.into()),
                    ("held", 0.into()),
                    ("created_at", (account.created_at as i64).into()),
                ],
            )
            .await;
        match inserted {
            Ok(_) => Ok(account),
            // Two first-time callers racing: the unique index on (owner, unit)
            // let one through. Undo ours and read theirs.
            Err(_) => {
                tx.rollback_to("account").await?;
                self.db
                .table(ACCOUNTS)
                .filter("owner", owner)
                .filter("unit", self.unit.as_str())
                .first_in(tx)
                .await?
                .map(|row| account_from(&row))
                .transpose()?
                .ok_or_else(|| Error::msg(format!("could not create or find the account for {owner}")))
            }
        }
    }

    /// Write the transfer and its two entries. `None` if the reference is
    /// taken — the caller rolls back and returns the winner.
    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        tx: &mut Transaction,
        kind: TransferKind,
        reference: &str,
        from: &str,
        to: &str,
        amount: i64,
        metadata: Json,
        now: u64,
    ) -> Result<Option<Transfer>> {
        let transfer = Transfer {
            id: id("tr"),
            reference: reference.to_string(),
            kind,
            from_account: from.to_string(),
            to_account: to.to_string(),
            amount,
            created_at: now,
            metadata: metadata.clone(),
        };

        let inserted = self
            .db
            .table(TRANSFERS)
            .insert_in(
                tx,
                &[
                    ("transfer_id", transfer.id.as_str().into()),
                    ("reference", reference.into()),
                    ("kind", kind.as_str().into()),
                    ("from_account", from.into()),
                    ("to_account", to.into()),
                    ("amount", amount.into()),
                    ("created_at", (now as i64).into()),
                    ("metadata", if metadata.is_null() { Value::Null } else { Value::from(metadata) }),
                ],
            )
            .await;
        if inserted.is_err() {
            return Ok(None);
        }

        for (account_id, signed) in [(from, -amount), (to, amount)] {
            let after = balance_of(tx, account_id).await?;
            self.db
                .table(ENTRIES)
                .insert_in(
                    tx,
                    &[
                        ("transfer_id", transfer.id.as_str().into()),
                        ("account_id", account_id.into()),
                        ("amount", signed.into()),
                        ("balance_after", after.into()),
                        ("kind", kind.as_str().into()),
                        ("reference", reference.into()),
                        ("created_at", (now as i64).into()),
                    ],
                )
                .await?;
        }
        Ok(Some(transfer))
    }
}

/// Debit an account only if `available` covers it. `true` when it did.
async fn debit_available(tx: &mut Transaction, account_id: &str, amount: i64) -> Result<bool> {
    let sql = format!(
        "update {ACCOUNTS} set balance = balance - {amount} where account_id = {} and balance - held >= {amount}",
        tx.dialect().placeholder(1)
    );
    Ok(tx.execute(&sql, &[Value::from(account_id)]).await? == 1)
}

/// Debit with no floor. For system accounts only, which are allowed to go
/// negative: that is where the other side of every credit lives.
async fn debit_unchecked(tx: &mut Transaction, account_id: &str, amount: i64) -> Result<()> {
    let sql = format!(
        "update {ACCOUNTS} set balance = balance - {amount} where account_id = {}",
        tx.dialect().placeholder(1)
    );
    tx.execute(&sql, &[Value::from(account_id)]).await?;
    Ok(())
}

async fn credit(tx: &mut Transaction, account_id: &str, amount: i64) -> Result<()> {
    let sql = format!(
        "update {ACCOUNTS} set balance = balance + {amount} where account_id = {}",
        tx.dialect().placeholder(1)
    );
    tx.execute(&sql, &[Value::from(account_id)]).await?;
    Ok(())
}

async fn balance_of(tx: &mut Transaction, account_id: &str) -> Result<i64> {
    let sql = format!("select balance from {ACCOUNTS} where account_id = {}", tx.dialect().placeholder(1));
    tx.scalar::<i64>(&sql, &[Value::from(account_id)])
        .await?
        .ok_or_else(|| Error::msg(format!("account `{account_id}` is not there")))
}

/// Take `amount` out of the account's lots, earliest expiry first.
///
/// Runs after the account row has been debited with its guard, which is also
/// the row lock — so two consumes on one account do this one at a time and
/// the second sees the first's decrements.
async fn draw_lots(tx: &mut Transaction, account_id: &str, mut amount: i64) -> Result<()> {
    let placeholder = tx.dialect().placeholder(1);
    // Nulls (never expires) last: spend the credits that will die first.
    let sql = format!(
        "select lot_id, remaining, expires_at from {LOTS} where account_id = {placeholder} and remaining > 0"
    );
    let mut lots = tx.select(&sql, &[Value::from(account_id)]).await?;
    lots.sort_by_key(|row| (row.get::<i64>("expires_at").ok().is_none(), row.get::<i64>("expires_at").unwrap_or(i64::MAX)));

    for lot in lots {
        if amount <= 0 {
            break;
        }
        let lot_id: String = lot.get("lot_id")?;
        let remaining: i64 = lot.get("remaining")?;
        let take = remaining.min(amount);
        let sql = format!(
            "update {LOTS} set remaining = remaining - {take} where lot_id = {} and remaining >= {take}",
            tx.dialect().placeholder(1)
        );
        if tx.execute(&sql, &[Value::from(lot_id.as_str())]).await? == 1 {
            amount -= take;
        }
    }
    // Credits with no lot — adjustments, or a book that predates lots — are
    // simply consumed; a shortfall here is not a shortfall of balance.
    Ok(())
}

/// Release a hold if it is still held. `true` when this call did it.
async fn release_in(db: &Database, tx: &mut Transaction, hold_id: &str) -> Result<bool> {
    let won = db
        .table(HOLDS)
        .filter("hold_id", hold_id)
        .filter("status", HoldStatus::Held.as_str())
        .update_in(tx, &[("status", HoldStatus::Released.as_str().into())])
        .await?;
    if won == 0 {
        return Ok(false);
    }
    let hold = db
        .table(HOLDS)
        .filter("hold_id", hold_id)
        .first_in(tx)
        .await?
        .ok_or_else(|| Error::msg(format!("hold `{hold_id}` vanished mid-release")))?;
    let hold = hold_from(&hold)?;
    let sql = format!(
        "update {ACCOUNTS} set held = held - {} where account_id = {}",
        hold.amount,
        tx.dialect().placeholder(1)
    );
    tx.execute(&sql, &[Value::from(hold.account_id.as_str())]).await?;
    Ok(true)
}

async fn hold_not_held(db: &Database, hold_id: &str) -> Error {
    match db.table(HOLDS).filter("hold_id", hold_id).first(db).await {
        Ok(Some(row)) => {
            let status = row.get::<String>("status").unwrap_or_default();
            Error::msg(format!("hold `{hold_id}` is already {status}; a hold is captured or released once"))
        }
        _ => Error::msg(format!("no hold `{hold_id}`")),
    }
}

fn insufficient(owner: &str, wanted: i64, available: i64) -> Error {
    Error::msg(format!("{owner} has {available} available and {wanted} was asked for"))
}

fn positive(amount: i64) -> Result<()> {
    if amount <= 0 {
        return Err(Error::msg(format!("an amount must be above zero; {amount} is not")));
    }
    Ok(())
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

fn account_from(row: &Row) -> Result<Account> {
    Ok(Account {
        id: row.get("account_id")?,
        owner: row.get("owner")?,
        unit: row.get("unit")?,
        balance: row.get("balance")?,
        held: row.get("held")?,
        created_at: row.get::<i64>("created_at")?.max(0) as u64,
    })
}

fn transfer_from(row: &Row) -> Result<Transfer> {
    Ok(Transfer {
        id: row.get("transfer_id")?,
        reference: row.get("reference")?,
        kind: TransferKind::parse(&row.get::<String>("kind")?)
            .ok_or_else(|| Error::msg("a transfer row holds a kind this crate does not know"))?,
        from_account: row.get("from_account")?,
        to_account: row.get("to_account")?,
        amount: row.get("amount")?,
        created_at: row.get::<i64>("created_at")?.max(0) as u64,
        metadata: row.get::<Json>("metadata").unwrap_or(Json::Null),
    })
}

fn entry_from(row: &Row) -> Result<Entry> {
    Ok(Entry {
        id: row.get("id")?,
        transfer_id: row.get("transfer_id")?,
        account_id: row.get("account_id")?,
        amount: row.get("amount")?,
        balance_after: row.get("balance_after")?,
        kind: TransferKind::parse(&row.get::<String>("kind")?)
            .ok_or_else(|| Error::msg("an entry row holds a kind this crate does not know"))?,
        reference: row.get("reference")?,
        created_at: row.get::<i64>("created_at")?.max(0) as u64,
    })
}

fn hold_from(row: &Row) -> Result<Hold> {
    Ok(Hold {
        id: row.get("hold_id")?,
        account_id: row.get("account_id")?,
        amount: row.get("amount")?,
        status: HoldStatus::parse(&row.get::<String>("status")?)
            .ok_or_else(|| Error::msg("a hold row holds a status this crate does not know"))?,
        reference: row.get("reference")?,
        created_at: row.get::<i64>("created_at")?.max(0) as u64,
        expires_at: row.get::<i64>("expires_at")?.max(0) as u64,
    })
}
