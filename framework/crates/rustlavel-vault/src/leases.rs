//! Keeping a grant alive, and ending it early.
//!
//! A lease is only a security property if somebody acts on it. Renewing is the
//! boring half: a process holding database credentials has to ask for more time
//! before two thirds of the lease has gone, or the account is dropped underneath
//! it. Revoking is the half that matters when something goes wrong — it is the
//! difference between "the leaked credential expires in an hour" and "the leaked
//! credential stopped working the moment we noticed".
//!
//! Vault splits these across two vocabularies for no reason a caller cares
//! about: everything with a lease id goes through `sys/leases/…`, and a token
//! goes through `auth/token/…-self` because a token's lease has no id.

use crate::client::VaultClient;
use crate::error::VaultError;
use rustlavel_core::Result;
use crate::lease::Lease;
use rustlavel_core::Json;
use std::time::Duration;

impl VaultClient {
    /// Ask for more time on a lease.
    ///
    /// The increment is a request, not an instruction: Vault caps it at the
    /// role's max TTL and answers with what it actually gave, which is why the
    /// renewed lease has to come from the response rather than from the
    /// argument. Renewing a lease that has already lapsed fails — there is
    /// nothing left to extend, and the account is already gone.
    pub async fn renew_lease(&self, lease: &Lease, increment: Option<Duration>) -> Result<Lease> {
        if lease.id().is_empty() {
            return Err(VaultError::BadRequest(
                "this lease has no id, so it is a token's. Renew it with `renew_token`.".into(),
            )
            .into());
        }

        let mut body = vec![("lease_id".to_string(), Json::from(lease.id()))];
        if let Some(increment) = increment {
            body.push(("increment".into(), Json::Number(increment.as_secs() as f64)));
        }

        let response = self.post("sys/leases/renew", Json::object(body)).await?;
        Ok(lease.renewed(response.lease.duration(), response.lease.renewable()))
    }

    /// End a lease now, deleting whatever it was holding open.
    ///
    /// Idempotent: Vault answers 204 for a lease it has never heard of, so a
    /// shutdown path that revokes twice does not need to care.
    pub async fn revoke_lease(&self, lease: &Lease) -> Result<()> {
        if lease.id().is_empty() {
            return Err(VaultError::BadRequest(
                "this lease has no id, so it is a token's. Revoke it with `revoke_token`.".into(),
            )
            .into());
        }

        self.post("sys/leases/revoke", Json::object([("lease_id", Json::from(lease.id()))]))
            .await
            .map(|_| ())
    }

    /// What Vault currently thinks of a lease.
    ///
    /// The honest way to find out whether a credential is still alive, as
    /// opposed to asking the local [`Lease`], which only knows what it was told
    /// when it was issued.
    pub async fn lookup_lease(&self, lease_id: &str) -> Result<Lease> {
        let response =
            self.post("sys/leases/lookup", Json::object([("lease_id", Json::from(lease_id))]))
                .await?;
        let data = &response.data;

        let seconds = data.get("ttl").and_then(Json::as_i64).unwrap_or(0).max(0);
        let renewable = data.get("renewable").and_then(Json::as_bool).unwrap_or(false);

        Ok(Lease::new(lease_id, Duration::from_secs(seconds as u64), renewable))
    }

    /// Ask for more time on the current token.
    ///
    /// The call a long-running service makes on a timer. Losing the token loses
    /// access to every secret behind it, so this failing is worth an alert
    /// rather than a log line.
    pub async fn renew_token(&self, increment: Option<Duration>) -> Result<Lease> {
        let mut body = Vec::new();
        if let Some(increment) = increment {
            body.push(("increment".to_string(), Json::Number(increment.as_secs() as f64)));
        }

        let response = self.post("auth/token/renew-self", Json::object(body)).await?;
        Ok(response.lease)
    }

    /// Give up the current token, and forget it.
    ///
    /// Worth calling on a clean shutdown: a token that outlives the process it
    /// was issued for is a credential nobody is watching. The token is cleared
    /// here as well, so a later request fails as "no token" rather than as a
    /// 403 that looks like a policy problem.
    pub async fn revoke_token(&self) -> Result<()> {
        let result = self.post("auth/token/revoke-self", Json::Null).await.map(|_| ());
        self.clear_token();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_client::{Fake, FakeResponse};

    fn vault(fake: Fake) -> VaultClient {
        VaultClient::new("https://vault.test:8200").faking(fake)
    }

    fn lease() -> Lease {
        Lease::new("database/creds/app-readwrite/5p1PU", Duration::from_secs(3600), true)
    }

    #[tokio::test]
    async fn renewing_a_lease_takes_the_duration_vault_actually_granted() {
        // Asked for two hours, capped at the role's max. Trusting the request
        // instead of the answer would leave the process renewing an hour late.
        let capped = FakeResponse::json(
            Json::parse(
                r#"{"lease_id":"database/creds/app-readwrite/5p1PU",
                    "renewable":true,"lease_duration":3600,"data":null,"auth":null}"#,
            )
            .unwrap(),
        );
        let client = vault(Fake::new().fallback(capped));

        let renewed =
            client.renew_lease(&lease(), Some(Duration::from_secs(7200))).await.unwrap();

        assert_eq!(renewed.duration(), Duration::from_secs(3600));
        assert_eq!(renewed.id(), "database/creds/app-readwrite/5p1PU");
        assert!(!renewed.should_renew());

        let sent = client.fake().unwrap().recorded()[0].json().unwrap();
        assert_eq!(sent.get("lease_id").unwrap().as_str(), Some("database/creds/app-readwrite/5p1PU"));
        assert_eq!(sent.get("increment").unwrap().as_i64(), Some(7200));
        client.fake().unwrap().assert_sent("/v1/sys/leases/renew");
    }

