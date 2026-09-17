//! A request for payment, and its state.

use crate::channel::Channel;
use crate::money::Money;
use rustlavel_core::Json;
use std::time::Duration;

/// Who is paying, as much as the gateway needs to show them a screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Customer {
    pub name: String,
    pub email: Option<String>,
    pub phone: Option<String>,
}

/// What to ask for.
#[derive(Debug, Clone, PartialEq)]
pub struct ChargeRequest {
    /// The application's own id for this charge — an order number, an invoice
    /// id. Sent to the gateway and echoed back in every event, which is how a
    /// webhook is matched to the thing it is about.
    pub reference: String,
    pub amount: Money,
    pub channel: Channel,
    /// How long the customer has. After this the gateway expires the charge
    /// and says so by webhook.
    pub expires_in: Duration,
    pub customer: Customer,
    /// Shown on the payment screen and the bank statement where the channel
    /// allows it.
    pub description: String,
    /// Anything the application wants back unchanged in the webhook.
    pub metadata: Json,
}

impl ChargeRequest {
    pub fn new(reference: impl Into<String>, amount: Money, channel: Channel) -> ChargeRequest {
        ChargeRequest {
            reference: reference.into(),
            amount,
            channel,
            // A day: long enough for a bank transfer somebody meant to make,
            // short enough that a VA number is not held open for a week.
            expires_in: Duration::from_secs(24 * 60 * 60),
            customer: Customer::default(),
            description: String::new(),
            metadata: Json::Null,
        }
    }

    pub fn expires_in(mut self, duration: Duration) -> ChargeRequest {
        self.expires_in = duration;
        self
    }

    pub fn customer(mut self, customer: Customer) -> ChargeRequest {
        self.customer = customer;
        self
    }

    pub fn description(mut self, description: impl Into<String>) -> ChargeRequest {
        self.description = description.into();
        self
    }

    pub fn metadata(mut self, metadata: Json) -> ChargeRequest {
        self.metadata = metadata;
        self
    }

    /// The checks every driver would otherwise repeat.
    pub fn validate(&self) -> rustlavel_core::Result<()> {
        self.amount.positive()?;
        if self.reference.trim().is_empty() {
            return Err(rustlavel_core::Error::msg(
                "a charge needs a reference — the id the webhook will be matched to",
            ));
        }
        if self.expires_in.is_zero() {
            return Err(rustlavel_core::Error::msg("a charge that expires immediately cannot be paid"));
        }
        Ok(())
    }
}

/// Where a charge is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChargeStatus {
    /// Created, and the customer has not paid yet.
    Pending,
    /// The gateway has the money.
    Paid,
    /// The customer did not pay in time.
    Expired,
    /// The application cancelled it before it was paid.
    Cancelled,
    /// The gateway could not complete it.
    Failed,
}

impl ChargeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ChargeStatus::Pending => "pending",
            ChargeStatus::Paid => "paid",
            ChargeStatus::Expired => "expired",
            ChargeStatus::Cancelled => "cancelled",
            ChargeStatus::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Option<ChargeStatus> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "pending" => ChargeStatus::Pending,
            "paid" | "settled" | "success" => ChargeStatus::Paid,
            "expired" => ChargeStatus::Expired,
            "cancelled" | "canceled" => ChargeStatus::Cancelled,
            "failed" => ChargeStatus::Failed,
            _ => return None,
        })
    }

    /// Whether anything more can happen to it.
    pub fn is_final(self) -> bool {
        !matches!(self, ChargeStatus::Pending)
    }
}

/// What the customer needs in order to pay. Which fields are set depends on
/// the channel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Instructions {
    /// The virtual account number, for a VA charge.
    pub account_number: Option<String>,
    /// The QR content, for QRIS. Render it; do not show the string.
    pub qr_string: Option<String>,
    /// Where to send the customer, for an e-wallet or a hosted page.
    pub url: Option<String>,
    /// The code to read out at a retail counter.
    pub payment_code: Option<String>,
}

/// A charge, as the gateway knows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Charge {
    /// The gateway's id. This is what a webhook carries and what
    /// deduplication keys on.
    pub id: String,
    /// The application's reference, echoed back.
    pub reference: String,
    pub status: ChargeStatus,
    pub channel: Channel,
    pub amount: Money,
    pub instructions: Instructions,
    /// Unix seconds.
    pub expires_at: u64,
    /// Unix seconds. `None` until paid.
    pub paid_at: Option<u64>,
    pub metadata: Json,
}

impl Charge {
    pub fn to_json(&self) -> Json {
        Json::object([
            ("id", Json::from(self.id.as_str())),
            ("reference", Json::from(self.reference.as_str())),
            ("status", Json::from(self.status.as_str())),
            ("channel", Json::from(self.channel.code())),
            ("amount", self.amount.to_json()),
            (
                "instructions",
                Json::object([
                    ("account_number", opt(&self.instructions.account_number)),
                    ("qr_string", opt(&self.instructions.qr_string)),
                    ("url", opt(&self.instructions.url)),
                    ("payment_code", opt(&self.instructions.payment_code)),
                ]),
            ),
            ("expires_at", Json::from(self.expires_at as i64)),
            ("paid_at", self.paid_at.map_or(Json::Null, |at| Json::from(at as i64))),
            ("metadata", self.metadata.clone()),
        ])
    }
}

fn opt(value: &Option<String>) -> Json {
    value.as_deref().map_or(Json::Null, Json::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Bank;

    #[test]
    fn a_request_is_validated_before_a_driver_sees_it() {
        let good = ChargeRequest::new("order-7", Money::idr(150_000), Channel::Qris);
        assert!(good.validate().is_ok());

        let nothing = ChargeRequest::new("order-7", Money::idr(0), Channel::Qris);
        assert!(nothing.validate().is_err());

        let unnamed = ChargeRequest::new("  ", Money::idr(1), Channel::Qris);
        assert!(unnamed.validate().is_err(), "a charge with no reference cannot be matched");

        let instant = ChargeRequest::new("x", Money::idr(1), Channel::VirtualAccount(Bank::Bca))
            .expires_in(Duration::ZERO);
        assert!(instant.validate().is_err());
    }

    /// Gateways spell "paid" several ways; the application should not have to
    /// know which.
    #[test]
    fn a_status_is_read_from_the_spellings_gateways_use() {
        assert_eq!(ChargeStatus::parse("PAID"), Some(ChargeStatus::Paid));
        assert_eq!(ChargeStatus::parse("settled"), Some(ChargeStatus::Paid));
        assert_eq!(ChargeStatus::parse("canceled"), Some(ChargeStatus::Cancelled));
        assert_eq!(ChargeStatus::parse("maybe"), None, "an unknown status must not become one");
        assert!(ChargeStatus::Paid.is_final());
        assert!(!ChargeStatus::Pending.is_final());
    }
}
