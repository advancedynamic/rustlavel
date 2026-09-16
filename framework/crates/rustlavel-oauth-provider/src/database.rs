//! The stores, backed by a table each.
//!
//! **The memory stores are for tests and a development server.** An
//! authorization server built on them loses every issued token when the process
//! restarts, and an empty store answers "unknown token" to everything — so a
//! deploy signs out every session and invalidates every integration at once.
//! These are the ones to deploy.
//!
//! # The two operations that must be atomic
//!
//! Most of this file is reading and writing rows. Two methods are not, and they
//! are the difference between a working authorization server and a replay:
//!
//! - [`CodeStore::consume`](crate::CodeStore::consume) must spend a code
//!   *exactly once*. Two requests carrying the same code must not both be told
//!   `Fresh`.
//! - [`TokenStore::rotate`](crate::TokenStore::rotate) must rotate a refresh
//!   token *exactly once*. Two requests presenting the same live token must not
//!   both be told yes, or a stolen token racing the legitimate one is issued a
//!   second family and nothing notices.
//!
//! Both are done here as a single `UPDATE … WHERE <still unspent>` and a check
//! of how many rows it changed. The database decides the winner, because it is
//! the only party that can: a read followed by a write leaves a window between
//! them, and that window is the whole attack.
//!
//! # Schema
//!
//! [`schema`] creates the four tables, so a migration and these stores cannot
//! disagree about a column name. Call it from a migration:
//!
//! ```ignore
//! migration!(CreateOauthTables, "…_create_oauth_tables",
//!     up: |schema| { rustlavel::oauth_provider::database::schema(schema).await },
//!     down: |schema| { rustlavel::oauth_provider::database::drop_schema(schema).await },
//! );
//! ```

use rustlavel_core::Result;
use rustlavel_db::{Database, Row, Schema, Value};

use crate::client::{Client, ClientStore};
use crate::code::{AuthorizationCode, CodeStore, Consumption};
use crate::consent::{ConsentStore, Grant};
use crate::store::StoreFuture;
use crate::token::{AccessToken, RefreshToken, TokenStore};
use rustlavel_oauth::{ChallengeMethod, Scopes};

/// Create the four tables these stores read.
///
/// One definition, called from a migration, so a column renamed here cannot
/// leave a store reading a column that no longer exists.
pub async fn schema(schema: &Schema<'_>) -> Result<()> {
    schema
        .create("oauth_clients", |t| {
            t.id();
            t.string("client_id").unique();
            // Null for a public client, which proves itself with PKCE rather
            // than with a secret it was never able to keep.
            t.string("secret_hash").nullable();
            t.string("name");
            t.text("redirect_uris");
            t.text("scopes");
            t.boolean("first_party").default_bool(false);
            t.timestamps();
        })
        .await?;

    schema
        .create("oauth_codes", |t| {
            t.id();
            // The digest, never the code. A leaked backup should not be a
            // folder of working credentials.
            t.string("code_hash").unique();
            t.string("client_id");
            t.string("user_id");
            t.text("scopes");
            t.string("redirect_uri");
            t.string("challenge");
            t.string("challenge_method");
            t.big_integer("issued_at");
            t.big_integer("expires_at");
            // Null until spent. This column *is* the lock: the update that
            // spends a code requires it to be null, so exactly one caller wins.
            t.big_integer("consumed_at").nullable();
            // The family the exchange produced, so a later replay knows what to
            // revoke.
            t.string("family").nullable();
        })
        .await?;

    schema
        .create("oauth_access_tokens", |t| {
            t.id();
            t.string("token_id").unique();
            t.string("token_hash").unique();
            t.string("client_id");
            // Null for `client_credentials`: that grant has no user behind it,
            // and a resource server must be able to tell the two apart.
            t.string("user_id").nullable();
            t.text("scopes");
            t.string("family");
            t.big_integer("issued_at");
            t.big_integer("expires_at");
            // Set rather than deleted, so a revoked token can still be
            // explained afterwards. A row that vanishes answers nothing.
            t.boolean("revoked").default_bool(false);
        })
        .await?;

    schema
        .create("oauth_refresh_tokens", |t| {
            t.id();
            t.string("token_id").unique();
            t.string("token_hash").unique();
            t.string("client_id");
            t.string("user_id").nullable();
            t.text("scopes");
            t.string("family");
            t.string("access_token_id");
            t.big_integer("issued_at");
            t.big_integer("expires_at");
            t.boolean("revoked").default_bool(false);
            // The second lock, and the same shape as `consumed_at` above: the
            // update that rotates requires this to be false.
            t.boolean("rotated").default_bool(false);
        })
        .await?;

    schema
        .create("oauth_consent", |t| {
            t.id();
            t.string("client_id");
            t.string("user_id");
            t.text("scopes");
            t.big_integer("granted_at");
        })
        .await
}

