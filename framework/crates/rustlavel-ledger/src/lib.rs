//! rustlavel-ledger: a book that balances.
//!
//! An application that sells credits has a number per customer that must be
//! right — not approximately right, not right after a nightly job, right now
//! and under load. This crate is that number and the rules around it.
//!
//! # Double entry
//!
//! Every movement of value is a [`Transfer`] between two [`Account`]s, and
//! writes two [`Entry`] rows: a debit on one and a credit on the other, equal
//! and opposite. Where does a top-up come *from*? A system account,
//! `system:topup`. Where does a spent credit *go*? `system:consumption`. So
//! the sum of every balance in the book is zero, always, and [`Ledger::audit`]
//! can say whether it is. A single-entry design — "add 100 to the user" — has
//! no such check, and its errors are invisible until a customer complains.
//!
//! # What is atomic, and how
//!
//! Every operation is one database transaction. The debit is
//! `UPDATE … SET balance = balance - ? WHERE id = ? AND balance - held >= ?`,
//! and the row count is the answer: zero means the funds were not there, and
//! nothing else has happened yet. That guard is also the lock — the database
//! serialises two transactions writing the same row — so two consumers racing
//! for the last credit cannot both have it. Measured in this crate's tests.
//!
//! # Holds
//!
//! A job that will cost 50 credits and take a minute must not find the
//! balance gone when it finishes. [`Ledger::hold`] reserves the amount up
//! front (it leaves `balance` and raises `held`; `available` is the
//! difference), and the job ends with [`Ledger::capture`] or
//! [`Ledger::release`]. Both are a compare-and-set on the hold's status, so a
//! hold cannot be captured twice. A hold has a lifetime; [`Ledger::sweep`]
//! releases the ones nobody came back for.
//!
//! # Expiry
//!
//! Credits bought in March may be gone in May. Each top-up is a [`Lot`] with
//! an expiry, consumption draws from the lot that expires first, and
//! [`Ledger::sweep`] moves what is left in a dead lot to `system:expiry` — as
//! a transfer, so it shows in the history like everything else.

pub mod account;
pub mod entry;
pub mod hold;
pub mod ledger;
pub mod schema;

pub use account::{Account, Balance, SYSTEM_CONSUMPTION, SYSTEM_EXPIRY, SYSTEM_TOPUP};
pub use entry::{Entry, Transfer, TransferKind};
pub use hold::{Hold, HoldStatus};
pub use ledger::{Ledger, Sweep};
pub use schema::{CreateLedgerTables, create_tables, drop_tables};

pub use rustlavel_core::{Error, Result};
