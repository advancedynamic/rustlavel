//! An amount of money.
//!
//! **An integer of minor units, never a float.** A float can hold 0.1 only
//! approximately, and an application that adds ten of them does not get 1.0.
//! Rupiah has no minor unit in practice but the rule is the same: the amount
//! is the whole number of the smallest unit the currency has, and arithmetic
//! on it is exact.

use rustlavel_core::{Error, Json, Result};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Money {
    /// Minor units: cents, sen. For IDR the minor unit is the rupiah itself.
    pub minor: i64,
    /// ISO 4217, upper case.
    pub currency: String,
}

impl Money {
    pub fn new(minor: i64, currency: impl Into<String>) -> Money {
        Money { minor, currency: currency.into().to_ascii_uppercase() }
    }

    /// Rupiah, which is what the first driver deals in.
    pub fn idr(rupiah: i64) -> Money {
        Money::new(rupiah, "IDR")
    }

    /// A charge for nothing or for less than nothing is a mistake upstream,
    /// and a gateway asked for one answers with an error that names nothing.
    pub fn positive(&self) -> Result<()> {
        if self.minor <= 0 {
            return Err(Error::msg(format!(
                "an amount must be above zero; {} {} is not",
                self.minor, self.currency
            )));
        }
        Ok(())
    }

    pub fn is_zero(&self) -> bool {
        self.minor == 0
    }

    /// Add, refusing to mix currencies. Silently adding 100 IDR to 100 USD
    /// produces a number that is neither.
    pub fn plus(&self, other: &Money) -> Result<Money> {
        self.same_currency(other)?;
        self.minor
            .checked_add(other.minor)
            .map(|minor| Money::new(minor, &self.currency))
            .ok_or_else(|| Error::msg("the amount overflowed"))
    }

    pub fn minus(&self, other: &Money) -> Result<Money> {
        self.same_currency(other)?;
        self.minor
            .checked_sub(other.minor)
            .map(|minor| Money::new(minor, &self.currency))
            .ok_or_else(|| Error::msg("the amount overflowed"))
    }

    fn same_currency(&self, other: &Money) -> Result<()> {
        if self.currency != other.currency {
            return Err(Error::msg(format!(
                "cannot combine {} with {}: different currencies",
                self.currency, other.currency
            )));
        }
        Ok(())
    }

    pub fn to_json(&self) -> Json {
        Json::object([
            ("minor", Json::from(self.minor)),
            ("currency", Json::from(self.currency.as_str())),
        ])
    }

    pub fn from_json(json: &Json) -> Option<Money> {
        let minor = json.get("minor").and_then(Json::as_i64)?;
        let currency = json.get("currency").and_then(Json::as_str)?;
        Some(Money::new(minor, currency))
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // "IDR 150000" — the currency first, so a column of amounts lines up
        // and a reader never has to guess which one they are looking at.
        write!(f, "{} {}", self.currency, self.minor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_is_exact_and_refuses_to_mix_currencies() {
        let a = Money::idr(150_000);
        let b = Money::idr(25_000);
        assert_eq!(a.plus(&b).unwrap(), Money::idr(175_000));
        assert_eq!(a.minus(&b).unwrap(), Money::idr(125_000));
        assert!(a.plus(&Money::new(1, "USD")).is_err(), "currencies were mixed");
    }

    #[test]
    fn a_charge_for_nothing_is_refused_before_it_reaches_a_gateway() {
        assert!(Money::idr(0).positive().is_err());
        assert!(Money::idr(-1).positive().is_err());
        assert!(Money::idr(1).positive().is_ok());
    }

    #[test]
    fn overflow_is_an_error_rather_than_a_wrap() {
        assert!(Money::idr(i64::MAX).plus(&Money::idr(1)).is_err());
    }

    #[test]
    fn survives_json() {
        let money = Money::idr(99_000);
        assert_eq!(Money::from_json(&money.to_json()), Some(money));
        assert_eq!(Money::new(5, "usd").currency, "USD");
    }
}
