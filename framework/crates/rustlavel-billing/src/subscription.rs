//! A customer on a plan, and the invoices that keep them there.

use rustlavel_core::Json;
use rustlavel_payment::{Channel, Instructions, Money};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionStatus {
    /// Subscribed, first invoice unpaid. No access yet.
    Pending,
    /// Paid up to `period_end`.
    Active,
    /// The period has ended and the renewal is unpaid. Still served, until
    /// the plan's grace runs out.
    Grace,
    /// Grace ran out. Not served; [`crate::Billing::renew`] raises the
    /// invoice that brings it back.
    Suspended,
    /// Over. A new subscription is a new row.
    Cancelled,
}

impl SubscriptionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SubscriptionStatus::Pending => "pending",
            SubscriptionStatus::Active => "active",
            SubscriptionStatus::Grace => "grace",
            SubscriptionStatus::Suspended => "suspended",
            SubscriptionStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(text: &str) -> Option<SubscriptionStatus> {
        Some(match text {
            "pending" => SubscriptionStatus::Pending,
            "active" => SubscriptionStatus::Active,
            "grace" => SubscriptionStatus::Grace,
            "suspended" => SubscriptionStatus::Suspended,
            "cancelled" => SubscriptionStatus::Cancelled,
            _ => return None,
        })
    }

    /// Whether the customer is served in this state. Grace counts: that is
    /// what grace is for.
    pub fn is_served(self) -> bool {
        matches!(self, SubscriptionStatus::Active | SubscriptionStatus::Grace)
    }
}

/// Who is subscribing, as much as a payment screen needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Subscriber {
    /// The application's id for the customer — also the ledger account owner
    /// the plan's credits are paid into.
    pub id: String,
    pub name: String,
    pub email: Option<String>,
    pub phone: Option<String>,
}

impl Subscriber {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Subscriber {
        Subscriber { id: id.into(), name: name.into(), email: None, phone: None }
    }

    pub fn email(mut self, email: impl Into<String>) -> Subscriber {
        self.email = Some(email.into());
        self
    }

