//! A reservation against an account.

use rustlavel_core::Json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HoldStatus {
    /// Reserved. `available` is down by the amount; `balance` is not.
    Held,
    /// Spent. A `Capture` transfer was written.
    Captured,
    /// Given back. Nothing was written to the history: nothing moved.
    Released,
}

impl HoldStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            HoldStatus::Held => "held",
            HoldStatus::Captured => "captured",
            HoldStatus::Released => "released",
        }
    }

    pub fn parse(text: &str) -> Option<HoldStatus> {
        Some(match text {
            "held" => HoldStatus::Held,
            "captured" => HoldStatus::Captured,
            "released" => HoldStatus::Released,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub id: String,
    pub account_id: String,
    pub amount: i64,
    pub status: HoldStatus,
    /// What the hold is for — a job id, usually. Unique: holding twice for one
    /// job returns the first hold.
    pub reference: String,
    pub created_at: u64,
    /// After this, [`crate::Ledger::sweep`] releases it. A job that died
    /// without releasing must not keep its credits reserved forever.
    pub expires_at: u64,
}

impl Hold {
    pub fn to_json(&self) -> Json {
        Json::object([
            ("id", Json::from(self.id.as_str())),
            ("account_id", Json::from(self.account_id.as_str())),
            ("amount", Json::from(self.amount)),
            ("status", Json::from(self.status.as_str())),
            ("reference", Json::from(self.reference.as_str())),
            ("created_at", Json::from(self.created_at as i64)),
            ("expires_at", Json::from(self.expires_at as i64)),
        ])
    }
}