/// Drop what [`schema`] created, newest first.
pub async fn drop_schema(schema: &Schema<'_>) -> Result<()> {
    schema.drop("oauth_consent").await?;
    schema.drop("oauth_refresh_tokens").await?;
    schema.drop("oauth_access_tokens").await?;
    schema.drop("oauth_codes").await?;
    schema.drop("oauth_clients").await
}

fn text(row: &Row, column: &str) -> String {
    row.get::<String>(column).unwrap_or_default()
}

fn number(row: &Row, column: &str) -> u64 {
    row.get::<i64>(column).unwrap_or_default().max(0) as u64
}

fn flag(row: &Row, column: &str) -> bool {
    row.get::<bool>(column).unwrap_or(false)
}

/// `None` rather than an empty string, because the difference is the whole
/// point: no user is a machine's token, and an empty user id is a bug.
fn optional(row: &Row, column: &str) -> Option<String> {
    row.get::<String>(column).ok().filter(|value| !value.is_empty())
}

// ---------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------

/// Clients read from `oauth_clients`.
#[derive(Clone)]
pub struct DatabaseClientStore {
    db: Database,
}

impl DatabaseClientStore {
    pub fn new(db: Database) -> DatabaseClientStore {
        DatabaseClientStore { db }
    }

    /// Record a client. The secret is digested here and the plaintext dropped,
    /// so the caller is the last thing that holds it.
    pub async fn create(&self, client: &Client, secret: Option<&str>) -> Result<()> {
        let hash = match secret {
            Some(secret) => Value::from(crate::store::digest(secret)),
            None => Value::Null,
        };

        self.db
            .table("oauth_clients")
            .insert_without_id(
                &self.db,
                &[
                    ("client_id", client.id.as_str().into()),
                    ("secret_hash", hash),
                    ("name", client.name.as_str().into()),
                    ("redirect_uris", client.redirect_uris.join(" ").into()),
                    ("scopes", client.scopes.to_string().into()),
                    ("first_party", client.first_party.into()),
                ],
            )
            .await?;
        Ok(())
    }
}

impl ClientStore for DatabaseClientStore {
    fn find<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<Client>> {
        Box::pin(async move {
            let Some(row) =
                self.db.table("oauth_clients").filter("client_id", id).first(&self.db).await?
            else {
                return Ok(None);
            };

            let secret_hash = text(&row, "secret_hash");
            // No secret is a public client, which proves itself with PKCE.
            // Defaulting the other way would let anybody who knows an id
            // present themselves as that client.
            let mut client = match secret_hash.is_empty() {
                true => Client::public(text(&row, "client_id")),
                false => Client::from_secret_hash(text(&row, "client_id"), secret_hash),
            };

            client = client.named(text(&row, "name")).scopes(Scopes::parse(&text(&row, "scopes")));
            for uri in text(&row, "redirect_uris").split_whitespace() {
                client = client.redirect_uri(uri);
            }
            if flag(&row, "first_party") {
                client = client.first_party();
            }
            Ok(Some(client))
        })
    }
}

// ---------------------------------------------------------------------------
// Codes
// ---------------------------------------------------------------------------