    pub fn phone(mut self, phone: impl Into<String>) -> Subscriber {
        self.phone = Some(phone.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub id: String,
    pub subscriber: Subscriber,
    pub plan_id: String,
    /// Set by [`crate::Billing::change_plan`]; becomes `plan_id` when the next
    /// invoice is paid.
    pub next_plan_id: Option<String>,
    pub status: SubscriptionStatus,
    /// How the customer paid last time, and how the renewal will be offered.
    pub channel: Channel,
    /// Unix seconds. Both zero until the first invoice is paid.
    pub period_start: u64,
    pub period_end: u64,
    /// Set by [`crate::Billing::cancel`]: no renewal is raised, and the
    /// subscription ends when the period does.
    pub cancel_at_period_end: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

impl Subscription {
    /// Served at `now`? Active or in grace, and — because a scheduler may not
    /// have run since the period ended — not past the grace the plan allows.
    pub fn is_served_at(&self, now: u64, grace_seconds: u64) -> bool {
        self.status.is_served() && now < self.period_end.saturating_add(grace_seconds)
    }

    pub fn to_json(&self) -> Json {
        Json::object([
            ("id", Json::from(self.id.as_str())),
            ("customer", Json::from(self.subscriber.id.as_str())),
            ("plan", Json::from(self.plan_id.as_str())),
            ("next_plan", opt(&self.next_plan_id)),
            ("status", Json::from(self.status.as_str())),
            ("channel", Json::from(self.channel.code())),
            ("period_start", Json::from(self.period_start as i64)),
            ("period_end", Json::from(self.period_end as i64)),
            ("cancel_at_period_end", Json::from(self.cancel_at_period_end)),
        ])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceStatus {
    /// Raised and payable.
    Open,
    Paid,
    /// Withdrawn by the application — a plan change, a cancellation — before
    /// it was paid. Its charge was cancelled at the gateway.
    Void,
    /// Not paid in time. The subscription it was for is suspended or
    /// cancelled; paying it now is not possible.
    Expired,
}

impl InvoiceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            InvoiceStatus::Open => "open",
            InvoiceStatus::Paid => "paid",
            InvoiceStatus::Void => "void",
            InvoiceStatus::Expired => "expired",
        }
    }

    pub fn parse(text: &str) -> Option<InvoiceStatus> {
        Some(match text {
            "open" => InvoiceStatus::Open,
            "paid" => InvoiceStatus::Paid,
            "void" => InvoiceStatus::Void,
            "expired" => InvoiceStatus::Expired,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Invoice {
    /// Also the charge's `reference` at the gateway and the ledger reference
    /// of the credits it buys — one id through all three, so a webhook and a
    /// top-up are matched to the invoice with no lookup table.
    pub id: String,
    pub subscription_id: String,
    pub customer: String,
    pub plan_id: String,
    /// 1 for the first invoice, then counting. Unique per subscription, which
    /// is what stops two schedulers raising the same renewal.
    pub number: i64,
    pub amount: Money,
    /// Credited when paid.
    pub credits: i64,
    /// The period this invoice buys. For the first invoice both are zero
    /// until it is paid, because access starts at payment, not at signup.
    pub period_start: u64,
    pub period_end: u64,
    pub status: InvoiceStatus,
    /// The gateway's charge id. `None` while the charge has not been created
    /// yet — the gateway was down when the invoice was raised — or after the
    /// gateway expired it; the next tick creates a fresh one either way.
    pub charge_id: Option<String>,
    pub channel: Channel,
    /// How to pay: the VA number, the QR, the URL. Empty until there is a
    /// charge.
    pub instructions: Instructions,
    /// Unix seconds. After this the subscription is in grace.
    pub due_at: u64,
    pub paid_at: Option<u64>,
    pub created_at: u64,
}

impl Invoice {
    pub fn is_payable(&self) -> bool {
        self.status == InvoiceStatus::Open && self.charge_id.is_some()
    }

    pub fn to_json(&self) -> Json {
        Json::object([
            ("id", Json::from(self.id.as_str())),
            ("subscription", Json::from(self.subscription_id.as_str())),
            ("customer", Json::from(self.customer.as_str())),
            ("plan", Json::from(self.plan_id.as_str())),
            ("number", Json::from(self.number)),
            ("amount", self.amount.to_json()),
            ("credits", Json::from(self.credits)),
            ("period_start", Json::from(self.period_start as i64)),
            ("period_end", Json::from(self.period_end as i64)),
            ("status", Json::from(self.status.as_str())),
            ("charge_id", opt(&self.charge_id)),
            ("channel", Json::from(self.channel.code())),
            ("instructions", instructions_to_json(&self.instructions)),
            ("due_at", Json::from(self.due_at as i64)),
            ("paid_at", self.paid_at.map_or(Json::Null, |at| Json::from(at as i64))),
        ])
    }
}

pub(crate) fn instructions_to_json(instructions: &Instructions) -> Json {
    Json::object([
        ("account_number", opt(&instructions.account_number)),
        ("qr_string", opt(&instructions.qr_string)),
        ("url", opt(&instructions.url)),
        ("payment_code", opt(&instructions.payment_code)),
    ])
}

pub(crate) fn instructions_from_json(json: &Json) -> Instructions {
    let text = |key: &str| json.get(key).and_then(Json::as_str).map(str::to_string);
    Instructions {
        account_number: text("account_number"),
        qr_string: text("qr_string"),
        url: text("url"),
        payment_code: text("payment_code"),
    }
}

fn opt(value: &Option<String>) -> Json {
    value.as_deref().map_or(Json::Null, Json::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_round_trip() {
        for status in [
            SubscriptionStatus::Pending,
            SubscriptionStatus::Active,
            SubscriptionStatus::Grace,
            SubscriptionStatus::Suspended,
            SubscriptionStatus::Cancelled,
        ] {
            assert_eq!(SubscriptionStatus::parse(status.as_str()), Some(status));
        }
        for status in [InvoiceStatus::Open, InvoiceStatus::Paid, InvoiceStatus::Void, InvoiceStatus::Expired] {
            assert_eq!(InvoiceStatus::parse(status.as_str()), Some(status));
        }
    }

    #[test]
    fn grace_is_served_and_suspension_is_not() {
        assert!(SubscriptionStatus::Grace.is_served());
        assert!(!SubscriptionStatus::Suspended.is_served());
        assert!(!SubscriptionStatus::Pending.is_served());
    }

    /// The scheduler may be late. A subscription whose grace ran out an hour
    /// ago must not be served just because nothing has flipped its status yet.
    #[test]
    fn a_stale_grace_status_does_not_grant_access_past_the_grace() {
        let subscription = Subscription {
            id: "sub".into(),
            subscriber: Subscriber::new("user:1", "Ada"),
            plan_id: "pro".into(),
            next_plan_id: None,
            status: SubscriptionStatus::Grace,
            channel: Channel::Qris,
            period_start: 1_000,
            period_end: 2_000,
            cancel_at_period_end: false,
            created_at: 1_000,
            updated_at: 1_000,
        };
        assert!(subscription.is_served_at(2_500, 1_000));
        assert!(!subscription.is_served_at(3_000, 1_000));
    }

    #[test]
    fn instructions_survive_the_json_column() {
        let instructions = Instructions {
            account_number: Some("8808123".into()),
            qr_string: None,
            url: Some("https://pay.test/x".into()),
            payment_code: None,
        };
        assert_eq!(instructions_from_json(&instructions_to_json(&instructions)), instructions);
    }
}
