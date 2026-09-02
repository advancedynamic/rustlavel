//! Storing passkeys in the database, and challenges in the session.
//!
//! `rustlavel-webauthn` defines the two stores it needs as traits and ships
//! in-memory implementations, which are right for tests and wrong for a
//! running site: credentials must outlive a restart, and a process that
//! forgets them locks everybody out.
//!
//! Challenges go in the **session** rather than a shared store, which is a
//! stronger arrangement than the trait requires. A challenge kept per session
//! cannot be answered by a different browser even if it leaks, and it is
//! removed on the way out, so it stays single-use.

use rustlavel::prelude::*;
use rustlavel::webauthn::{
    Ceremony, Challenge, ChallengeStore, Credential, CredentialStore, RelyingParty, UserEntity,
};
use std::sync::Arc;

use crate::support::tokens;

/// Credentials in the `user_passkeys` table.
pub struct DbPasskeys {
    db: Database,
}

impl DbPasskeys {
    pub fn new(db: Database) -> Self {
        DbPasskeys { db }
    }

    /// The rows a settings page lists, newest first.
    pub async fn list_for(&self, user_id: i64) -> Result<Vec<Json>> {
        let rows = self
            .db
            .table("user_passkeys")
            .filter("user_id", user_id)
            .latest("id")
            .get(&self.db)
            .await?;

        Ok(rows
            .iter()
            .map(|row| {
                Json::object([
                    ("id", Json::from(row.get::<i64>("id").unwrap_or_default())),
                    (
                        "label",
                        Json::from(
                            row.get::<String>("label").unwrap_or_else(|_| "Passkey".into()),
                        ),
                    ),
                    (
                        "created_at",
                        Json::from(tokens::humanise(&row.get::<String>("created_at").unwrap_or_default())),
                    ),
                    (
                        "last_used_at",
                        Json::from(match row.get::<String>("last_used_at") {
                            Ok(at) if !at.is_empty() => tokens::humanise(&at),
                            _ => "never".to_string(),
                        }),
                    ),
                ])
            })
            .collect())
    }

    pub async fn delete(&self, user_id: i64, id: i64) -> Result<u64> {
        // Scoped to the owner. Without the user_id an id from another account
        // would delete somebody else's key.
        self.db
            .table("user_passkeys")
            .filter("id", id)
            .filter("user_id", user_id)
            .delete(&self.db)
            .await
    }

    pub async fn count_for(&self, user_id: i64) -> Result<i64> {
        self.db.table("user_passkeys").filter("user_id", user_id).count(&self.db).await
    }

    fn to_credential(row: &rustlavel::db::Row) -> Result<Credential> {
        use rustlavel::auth::base64;
        let id = base64::decode(&row.get::<String>("credential_id")?)
            .ok_or_else(|| Error::msg("a stored credential id is not valid base64url"))?;
        let key_bytes = base64::decode(&row.get::<String>("public_key")?)
            .ok_or_else(|| Error::msg("a stored public key is not valid base64url"))?;
        let key = rustlavel::webauthn::CoseKey::parse(&rustlavel::webauthn::Cbor::parse(&key_bytes)?)?;
        let user_handle = row.get::<i64>("user_id")?.to_string().into_bytes();

        Ok(Credential::new(
            id,
            key,
            row.get::<i64>("sign_count").unwrap_or(0) as u32,
            user_handle,
            [0u8; 16],
            0,
        ))
    }
}