/// Authorisation codes in `oauth_codes`.
#[derive(Clone)]
pub struct DatabaseCodeStore {
    db: Database,
}

impl DatabaseCodeStore {
    pub fn new(db: Database) -> DatabaseCodeStore {
        DatabaseCodeStore { db }
    }

    fn read(row: &Row) -> AuthorizationCode {
        let mut code = AuthorizationCode::from_hash(text(row, "code_hash"));
        code.client_id = text(row, "client_id");
        code.user_id = text(row, "user_id");
        code.redirect_uri = text(row, "redirect_uri");
        code.scopes = Scopes::parse(&text(row, "scopes"));
        code.challenge = text(row, "challenge");
        code.challenge_method = match text(row, "challenge_method").as_str() {
            "S256" => ChallengeMethod::S256,
            // Anything this server does not recognise is `Plain`, which is the
            // weaker of the two and therefore the one that fails closed: a
            // verifier that would satisfy `Plain` will not satisfy `S256`.
            _ => ChallengeMethod::Plain,
        };
        code.issued_at = number(row, "issued_at");
        code.expires_at = number(row, "expires_at");
        code.family = optional(row, "family");
        code
    }
}

impl CodeStore for DatabaseCodeStore {
    fn store<'a>(&'a self, code: AuthorizationCode) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.db
                .table("oauth_codes")
                .insert_without_id(
                    &self.db,
                    &[
                        ("code_hash", code.hash().into()),
                        ("client_id", code.client_id.as_str().into()),
                        ("user_id", code.user_id.as_str().into()),
                        ("scopes", code.scopes.to_string().into()),
                        ("redirect_uri", code.redirect_uri.as_str().into()),
                        ("challenge", code.challenge.as_str().into()),
                        ("challenge_method", code.challenge_method.as_str().into()),
                        ("issued_at", (code.issued_at as i64).into()),
                        ("expires_at", (code.expires_at as i64).into()),
                    ],
                )
                .await?;
            Ok(())
        })
    }

    fn consume<'a>(
        &'a self,
        hash: &'a str,
        family: &'a str,
        now: u64,
    ) -> StoreFuture<'a, Consumption> {
        Box::pin(async move {
            // **One statement decides.** `consumed_at IS NULL` is the condition,
            // and the row count is the answer: exactly one caller can change a
            // row from unspent to spent, whatever else is in flight. Reading
            // first and writing second would leave a window in which two
            // callers both see it unspent — which is the replay this exists to
            // prevent, and it would pass every single-threaded test.
            let claimed = self
                .db
                .table("oauth_codes")
                .filter("code_hash", hash)
                .filter_null("consumed_at")
                .update(
                    &self.db,
                    &[("consumed_at", (now as i64).into()), ("family", family.into())],
                )
                .await?;

            let Some(row) =
                self.db.table("oauth_codes").filter("code_hash", hash).first(&self.db).await?
            else {
                return Ok(Consumption::Unknown);
            };

            if claimed == 0 {
                // Somebody else spent it — this call or an earlier one. Either
                // way the caller must revoke what that exchange produced.
                return Ok(Consumption::Replayed { family: optional(&row, "family") });
            }

            let code = Self::read(&row);
            // Expiry is checked *after* the claim, on purpose: an expired code
            // is still spent by this call, so a second presentation of it is
            // reported as a replay rather than as expiry. A code that can be
            // presented twice and answer "expired" both times tells an attacker
            // nothing happened.
            if code.expires_at <= now {
                return Ok(Consumption::Expired);
            }
            Ok(Consumption::Fresh(Box::new(code)))
        })
    }

    fn purge<'a>(&'a self, before: u64) -> StoreFuture<'a, usize> {
        Box::pin(async move {
            let removed = self
                .db
                .table("oauth_codes")
                .filter_op("expires_at", "<", before as i64)
                .delete(&self.db)
                .await?;
            Ok(removed as usize)
        })
    }
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

/// Access and refresh tokens, in a table each.
#[derive(Clone)]
pub struct DatabaseTokenStore {
    db: Database,
}

