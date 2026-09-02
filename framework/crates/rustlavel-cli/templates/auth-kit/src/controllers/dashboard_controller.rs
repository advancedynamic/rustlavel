use rustlavel::prelude::*;

use crate::models::login_attempt::LoginAttempt;
use crate::models::user::User;
use crate::support::{page, tokens};

pub struct DashboardController;

impl DashboardController {
    pub async fn index(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let user_id = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let Some(user) = User::find(&db, user_id).await? else { return Ok(Response::see_other("/login")) };

        let mut context = page::shell(&req, "dashboard").await;
        context = page::with_user(context, &req, &user).await?;

        let mfa = crate::controllers::auth::mfa_controller::has_factor(&db, user.id).await?;
        // The one before this one: the newest success is the sign-in that is
        // being read right now, and telling somebody they last signed in a
        // second ago is useless for spotting an intrusion.
        let history = LoginAttempt::get(&db, LoginAttempt::for_user(user.id).limit(10)).await?;
        let previous = history.iter().filter(|a| a.successful).nth(1);

        context = context
            .with("mfa_enabled", Json::from(mfa))
            .with(
                "last_login_at",
                previous.map_or(Json::Null, |a| {
                    Json::from(tokens::humanise(a.created_at.as_deref().unwrap_or_default()))
                }),
            )
            .with(
                "last_login_ip",
                Json::from(previous.and_then(|a| a.ip.clone()).unwrap_or_else(|| "an unknown address".into())),
            );

        let entries: Vec<Json> = history.iter().take(6).map(attempt_json).collect();
        context = context
            .with("recent_logins_empty", Json::from(entries.is_empty()))
            .with("recent_logins", Json::Array(entries));

        let stats = Self::stats(&req, &db).await?;
        context = context.with("stats", Json::Array(stats));

        req.view("dashboard", &context)
    }

    /// The cards along the top. Only the ones this person may see: a count of
    /// every user is a small leak, but it is still one.
    async fn stats(req: &Request, db: &Database) -> Result<Vec<Json>> {
        let mut stats = Vec::new();

        if req.can("users.view").await? {
            let total = db.table("users").count(db).await?;
            let pending = db.table("users").filter_null("password_hash").count(db).await?;
            stats.push(card("Users", total, (pending > 0).then(|| format!("{pending} not activated"))));

            // The last day, not "today": a count that resets at midnight hides
            // exactly the run of failures somebody wants to see at 00:05.
            let since = tokens::format_utc(tokens::unix_now() - 24 * 60 * 60);
            let failures = db
                .table("login_attempts")
                .filter("successful", false)
                .filter_op("created_at", ">", since)
                .count(db)
                .await?;
            stats.push(card("Failed sign-ins, last day", failures, None));
        }
        if req.can("roles.view").await? {
            stats.push(card("Roles", db.table("roles").count(db).await?, None));
        }
        Ok(stats)
    }
}

fn card(label: &str, value: i64, note: Option<String>) -> Json {
    Json::object([
        ("label", Json::from(label)),
        ("value", Json::from(value)),
        ("note", note.map_or(Json::Null, Json::from)),
    ])
}

pub fn attempt_json(attempt: &LoginAttempt) -> Json {
    Json::object([
        ("at", Json::from(tokens::humanise(attempt.created_at.as_deref().unwrap_or_default()))),
        ("successful", Json::from(attempt.successful)),
        ("ip", Json::from(attempt.ip.clone().unwrap_or_else(|| "unknown".into()))),
        ("agent", Json::from(attempt.user_agent.clone().unwrap_or_default())),
    ])
}
