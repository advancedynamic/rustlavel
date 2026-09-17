//! The trait a driver implements.

use crate::charge::{Charge, ChargeRequest};
use crate::transfer::{Transfer, TransferRequest};
use crate::webhook::WebhookEvent;
use rustlavel_core::Result;
use rustlavel_http::Headers;
use std::future::Future;
use std::pin::Pin;

pub type GatewayFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// A payment gateway.
///
/// Boxed futures rather than `async fn`, because an application holds
/// whichever driver it configured as `dyn Gateway` and swaps it for the fake
/// in tests.
pub trait Gateway: Send + Sync + 'static {
    /// Shown in logs and in the webhook log, so an event can be told apart by
    /// where it came from.
    fn name(&self) -> &'static str;

    /// Ask the customer for money. The returned [`Charge`] carries what they
    /// need to pay it.
    fn create_charge<'a>(&'a self, request: &'a ChargeRequest) -> GatewayFuture<'a, Charge>;

    /// The charge as the gateway sees it now. For reconciliation, and for the
    /// case where a webhook was missed.
    fn charge<'a>(&'a self, id: &'a str) -> GatewayFuture<'a, Charge>;

    /// Withdraw a charge the customer has not paid. A paid charge cannot be
    /// cancelled — that is a refund, and a different conversation.
    fn cancel_charge<'a>(&'a self, id: &'a str) -> GatewayFuture<'a, Charge>;

    /// Send money out.
    fn transfer<'a>(&'a self, request: &'a TransferRequest) -> GatewayFuture<'a, Transfer>;

    /// Send money out to several accounts. The default sends them one at a
    /// time; a driver whose gateway has a batch endpoint overrides it. Either
    /// way every result is returned, failures included, in the order asked.
    fn batch_transfer<'a>(
        &'a self,
        requests: &'a [TransferRequest],
    ) -> GatewayFuture<'a, Vec<Result<Transfer>>> {
        Box::pin(async move {
            let mut results = Vec::with_capacity(requests.len());
            for request in requests {
                results.push(self.transfer(request).await);
            }
            Ok(results)
        })
    }

    fn transfer_status<'a>(&'a self, id: &'a str) -> GatewayFuture<'a, Transfer>;

    /// Check a webhook's signature and read the event out of it.
    ///
    /// **The body is the raw bytes, not parsed JSON.** A signature is over
    /// the bytes the gateway sent; re-serialising parsed JSON reorders keys
    /// and drops whitespace, and the signature no longer matches. The
    /// framework hands the bytes over untouched for exactly this reason.
    ///
    /// An error means "not from the gateway": refuse with `401` and do not
    /// look inside.
    fn verify_webhook<'a>(&'a self, headers: &'a Headers, body: &'a [u8]) -> Result<WebhookEvent>;
}
