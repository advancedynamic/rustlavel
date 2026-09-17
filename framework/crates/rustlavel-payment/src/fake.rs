//! A gateway that keeps everything in memory and never talks to anybody.
//!
//! For tests and for a sandbox: the whole payment flow — charge, customer
//! pays, webhook arrives, application credits — can be driven end to end with
//! no account at any gateway. `mark_paid` is the customer; `webhook_for` is
//! the gateway's callback, signed the way a real one is so the receiver's
//! verification path is the one being exercised.

use crate::channel::Channel;
use crate::charge::{Charge, ChargeRequest, ChargeStatus, Instructions};
use crate::gateway::{Gateway, GatewayFuture};
use crate::signature::{hmac_sha256_hex, verify_hmac_sha256_hex};
use crate::transfer::{Transfer, TransferRequest, TransferStatus};
use crate::webhook::{EventKind, WebhookEvent};
use rustlavel_core::{Error, Json, Result};
use rustlavel_http::Headers;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The header the fake signs into. A real driver has its own.
pub const SIGNATURE_HEADER: &str = "x-fake-signature";

pub struct FakeGateway {
    secret: Vec<u8>,
    next: AtomicU64,
    charges: Mutex<HashMap<String, Charge>>,
    transfers: Mutex<HashMap<String, Transfer>>,
    /// References already used for a transfer. A real gateway refuses a
    /// second transfer under the same reference, and so does this one —
    /// that refusal is the protection against a retry paying somebody twice,
    /// and a fake that let it through would let a test pass that should not.
    transfer_references: Mutex<HashMap<String, String>>,
}

impl FakeGateway {
    pub fn new(secret: impl Into<Vec<u8>>) -> FakeGateway {
        FakeGateway {
            secret: secret.into(),
            next: AtomicU64::new(1),
            charges: Mutex::new(HashMap::new()),
            transfers: Mutex::new(HashMap::new()),
            transfer_references: Mutex::new(HashMap::new()),
        }
    }

    fn id(&self, prefix: &str) -> String {
        format!("{prefix}_{}", self.next.fetch_add(1, Ordering::Relaxed))
    }

    /// The customer pays. `None` if there is no such charge or it is not
    /// pending — a paid charge cannot be paid again.
    pub fn mark_paid(&self, id: &str) -> Option<Charge> {
        let mut charges = self.charges.lock().expect("the fake's charges are poisoned");
        let charge = charges.get_mut(id)?;
        if charge.status != ChargeStatus::Pending {
            return None;
        }
        charge.status = ChargeStatus::Paid;
        charge.paid_at = Some(now());
        Some(charge.clone())
    }

    /// Time passes and the customer does not pay.
    pub fn mark_expired(&self, id: &str) -> Option<Charge> {
        let mut charges = self.charges.lock().expect("the fake's charges are poisoned");
        let charge = charges.get_mut(id)?;
        if charge.status != ChargeStatus::Pending {
            return None;
        }
        charge.status = ChargeStatus::Expired;
        Some(charge.clone())
    }

    /// The gateway's callback for a charge in its current state: the body
    /// and the headers, signed so [`Gateway::verify_webhook`] accepts them.
    ///
    /// `event_id` is what deduplication keys on. Pass the same one twice to
    /// simulate the retry a real gateway sends when it does not hear the
    /// acknowledgement.
    pub fn webhook_for(&self, charge_id: &str, event_id: &str) -> Option<(Headers, Vec<u8>)> {
        let charge = self.charges.lock().expect("the fake's charges are poisoned").get(charge_id)?.clone();
        let kind = match charge.status {
            ChargeStatus::Paid => "charge.paid",
            ChargeStatus::Expired => "charge.expired",
            ChargeStatus::Failed => "charge.failed",
            _ => return None,
        };
        let body = Json::object([
            ("event_id", Json::from(event_id)),
            ("type", Json::from(kind)),
            ("charge", charge.to_json()),
        ])
        .to_string()
        .into_bytes();
        Some((self.sign(&body), body))
    }

    /// Sign arbitrary bytes the way the fake's callbacks are signed, for a
    /// test that wants to send a tampered body.
    pub fn sign(&self, body: &[u8]) -> Headers {
        let mut headers = Headers::new();
        headers.set(SIGNATURE_HEADER, hmac_sha256_hex(&self.secret, body));
        headers.set("content-type", "application/json");
        headers
    }

