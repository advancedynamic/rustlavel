//! Money out: to a bank account or an e-wallet.

use crate::channel::{Bank, Wallet};
use crate::money::Money;
use rustlavel_core::{Error, Json, Result};

/// Where the money goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Account {
    Bank { bank: Bank, number: String, holder: String },
    Wallet { wallet: Wallet, phone: String, holder: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRequest {
    /// The application's id, for matching the result — and for the gateway to
    /// refuse a second transfer with the same one, which is the protection
    /// against a retry paying somebody twice.
    pub reference: String,
    pub amount: Money,
    pub to: Account,
    pub description: String,
}

impl TransferRequest {
    pub fn new(reference: impl Into<String>, amount: Money, to: Account) -> TransferRequest {
        TransferRequest { reference: reference.into(), amount, to, description: String::new() }
    }

    pub fn description(mut self, description: impl Into<String>) -> TransferRequest {
        self.description = description.into();
        self
    }

    pub fn validate(&self) -> Result<()> {
        self.amount.positive()?;
        if self.reference.trim().is_empty() {
            return Err(Error::msg("a transfer needs a reference: it is what stops a retry paying twice"));
        }
        let number = match &self.to {
            Account::Bank { number, .. } => number,
            Account::Wallet { phone, .. } => phone,
        };
        if number.trim().is_empty() {
            return Err(Error::msg("a transfer needs a destination account or number"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferStatus {
    Pending,
    Completed,
    Failed,
}

impl TransferStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TransferStatus::Pending => "pending",
            TransferStatus::Completed => "completed",
            TransferStatus::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Option<TransferStatus> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "pending" | "processing" => TransferStatus::Pending,
            "completed" | "success" | "settled" => TransferStatus::Completed,
            "failed" => TransferStatus::Failed,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    /// The gateway's id.
    pub id: String,
    pub reference: String,
    pub status: TransferStatus,
    pub amount: Money,
    /// Why it failed, when it did, in the gateway's words.
    pub failure: Option<String>,
}

impl Transfer {
    pub fn to_json(&self) -> Json {
        Json::object([
            ("id", Json::from(self.id.as_str())),
            ("reference", Json::from(self.reference.as_str())),
            ("status", Json::from(self.status.as_str())),
            ("amount", self.amount.to_json()),
            ("failure", self.failure.as_deref().map_or(Json::Null, Json::from)),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transfer_needs_a_reference_and_a_destination() {
        let to = Account::Bank { bank: Bank::Bca, number: "1234567890".into(), holder: "Ada".into() };
        assert!(TransferRequest::new("payout-1", Money::idr(50_000), to.clone()).validate().is_ok());
        assert!(TransferRequest::new("", Money::idr(50_000), to.clone()).validate().is_err());

        let nowhere = Account::Wallet { wallet: Wallet::Dana, phone: " ".into(), holder: "Ada".into() };
        assert!(TransferRequest::new("payout-2", Money::idr(50_000), nowhere).validate().is_err());
    }
}
