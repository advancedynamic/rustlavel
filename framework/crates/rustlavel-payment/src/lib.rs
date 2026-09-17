//! rustlavel-payment: payment gateways behind one trait.
//!
//! An application takes money through a *gateway* — a company that presents a
//! virtual account number, a QR code or an e-wallet screen to the customer,
//! collects the payment, and tells the application by webhook. Every gateway
//! does the same five things and describes them differently, so this crate
//! holds the description and a driver holds the difference.
//!
//! # What crosses the trait
//!
//! - [`Money`] — an amount in minor units and a currency. An integer, never a
//!   float: money that is rounded is money that goes missing.
//! - [`Channel`] — how the customer pays: a bank's virtual account, QRIS, an
//!   e-wallet, or a retail counter. The Indonesian set, because that is where
//!   the first driver lives; the enum is not closed.
//! - [`Charge`] — a request for payment and its state, with what the customer
//!   needs to pay it (a VA number, a QR string, a deeplink).
//! - [`Transfer`] — money out, to a bank account or an e-wallet.
//! - [`WebhookEvent`] — what a gateway says happened, after its signature has
//!   been checked.
//!
//! # The receiver, and why it deduplicates on disk
//!
//! A gateway retries a webhook until it is acknowledged, and a network that
//! drops the acknowledgement delivers the same payment twice. [`Receiver`]
//! verifies the signature (each driver knows its own scheme), records the
//! event by the *gateway's* id, and hands a duplicate straight back as already
//! handled. That record must survive a restart: a duplicate that arrives after
//! a deploy is still a duplicate, and crediting it twice is a refund
//! conversation. So the log is a trait with a table-backed implementation
//! behind the `db` feature, and the in-memory one is for tests.
//!
//! # No driver is invented
//!
//! [`FakeGateway`] is the only driver here today. A driver for a real gateway
//! is written against that gateway's published specification, not from what
//! its competitors do — an adapter written from guesses is code that looks
//! finished and fails on the first real callback.

pub mod channel;
pub mod charge;
#[cfg(feature = "db")]
pub mod database;
pub mod fake;
pub mod gateway;
pub mod money;
pub mod receiver;
pub mod signature;
pub mod transfer;
pub mod webhook;

pub use channel::{Bank, Channel, Retail, Wallet};
pub use charge::{Charge, ChargeRequest, ChargeStatus, Customer, Instructions};
pub use fake::FakeGateway;
pub use gateway::{Gateway, GatewayFuture};
pub use money::Money;
pub use receiver::{Handled, Receiver};
pub use transfer::{Account, Transfer, TransferRequest, TransferStatus};
pub use webhook::{EventKind, MemoryWebhookLog, WebhookEvent, WebhookLog};

pub use rustlavel_core::{Error, Result};