    pub fn charges(&self) -> Vec<Charge> {
        self.charges.lock().expect("the fake's charges are poisoned").values().cloned().collect()
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn instructions_for(channel: Channel, id: &str) -> Instructions {
    match channel {
        Channel::VirtualAccount(_) => Instructions {
            account_number: Some(format!("8808{:012}", id.len() * 7919)),
            ..Instructions::default()
        },
        Channel::Qris => Instructions {
            qr_string: Some(format!("00020101021226fake{id}6304ABCD")),
            ..Instructions::default()
        },
        Channel::EWallet(_) => Instructions {
            url: Some(format!("https://fake.gateway.test/pay/{id}")),
            ..Instructions::default()
        },
        Channel::Retail(_) => Instructions {
            payment_code: Some(format!("{:08}", id.len() * 104729)),
            ..Instructions::default()
        },
    }
}

impl Gateway for FakeGateway {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn create_charge<'a>(&'a self, request: &'a ChargeRequest) -> GatewayFuture<'a, Charge> {
        Box::pin(async move {
            request.validate()?;
            let id = self.id("fake_ch");
            let charge = Charge {
                id: id.clone(),
                reference: request.reference.clone(),
                status: ChargeStatus::Pending,
                channel: request.channel,
                amount: request.amount.clone(),
                instructions: instructions_for(request.channel, &id),
                expires_at: now() + request.expires_in.as_secs(),
                paid_at: None,
                metadata: request.metadata.clone(),
            };
            self.charges.lock().expect("the fake's charges are poisoned").insert(id, charge.clone());
            Ok(charge)
        })
    }

    fn charge<'a>(&'a self, id: &'a str) -> GatewayFuture<'a, Charge> {
        Box::pin(async move {
            self.charges
                .lock()
                .expect("the fake's charges are poisoned")
                .get(id)
                .cloned()
                .ok_or_else(|| Error::msg(format!("no charge `{id}`")))
        })
    }

    fn cancel_charge<'a>(&'a self, id: &'a str) -> GatewayFuture<'a, Charge> {
        Box::pin(async move {
            let mut charges = self.charges.lock().expect("the fake's charges are poisoned");
            let charge = charges.get_mut(id).ok_or_else(|| Error::msg(format!("no charge `{id}`")))?;
            if charge.status != ChargeStatus::Pending {
                return Err(Error::msg(format!(
                    "charge `{id}` is {} and cannot be cancelled; a paid charge is refunded, not cancelled",
                    charge.status.as_str()
                )));
            }
            charge.status = ChargeStatus::Cancelled;
            Ok(charge.clone())
        })
    }

    fn transfer<'a>(&'a self, request: &'a TransferRequest) -> GatewayFuture<'a, Transfer> {
        Box::pin(async move {
            request.validate()?;
            {
                let refs = self.transfer_references.lock().expect("poisoned");
                if let Some(existing) = refs.get(&request.reference) {
                    return Err(Error::msg(format!(
                        "a transfer with reference `{}` already exists as `{existing}`",
                        request.reference
                    )));
                }
            }
            let id = self.id("fake_tr");
            let transfer = Transfer {
                id: id.clone(),
                reference: request.reference.clone(),
                status: TransferStatus::Completed,
                amount: request.amount.clone(),
                failure: None,
            };
            self.transfer_references.lock().expect("poisoned").insert(request.reference.clone(), id.clone());
            self.transfers.lock().expect("poisoned").insert(id, transfer.clone());
            Ok(transfer)
        })
    }

    fn transfer_status<'a>(&'a self, id: &'a str) -> GatewayFuture<'a, Transfer> {
        Box::pin(async move {
            self.transfers
                .lock()
                .expect("poisoned")
                .get(id)
                .cloned()
                .ok_or_else(|| Error::msg(format!("no transfer `{id}`")))
        })
    }

    fn verify_webhook<'a>(&'a self, headers: &'a Headers, body: &'a [u8]) -> Result<WebhookEvent> {
        let presented = headers
            .get(SIGNATURE_HEADER)
            .ok_or_else(|| Error::msg(format!("no {SIGNATURE_HEADER} header")))?;
        if !verify_hmac_sha256_hex(&self.secret, body, presented) {
            return Err(Error::msg("the signature did not verify"));
        }

        // Only now is the body looked at.
        let json = Json::parse(&String::from_utf8_lossy(body))?;
        let event_id = json.get("event_id").and_then(Json::as_str).ok_or_else(|| Error::msg("no event_id"))?;
        let kind = match json.get("type").and_then(Json::as_str).unwrap_or("") {
            "charge.paid" => EventKind::ChargePaid,
            "charge.expired" => EventKind::ChargeExpired,
            "charge.failed" => EventKind::ChargeFailed,
            other => EventKind::Other(other.to_string()),
        };
        let charge = json.get("charge").and_then(charge_from_json);

        Ok(WebhookEvent { id: event_id.to_string(), kind, charge, transfer: None, raw: json })
    }
}

fn charge_from_json(json: &Json) -> Option<Charge> {
    Some(Charge {
        id: json.get("id")?.as_str()?.to_string(),
        reference: json.get("reference")?.as_str()?.to_string(),
        status: ChargeStatus::parse(json.get("status")?.as_str()?)?,
        channel: Channel::parse(json.get("channel")?.as_str()?)?,
        amount: crate::money::Money::from_json(json.get("amount")?)?,
        instructions: Instructions {
            account_number: json.get("instructions.account_number").and_then(Json::as_str).map(str::to_string),
            qr_string: json.get("instructions.qr_string").and_then(Json::as_str).map(str::to_string),
            url: json.get("instructions.url").and_then(Json::as_str).map(str::to_string),
            payment_code: json.get("instructions.payment_code").and_then(Json::as_str).map(str::to_string),
        },
        expires_at: json.get("expires_at").and_then(Json::as_i64).unwrap_or(0).max(0) as u64,
        paid_at: json.get("paid_at").and_then(Json::as_i64).map(|at| at.max(0) as u64),
        metadata: json.get("metadata").cloned().unwrap_or(Json::Null),
    })
}
