use rustlavel::prelude::*;

/// One sign-in attempt. Both halves matter: the successes answer "when did I
/// last sign in", and the failures are how somebody notices a password being
/// guessed.
#[derive(Model, Default, Debug, Clone)]
#[model(table = "login_attempts")]
pub struct LoginAttempt {
    #[model(primary_key, generated)]
    pub id: i64,
    pub user_id: Option<i64>,
    pub email: String,
    pub successful: bool,
    pub reason: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: Option<String>,
}

impl LoginAttempt {
    /// Record an attempt.
    ///
    /// Failures are never silently dropped, including the ones against an
    /// address with no account: an attacker working through a list of
    /// addresses looks exactly like that, and nothing else would show it.
    pub async fn record(
        db: &Database,
        email: &str,
        user_id: Option<i64>,
        successful: bool,
        reason: Option<&str>,
        req: &Request,
    ) -> Result<()> {
        let mut attempt = LoginAttempt {
            user_id,
            email: email.trim().to_lowercase(),
            successful,
            reason: reason.map(str::to_string),
            ip: req.ip(),
            // Truncated: this is a label for a person reading a list, not a
            // field anything parses, and an unbounded header does not belong
            // in a column.
            user_agent: req.header("user-agent").map(|agent| agent.chars().take(180).collect()),
            ..Default::default()
        };
        attempt.insert(db).await
    }

    pub fn for_user(user_id: i64) -> QueryBuilder {
        LoginAttempt::query().filter("user_id", user_id).latest("id")
    }

    /// Failures against one account since a moment, for the lockout count.
    pub fn recent_failures(user_id: i64, since: &str) -> QueryBuilder {
        LoginAttempt::query()
            .filter("user_id", user_id)
            .filter("successful", false)
            .filter_op("created_at", ">", since)
    }
}
