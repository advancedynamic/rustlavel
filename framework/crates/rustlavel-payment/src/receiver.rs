//! The endpoint a gateway calls.
//!
//! Three things, in order, and the order is the point:
//!
//! 1. Verify the signature. Anything that fails is `401` and is not parsed
//!    further — a body that did not come from the gateway is not looked at.
//! 2. Record the event under the gateway's id. A duplicate is answered `200`
//!    *without* running the handler: the gateway retried because it did not
//!    hear the first acknowledgement, and it needs to hear this one.
//! 3. Run the application's handler. If it fails, answer `500` so the gateway
//!    retries — and undo the record, so the retry is not then dropped as a
//!    duplicate of an event nothing handled.
//!
//! Step 3's undo is what makes "at least once" from the gateway into "exactly
//! once" at the application: a handler that crashed half-way is retried, and
//! a handler that finished is never run again.

use crate::gateway::Gateway;
use crate::webhook::{WebhookEvent, WebhookLog};
use rustlavel_core::{Json, Result};
use rustlavel_http::{BoxFuture, Request, Response, Status};
use std::sync::Arc;

/// What the receiver did with a delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handled {
    /// Verified, new, and the handler ran.
    Processed,
    /// Verified, but seen before. The handler did not run.
    Duplicate,
    /// The signature did not check out. Nothing was parsed.
    Refused,
    /// Verified and new, but the handler failed. The record was withdrawn so
    /// the gateway's retry is processed.
    HandlerFailed(String),
}

pub type Handler =
    Arc<dyn Fn(WebhookEvent) -> BoxFuture<Result<()>> + Send + Sync + 'static>;

pub struct Receiver {
    gateway: Arc<dyn Gateway>,
    log: Arc<dyn WebhookLog>,
    handler: Handler,
}

impl Receiver {
    pub fn new(
        gateway: Arc<dyn Gateway>,
        log: Arc<dyn WebhookLog>,
        handler: impl Fn(WebhookEvent) -> BoxFuture<Result<()>> + Send + Sync + 'static,
    ) -> Receiver {
        Receiver { gateway, log, handler: Arc::new(handler) }
    }

    /// Process one delivery. Separated from the HTTP handler so it can be
    /// tested without a request, and so a replay tool can call it.
    pub async fn deliver(&self, headers: &rustlavel_http::Headers, body: &[u8]) -> Handled {
        let event = match self.gateway.verify_webhook(headers, body) {
            Ok(event) => event,
            Err(error) => {
                // Warn, not error: a refused webhook is usually a
                // misconfigured secret or a probe, and neither is an incident.
                // The gateway's name says which secret to check.
                rustlavel_core::warn!(
                    "payment: refused a webhook for {}: {error}",
                    self.gateway.name()
                );
                return Handled::Refused;
            }
        };

        match self.log.record(&event).await {
            Ok(true) => {}
            Ok(false) => {
                rustlavel_core::debug!("payment: duplicate {} ignored", event.dedup_key());
                return Handled::Duplicate;
            }
            Err(error) => {
                // The log is down. Failing the delivery is the safe direction:
                // the gateway retries, and nothing is credited without a
                // record of it.
                return Handled::HandlerFailed(format!("the webhook log failed: {error}"));
            }
        }

        let key = event.dedup_key();
        match (self.handler)(event).await {
            Ok(()) => Handled::Processed,
            Err(error) => {
                rustlavel_core::error!("payment: the handler for {key} failed: {error}");
                // Withdrawn, so the retry the 500 provokes is processed. If
                // the withdrawal itself fails the event stays recorded and
                // the retry will be dropped — said loudly, because that is a
                // payment nobody will credit.
                if let Err(inner) = self.log.withdraw(&key).await {
                    rustlavel_core::error!(
                        "payment: could not withdraw {key} after a failed handler; the gateway's \
                         retry will be treated as a duplicate: {inner}"
                    );
                }
                Handled::HandlerFailed(error.to_string())
            }
        }
    }

    /// The route handler: mount it at whatever path the gateway was given.
    ///
    /// ```ignore
    /// let receiver = Arc::new(Receiver::new(gateway, log, |event| Box::pin(async move { … })));
    /// r.post("/webhooks/payment", move |req: Request| {
    ///     let receiver = Arc::clone(&receiver);
    ///     async move { receiver.handle(req).await }
    /// });
    /// ```
    ///
    /// Behind no CSRF and no session middleware: the caller is a server with
    /// a signature, and a CSRF check would refuse it every time.
    pub async fn handle(&self, request: Request) -> Response {
        match self.deliver(request.headers(), request.body()).await {
            Handled::Processed | Handled::Duplicate => Response::ok(),
            Handled::Refused => Response::new(Status::UNAUTHORIZED)
                .with_json(Json::object([("message", Json::from("the signature did not verify"))])),
            // 500 so the gateway retries. The record was not kept, so the
            // retry is processed rather than dropped as a duplicate.
            Handled::HandlerFailed(_) => Response::new(Status::INTERNAL_ERROR),
        }
    }
}