impl DatabaseTokenStore {
    pub fn new(db: Database) -> DatabaseTokenStore {
        DatabaseTokenStore { db }
    }

    fn read_access(row: &Row) -> AccessToken {
        let mut token = AccessToken::from_hash(text(row, "token_id"), text(row, "token_hash"));
        token.client_id = text(row, "client_id");
        token.user_id = optional(row, "user_id");
        token.scopes = Scopes::parse(&text(row, "scopes"));
        token.family = text(row, "family");
        token.issued_at = number(row, "issued_at");
        token.expires_at = number(row, "expires_at");
        token.revoked = flag(row, "revoked");
        token
    }

    fn read_refresh(row: &Row) -> RefreshToken {
        let mut token = RefreshToken::from_hash(text(row, "token_id"), text(row, "token_hash"));
        token.client_id = text(row, "client_id");
        token.user_id = optional(row, "user_id");
        token.scopes = Scopes::parse(&text(row, "scopes"));
        token.family = text(row, "family");
        token.access_token_id = text(row, "access_token_id");
        token.issued_at = number(row, "issued_at");
        token.expires_at = number(row, "expires_at");
        token.revoked = flag(row, "revoked");
        token.rotated = flag(row, "rotated");
        token
    }
}

impl TokenStore for DatabaseTokenStore {
    fn store_access<'a>(&'a self, token: AccessToken) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.db
                .table("oauth_access_tokens")
                .insert_without_id(
                    &self.db,
                    &[
                        ("token_id", token.id.as_str().into()),
                        ("token_hash", token.hash().into()),
                        ("client_id", token.client_id.as_str().into()),
                        (
                            "user_id",
                            token.user_id.as_deref().map_or(Value::Null, Value::from),
                        ),
                        ("scopes", token.scopes.to_string().into()),
                        ("family", token.family.as_str().into()),
                        ("issued_at", (token.issued_at as i64).into()),
                        ("expires_at", (token.expires_at as i64).into()),
                        ("revoked", token.revoked.into()),
                    ],
                )
                .await?;
            Ok(())
        })
    }

    fn store_refresh<'a>(&'a self, token: RefreshToken) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.db
                .table("oauth_refresh_tokens")
                .insert_without_id(
                    &self.db,
                    &[
                        ("token_id", token.id.as_str().into()),
                        ("token_hash", token.hash().into()),
                        ("client_id", token.client_id.as_str().into()),
                        (
                            "user_id",
                            token.user_id.as_deref().map_or(Value::Null, Value::from),
                        ),
                        ("scopes", token.scopes.to_string().into()),
                        ("family", token.family.as_str().into()),
                        ("access_token_id", token.access_token_id.as_str().into()),
                        ("issued_at", (token.issued_at as i64).into()),
                        ("expires_at", (token.expires_at as i64).into()),
                        ("revoked", token.revoked.into()),
                        ("rotated", token.rotated.into()),
                    ],
                )
                .await?;
            Ok(())
        })
    }

    fn find_access<'a>(&'a self, hash: &'a str) -> StoreFuture<'a, Option<AccessToken>> {
        Box::pin(async move {
            let row = self
                .db
                .table("oauth_access_tokens")
                .filter("token_hash", hash)
                .first(&self.db)
                .await?;
            Ok(row.as_ref().map(Self::read_access))
        })
    }

    fn find_refresh<'a>(&'a self, hash: &'a str) -> StoreFuture<'a, Option<RefreshToken>> {
        Box::pin(async move {
            let row = self
                .db
                .table("oauth_refresh_tokens")
                .filter("token_hash", hash)
                .first(&self.db)
                .await?;
            Ok(row.as_ref().map(Self::read_refresh))
        })
    }

    fn rotate<'a>(&'a self, id: &'a str) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            // The second compare-and-set, and the same shape as `consume`.
            // `rotated = false` is the condition; the row count is the answer.
            // Two requests presenting the same live refresh token must not both
            // be told yes — the one that loses is how a stolen token is caught.
            let rotated = self
                .db
                .table("oauth_refresh_tokens")
                .filter("token_id", id)
                .filter("rotated", false)
                .update(&self.db, &[("rotated", true.into())])
                .await?;
            Ok(rotated == 1)
        })
    }

    fn revoke_access<'a>(&'a self, id: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.db
                .table("oauth_access_tokens")
                .filter("token_id", id)
                .update(&self.db, &[("revoked", true.into())])
                .await?;
            Ok(())
        })
    }

    fn revoke_refresh<'a>(&'a self, id: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.db
                .table("oauth_refresh_tokens")
                .filter("token_id", id)
                .update(&self.db, &[("revoked", true.into())])
                .await?;
            Ok(())
        })
    }

    fn revoke_family<'a>(&'a self, family: &'a str) -> StoreFuture<'a, usize> {
        Box::pin(async move {
            // Both tables. Revoking only the refresh tokens would leave the
            // access tokens already minted from this family live for their full
            // hour, which is the window the revocation exists to close.
            let access = self
                .db
                .table("oauth_access_tokens")
                .filter("family", family)
                .filter("revoked", false)
                .update(&self.db, &[("revoked", true.into())])
                .await?;
            let refresh = self
                .db
                .table("oauth_refresh_tokens")
                .filter("family", family)
                .filter("revoked", false)
                .update(&self.db, &[("revoked", true.into())])
                .await?;
            Ok((access + refresh) as usize)
        })
    }

    fn purge<'a>(&'a self, now: u64) -> StoreFuture<'a, usize> {
        Box::pin(async move {
            let access = self
                .db
                .table("oauth_access_tokens")
                .filter_op("expires_at", "<", now as i64)
                .delete(&self.db)
                .await?;
            let refresh = self
                .db
                .table("oauth_refresh_tokens")
                .filter_op("expires_at", "<", now as i64)
                .delete(&self.db)
                .await?;
            Ok((access + refresh) as usize)
        })
    }
}

