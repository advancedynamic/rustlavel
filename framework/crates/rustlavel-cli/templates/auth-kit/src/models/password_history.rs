use rustlavel::prelude::*;

/// A password this person used to have, as an argon2 hash.
///
/// Kept only so a new password can be refused for being an old one. Nothing
/// here is reversible, and the rows go with the account.
#[derive(Model, Default, Debug, Clone)]
#[model(table = "password_history")]
pub struct PasswordHistory {
    #[model(primary_key, generated)]
    pub id: i64,
    pub user_id: i64,
    pub password_hash: String,
}

impl PasswordHistory {
    /// This person's old hashes, newest first.
    pub fn for_user(user_id: i64) -> QueryBuilder {
        PasswordHistory::query().filter("user_id", user_id).latest("id")
    }
}
