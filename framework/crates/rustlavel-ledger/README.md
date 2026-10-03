# rustlavel-ledger

A double-entry ledger for credits and balances, for the [Rustlavel](https://github.com/advancedynamic/rustlavel) framework: atomic debit and credit, holds that capture or release exactly once, credits that expire by batch, and a history that adds up.

## The number that has to be right

An application that sells credits has a number per customer that must be right — not approximately, not after a nightly job, right now and under load.

```rust
let ledger = Ledger::new(db.clone(), "credits");

ledger.top_up("user:42", 500, "payment_abc", Some(Duration::from_secs(60 * 86400))).await?;
ledger.consume("user:42", 30, "job_7").await?;          // Err if 30 is not available
ledger.balance("user:42").await?                          // Balance { total, held, available }
ledger.history("user:42", 50).await?                      // newest first, with running balance
```

**Every top-up and consume is idempotent on its reference.** A payment webhook retried three times credits once; a job retried credits once. The unique index on the reference enforces it, so it holds under concurrency.

## Double entry

Every movement is a transfer between two accounts and writes two entries, equal and opposite. A top-up comes *from* `system:topup`; a spent credit goes *to* `system:consumption`; an expired one to `system:expiry`. So the sum of every balance in the book is zero, always — and `ledger.audit()` says whether it is. A single-entry design has no such check, and its errors are invisible until a customer complains.

## Holds

A job that will cost 50 credits and take a minute must not find the balance gone when it finishes.

```rust
let hold = ledger.hold("user:42", 50, "job_7", Duration::from_secs(900)).await?;
// available is down by 50; balance is not
// … the job runs …
ledger.capture(&hold.id).await?;   // spend it — once; a second capture is an error, not a charge
// or
ledger.release(&hold.id).await?;   // give it back — once
```

A hold has a lifetime. `ledger.sweep()` releases the ones nobody came back for, and expires lots past their date; run it on a schedule.

## What is atomic, and how

Every operation is one transaction. The debit is

```sql
UPDATE ledger_accounts SET balance = balance - ? WHERE account_id = ? AND balance - held >= ?
```

and the row count is the answer: zero means the funds were not there, and nothing else has happened. That guard is also the lock — the database serialises two transactions writing the same row — so two consumers racing for the last credit cannot both have it. Capture and release are the same shape on the hold's `status`.

Measured, not argued, and against **all four databases** — PostgreSQL 16, MySQL 8.4, SQL Server 2022 and SQLite: twenty tasks each spending 10 from a balance of 100 — exactly ten succeed, the balance ends at zero, the book balances. Remove the `WHERE` guard and twenty succeed. Sixteen tasks capturing one hold — one charge. Eight first-time callers for one owner — one account.

That last one is the interesting one, because the four databases disagree about it. **An account is created before the transaction that will use it begins**, as a statement of its own. Created inside the transaction, PostgreSQL aborts the whole transaction on the duplicate-key error a lost race produces (which a savepoint once papered over), and MySQL fails differently and worse: a transaction's first read fixes its snapshot, so after losing the insert the re-read cannot see the row the winner just committed, and InnoDB can deadlock the racing inserts, rolling the entire transaction back and taking the savepoint with it. Found by running the suite against MySQL, where the first-use test failed 120 runs in 120 under load and now fails none.

## Tables

`create_tables(schema)` or the `CreateLedgerTables` migration: `ledger_accounts`, `ledger_transfers`, `ledger_entries`, `ledger_holds`, `ledger_lots`.

## Licence

MIT.