// ---------------------------------------------------------------------------
// Consent
// ---------------------------------------------------------------------------

/// Recorded consent in `oauth_consent`.
#[derive(Clone)]
pub struct DatabaseConsentStore {
    db: Database,
}

impl DatabaseConsentStore {
    pub fn new(db: Database) -> DatabaseConsentStore {
        DatabaseConsentStore { db }
    }
}

impl ConsentStore for DatabaseConsentStore {
    fn find<'a>(&'a self, client_id: &'a str, user_id: &'a str) -> StoreFuture<'a, Option<Grant>> {
        Box::pin(async move {
            let row = self
                .db
                .table("oauth_consent")
                .filter("client_id", client_id)
                .filter("user_id", user_id)
                .first(&self.db)
                .await?;

            Ok(row.map(|row| Grant {
                client_id: text(&row, "client_id"),
                user_id: text(&row, "user_id"),
                scopes: Scopes::parse(&text(&row, "scopes")),
                granted_at: number(&row, "granted_at"),
            }))
        })
    }

    fn record<'a>(&'a self, grant: Grant) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            // Replacing rather than merging: the scopes on the screen the user
            // just approved are the scopes they agreed to. Accumulating across
            // visits would mean a grant nobody ever saw in full.
            self.db
                .table("oauth_consent")
                .filter("client_id", grant.client_id.as_str())
                .filter("user_id", grant.user_id.as_str())
                .delete(&self.db)
                .await?;

            self.db
                .table("oauth_consent")
                .insert_without_id(
                    &self.db,
                    &[
                        ("client_id", grant.client_id.as_str().into()),
                        ("user_id", grant.user_id.as_str().into()),
                        ("scopes", grant.scopes.to_string().into()),
                        ("granted_at", (grant.granted_at as i64).into()),
                    ],
                )
                .await?;
            Ok(())
        })
    }

    fn forget<'a>(&'a self, client_id: &'a str, user_id: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.db
                .table("oauth_consent")
                .filter("client_id", client_id)
                .filter("user_id", user_id)
                .delete(&self.db)
                .await?;
            Ok(())
        })
    }
}
