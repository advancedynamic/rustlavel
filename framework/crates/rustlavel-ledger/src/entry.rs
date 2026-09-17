//! A transfer, and the two entries it writes.

use rustlavel_core::Json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferKind {
    /// Credits in, from `system:topup`.
    TopUp,
    /// Credits spent, to `system:consumption`.
    Consume,
    /// A hold being captured. Same direction as `Consume`, kept distinct so
    /// the history says which.
    Capture,
    /// Credits that ran out, to `system:expiry`.
    Expire,
    /// A correction by an operator. Either direction.
    Adjust,
    /// Credits given back, from `system:consumption`.
    Refund,
}

impl TransferKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TransferKind::TopUp => "topup",
            TransferKind::Consume => "consume",
            TransferKind::Capture => "capture",
            TransferKind::Expire => "expire",
            TransferKind::Adjust => "adjust",
            TransferKind::Refund => "refund",
        }
    }

    pub fn parse(text: &str) -> Option<TransferKind> {
        Some(match text {
            "topup" => TransferKind::TopUp,
            "consume" => TransferKind::Consume,
            "capture" => TransferKind::Capture,
            "expire" => TransferKind::Expire,
            "adjust" => TransferKind::Adjust,
            "refund" => TransferKind::Refund,
            _ => return None,
        })
    }
}

/// One movement of value between two accounts.
#[derive(Debug, Clone, PartialEq)]
pub struct Transfer {
    pub id: String,
    /// The caller's idempotency key — a payment id, a job id. Unique: a second
    /// transfer under the same reference returns the first rather than moving
    /// the money again, which is what stops a retried webhook crediting twice.
    pub reference: String,
    pub kind: TransferKind,
    pub from_account: String,
    pub to_account: String,
    /// Always positive. Direction is the two account fields.
    pub amount: i64,
    pub created_at: u64,
    pub metadata: Json,
}

impl Transfer {
    pub fn to_json(&self) -> Json {
        Json::object([
            ("id", Json::from(self.id.as_str())),
            ("reference", Json::from(self.reference.as_str())),
            ("kind", Json::from(self.kind.as_str())),
            ("from", Json::from(self.from_account.as_str())),
            ("to", Json::from(self.to_account.as_str())),
            ("amount", Json::from(self.amount)),
            ("created_at", Json::from(self.created_at as i64)),
            ("metadata", self.metadata.clone()),
        ])
    }
}

/// One side of a transfer, on one account. Two per transfer, equal and
/// opposite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    pub transfer_id: String,
    pub account_id: String,
    /// Signed: negative is a debit, positive a credit.
    pub amount: i64,
    /// The account's balance once this entry was applied. A history that
    /// carries its running balance can be checked line by line.
    pub balance_after: i64,
    pub kind: TransferKind,
    pub reference: String,
    pub created_at: u64,
}

impl Entry {
    pub fn to_json(&self) -> Json {
        Json::object([
            ("id", Json::from(self.id)),
            ("transfer_id", Json::from(self.transfer_id.as_str())),
            ("amount", Json::from(self.amount)),
            ("balance_after", Json::from(self.balance_after)),
            ("kind", Json::from(self.kind.as_str())),
            ("reference", Json::from(self.reference.as_str())),
            ("created_at", Json::from(self.created_at as i64)),
        ])
    }
}