    #[tokio::test]
    async fn revoking_a_lease_sends_its_id_and_accepts_no_content() {
        let client = vault(Fake::new().fallback(FakeResponse::text("").status(204)));

        client.revoke_lease(&lease()).await.unwrap();

        client.fake().unwrap().assert_sent("/v1/sys/leases/revoke");
        let sent = client.fake().unwrap().recorded()[0].json().unwrap();
        assert!(sent.get("lease_id").is_some());
    }

    #[tokio::test]
    async fn a_token_lease_is_sent_to_the_right_endpoint_or_refused() {
        // A token's lease has no id, and posting an empty one to
        // `sys/leases/renew` fails with a message about the id rather than
        // about the token.
        let client = vault(Fake::new().fallback(FakeResponse::text("").status(204)));

        let error = client.renew_lease(&Lease::none(), None).await.unwrap_err().to_string();
        assert!(error.contains("renew_token"), "{error}");

        let error = client.revoke_lease(&Lease::none()).await.unwrap_err().to_string();
        assert!(error.contains("revoke_token"), "{error}");

        client.fake().unwrap().assert_count(0);
    }

    #[tokio::test]
    async fn renewing_the_token_reads_the_lease_back_from_auth() {
        // `auth/token/renew-self` answers with the lease under `auth`, the same
        // as a login and unlike `sys/leases/renew`.
        let renewed = FakeResponse::json(
            Json::parse(
                r#"{"lease_id":"","renewable":false,"lease_duration":0,"data":null,
                    "auth":{"client_token":"s.RpkkBIsBT61V1PTDZGP6eE4q",
                            "lease_duration":7200,"renewable":true}}"#,
            )
            .unwrap(),
        );
        let client = vault(Fake::new().fallback(renewed));
        client.set_token("s.RpkkBIsBT61V1PTDZGP6eE4q");

        let lease = client.renew_token(Some(Duration::from_secs(7200))).await.unwrap();

        assert_eq!(lease.duration(), Duration::from_secs(7200));
        assert!(lease.renewable());
        client.fake().unwrap().assert_sent("/v1/auth/token/renew-self");
    }

    #[tokio::test]
    async fn revoking_the_token_also_forgets_it() {
        // Keeping a revoked token would turn every later call into a 403 that
        // reads like a policy problem.
        let client = vault(Fake::new().fallback(FakeResponse::text("").status(204)));
        client.set_token("s.doomed");

        client.revoke_token().await.unwrap();

        assert!(!client.has_token());
        client.fake().unwrap().assert_sent("/v1/auth/token/revoke-self");
    }

    #[tokio::test]
    async fn looking_a_lease_up_reports_what_the_server_believes() {
        let body = FakeResponse::json(
            Json::parse(
                r#"{"data":{"expire_time":"2026-08-29T14:49:47Z",
                            "id":"database/creds/app-readwrite/5p1PU",
                            "issue_time":"2026-08-29T13:49:47Z","last_renewal":null,
                            "path":"database/creds/app-readwrite","renewable":true,"ttl":3600}}"#,
            )
            .unwrap(),
        );
        let client = vault(Fake::new().fallback(body));

        let lease = client.lookup_lease("database/creds/app-readwrite/5p1PU").await.unwrap();

        assert_eq!(lease.duration(), Duration::from_secs(3600));
        assert!(lease.renewable());
        client.fake().unwrap().assert_sent("/v1/sys/leases/lookup");
    }

    #[tokio::test]
    async fn a_revoked_lease_can_no_longer_be_looked_up() {
        // Observed from OpenBao: a 400 saying "invalid lease", not a 404.
        let gone = FakeResponse::text(r#"{"errors":["invalid lease"]}"#).status(400);
        let client = vault(Fake::new().fallback(gone));

        let error = client.lookup_lease("database/creds/app/gone").await.unwrap_err();

        // Vault says "invalid lease" with a 400, not a 404 — a lease that has
        // been revoked is gone rather than missing, and the message is the part
        // worth asserting.
        let error = error.to_string();
        assert!(error.contains("invalid lease"), "{error}");
        assert!(!error.contains("nothing is stored at"), "{error}");
    }
}