impl CredentialStore for DbPasskeys {
    fn create(&self, credential: Credential) -> rustlavel::webauthn::credential::CredentialFuture<'_, Credential> {
        Box::pin(async move {
            use rustlavel::auth::base64;
            let user_id: i64 = String::from_utf8_lossy(credential.user_handle())
                .parse()
                .map_err(|_| Error::msg("a passkey user handle is not a user id"))?;

            self.db
                .table("user_passkeys")
                .insert_without_id(
                    &self.db,
                    &[
                        ("user_id", user_id.into()),
                        ("credential_id", base64::encode_url(credential.id()).into()),
                        ("public_key", base64::encode_url(&credential.key().to_bytes()).into()),
                        ("sign_count", (credential.sign_count() as i64).into()),
                        ("created_at", tokens::now().into()),
                        ("updated_at", tokens::now().into()),
                    ],
                )
                .await?;
            Ok(credential)
        })
    }

    fn find<'a>(&'a self, id: &'a [u8]) -> rustlavel::webauthn::credential::CredentialFuture<'a, Option<Credential>> {
        Box::pin(async move {
            use rustlavel::auth::base64;
            let rows = self
                .db
                .table("user_passkeys")
                .filter("credential_id", base64::encode_url(id))
                .get(&self.db)
                .await?;
            rows.first().map(DbPasskeys::to_credential).transpose()
        })
    }

    fn find_for_user<'a>(
        &'a self,
        user_handle: &'a [u8],
    ) -> rustlavel::webauthn::credential::CredentialFuture<'a, Vec<Credential>> {
        Box::pin(async move {
            let user_id: i64 = String::from_utf8_lossy(user_handle).parse().unwrap_or(0);
            let rows =
                self.db.table("user_passkeys").filter("user_id", user_id).get(&self.db).await?;
            rows.iter().map(DbPasskeys::to_credential).collect()
        })
    }

    fn delete<'a>(&'a self, id: &'a [u8]) -> rustlavel::webauthn::credential::CredentialFuture<'a, ()> {
        Box::pin(async move {
            use rustlavel::auth::base64;
            self.db
                .table("user_passkeys")
                .filter("credential_id", base64::encode_url(id))
                .delete(&self.db)
                .await?;
            Ok(())
        })
    }

    fn record_use<'a>(
        &'a self,
        id: &'a [u8],
        sign_count: u32,
        at: u64,
    ) -> rustlavel::webauthn::credential::CredentialFuture<'a, ()> {
        Box::pin(async move {
            use rustlavel::auth::base64;
            // The counter write is what lets a cloned authenticator be spotted
            // later: a value that goes backwards means two devices hold the
            // same key.
            self.db
                .table("user_passkeys")
                .filter("credential_id", base64::encode_url(id))
                .update(
                    &self.db,
                    &[
                        ("sign_count", (sign_count as i64).into()),
                        ("last_used_at", tokens::format_utc(at as i64).into()),
                    ],
                )
                .await?;
            Ok(())
        })
    }
}

/// Challenges kept in the session that started the ceremony.
pub struct SessionChallenges {
    session: rustlavel::auth::SessionHandle,
}

impl SessionChallenges {
    pub fn new(session: rustlavel::auth::SessionHandle) -> Self {
        SessionChallenges { session }
    }

    fn key(encoded: &str) -> String {
        format!("_webauthn:{encoded}")
    }
}

impl ChallengeStore for SessionChallenges {
    fn store(&self, challenge: Challenge) -> rustlavel::webauthn::challenge::ChallengeFuture<'_, ()> {
        let key = SessionChallenges::key(&challenge.encoded());
        let record = Json::object([
            ("ceremony", Json::from(challenge.ceremony().name())),
            ("expires_at", Json::from(challenge.expires_at() as i64)),
            (
                "user_handle",
                challenge
                    .user_handle()
                    .map_or(Json::Null, |handle| Json::from(String::from_utf8_lossy(handle).to_string())),
            ),
        ]);
        self.session.put(key, record);
        Box::pin(async { Ok(()) })
    }

    fn take<'a>(
        &'a self,
        encoded: &'a str,
    ) -> rustlavel::webauthn::challenge::ChallengeFuture<'a, Option<Challenge>> {
        // Removed as it is read, which is the whole contract: a challenge that
        // can be answered twice is a password.
        let stored = self.session.forget(&SessionChallenges::key(encoded));
        let encoded = encoded.to_string();
        Box::pin(async move {
            let Some(record) = stored else { return Ok(None) };
            let ceremony = match record.get("ceremony").and_then(Json::as_str) {
                Some("registration") => Ceremony::Registration,
                Some("authentication") => Ceremony::Authentication,
                _ => return Ok(None),
            };
            let expires_at = record.get("expires_at").and_then(Json::as_i64).unwrap_or(0) as u64;
            let user_handle = record
                .get("user_handle")
                .and_then(Json::as_str)
                .map(|handle| handle.as_bytes().to_vec());
            // The key is the challenge itself in base64url, so the bytes come
            // straight back out of it rather than being stored a second time.
            let Some(bytes) = rustlavel::auth::base64::decode(&encoded) else { return Ok(None) };
            Challenge::restore(bytes, ceremony, expires_at, user_handle).map(Some)
        })
    }
}

/// The relying party this application is, from configuration.
pub fn relying_party(req: &Request) -> Result<RelyingParty> {
    RelyingParty::from_config(req.config())
}

/// The WebAuthn identity of a user. The handle is their id as text, so a
/// stored credential can be traced back to a row.
pub fn user_entity(id: i64, name: &str, display: &str) -> UserEntity {
    UserEntity::new(id.to_string().into_bytes(), name, display)
}

/// Both stores, ready for one request.
pub fn stores(req: &Request) -> (Arc<DbPasskeys>, Arc<SessionChallenges>) {
    let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
    (Arc::new(DbPasskeys::new(db)), Arc::new(SessionChallenges::new(req.session().clone())))
}
