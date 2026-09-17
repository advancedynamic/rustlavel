//! What is being sold.

use rustlavel_core::Json;
use rustlavel_payment::Money;
use std::time::Duration;

/// How long one paid period lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cycle {
    /// Thirty days. Not "a month": a subscription taken on the 31st has no
    /// 31st next month, and a fixed length is a promise that can be kept.
    Monthly,
    /// Three hundred and sixty-five days.
    Yearly,
    Days(u32),
}

impl Cycle {
    pub fn length(self) -> Duration {
        let days = match self {
            Cycle::Monthly => 30,
            Cycle::Yearly => 365,
            Cycle::Days(days) => days.max(1),
        };
        Duration::from_secs(u64::from(days) * 86_400)
    }

    pub fn as_str(self) -> String {
        match self {
            Cycle::Monthly => "monthly".to_string(),
            Cycle::Yearly => "yearly".to_string(),
            Cycle::Days(days) => format!("days:{days}"),
        }
    }

    pub fn parse(text: &str) -> Option<Cycle> {
        match text {
            "monthly" => Some(Cycle::Monthly),
            "yearly" => Some(Cycle::Yearly),
            other => other.strip_prefix("days:")?.parse().ok().map(Cycle::Days),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Stable, and what a subscription refers to: `starter`, `pro`.
    pub id: String,
    pub name: String,
    pub price: Money,
    /// Credited to the subscriber's ledger account each time an invoice is
    /// paid.
    pub credits_per_cycle: i64,
    pub cycle: Cycle,
    /// How long after a period ends an unpaid subscription keeps its access
    /// before being suspended.
    pub grace: Duration,
}

impl Plan {
    pub fn new(id: impl Into<String>, name: impl Into<String>, price: Money, credits_per_cycle: i64) -> Plan {
        Plan {
            id: id.into(),
            name: name.into(),
            price,
            credits_per_cycle,
            cycle: Cycle::Monthly,
            // Three days: long enough for a bank transfer over a weekend,
            // short enough that a lapsed customer is not served for a month.
            grace: Duration::from_secs(3 * 86_400),
        }
    }

    pub fn cycle(mut self, cycle: Cycle) -> Plan {
        self.cycle = cycle;
        self
    }

    pub fn grace(mut self, grace: Duration) -> Plan {
        self.grace = grace;
        self
    }

    pub fn to_json(&self) -> Json {
        Json::object([
            ("id", Json::from(self.id.as_str())),
            ("name", Json::from(self.name.as_str())),
            ("price", self.price.to_json()),
            ("credits_per_cycle", Json::from(self.credits_per_cycle)),
            ("cycle", Json::from(self.cycle.as_str())),
            ("grace_seconds", Json::from(self.grace.as_secs() as i64)),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycles_round_trip_and_are_fixed_lengths() {
        for cycle in [Cycle::Monthly, Cycle::Yearly, Cycle::Days(7)] {
            assert_eq!(Cycle::parse(&cycle.as_str()), Some(cycle));
        }
        assert_eq!(Cycle::Monthly.length(), Duration::from_secs(30 * 86_400));
        assert_eq!(Cycle::Days(0).length(), Duration::from_secs(86_400), "a zero-day cycle would invoice forever");
        assert_eq!(Cycle::parse("weekly"), None);
    }
}
