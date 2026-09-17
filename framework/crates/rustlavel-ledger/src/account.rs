//! An account, and what it holds.

use rustlavel_core::Json;

/// The other side of a top-up. Credits come from here.
pub const SYSTEM_TOPUP: &str = "system:topup";
/// Where spent credits go.
pub const SYSTEM_CONSUMPTION: &str = "system:consumption";
/// Where credits nobody used in time go.
pub const SYSTEM_EXPIRY: &str = "system:expiry";

/// One account. An owner has one per unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    /// Who this belongs to: `user:42`, `workspace:7`, or a `system:` name.
    /// A string rather than a foreign key, because the users live in the
    /// application's tables and a ledger that knew their shape would be one
    /// that had to change with them.
    pub owner: String,
    /// What is counted: `credits`, `IDR`. An owner may have one account per
    /// unit, and units never mix.
    pub unit: String,
    /// Everything credited minus everything debited, holds included.
    pub balance: i64,
    /// Reserved by holds not yet captured or released.
    pub held: i64,
    pub created_at: u64,
}

impl Account {
    /// What can be spent or held right now.
    pub fn available(&self) -> i64 {
        self.balance - self.held
    }

    pub fn is_system(&self) -> bool {
        self.owner.starts_with("system:")
    }

    pub fn balance_view(&self) -> Balance {
        Balance { total: self.balance, held: self.held, available: self.available() }
    }
}

/// The three numbers a screen shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Balance {
    pub total: i64,
    pub held: i64,
    pub available: i64,
}

impl Balance {
    pub fn to_json(&self) -> Json {
        Json::object([
            ("total", Json::from(self.total)),
            ("held", Json::from(self.held)),
            ("available", Json::from(self.available)),
        ])
    }
}
