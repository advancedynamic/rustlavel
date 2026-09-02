use rustlavel::prelude::*;

/// A single-use secret emailed to a person: an activation link, a password
/// reset, a confirmation of a new address.
///
/// The token itself is never stored. What is stored is its SHA-256, so a
/// leaked database backup is not a folder of working links.
#[derive(Model, Default, Debug, Clone)]
#[model(table = "user_tokens")]
pub struct UserToken {
    #[model(primary_key, generated)]
    pub id: i64,
    pub user_id: i64,
    pub purpose: String,
    pub token_hash: String,
    pub payload: Option<String>,
    pub expires_at: String,
    pub used_at: Option<String>,
}

pub const ACTIVATION: &str = "activation";
pub const PASSWORD_RESET: &str = "password_reset";
pub const EMAIL_CHANGE: &str = "email_change";

impl UserToken {
    /// An unused, unexpired token of this purpose.
    ///
    /// The lookup is by hash, so the plaintext travels only in the email and
    /// in the URL the person clicks.
    pub fn usable(purpose: &str, token: &str, now: &str) -> QueryBuilder {
        UserToken::query()
            .filter("purpose", purpose)
            .filter("token_hash", hash_token(token))
            .filter_null("used_at")
            .filter_op("expires_at", ">", now)
    }
}

/// The hash a token is stored under.
///
/// SHA-256 rather than argon2, and that is the right call here: a token is 32
/// random bytes, so there is no guessing to slow down, and a password-reset
/// check that took a quarter of a second would be a denial-of-service lever
/// anybody could pull.
pub fn hash_token(token: &str) -> String {
    use rustlavel::auth::hashing::sha256_hex;
    sha256_hex(token.as_bytes())
}
