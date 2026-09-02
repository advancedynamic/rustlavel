use rustlavel::prelude::*;

/// A person who can sign in.
///
/// `password_hash` is nullable because a user invited by an administrator
/// exists before they have chosen one. Nothing may sign in until it is set;
/// [`User::can_sign_in`] is the single place that decides.
#[derive(Model, Default, Debug, Clone)]
#[model(table = "users")]
pub struct User {
    #[model(primary_key, generated)]
    pub id: i64,
    pub name: String,
    pub email: String,
    pub password_hash: Option<String>,
    pub email_verified_at: Option<String>,
    pub locked_until: Option<String>,
    pub failed_attempts: i64,
    pub last_login_at: Option<String>,
    pub last_login_ip: Option<String>,
    pub session_epoch: Option<String>,
    pub is_active: bool,
}

impl User {
    pub fn by_email(email: &str) -> QueryBuilder {
        // Addresses are compared lowercased, because a person who signed up as
        // Alice@ and comes back as alice@ is the same person, and because two
        // rows differing only in case is an account-takeover waiting to happen.
        User::query().filter("email", email.trim().to_lowercase())
    }

    /// Whether this account may sign in at all, and why not when it may not.
    ///
    /// One function rather than three checks at each call site: a login path
    /// that forgets one of these is a login path that lets somebody in.
    pub fn can_sign_in(&self, now: &str) -> Result<(), &'static str> {
        if !self.is_active {
            return Err("inactive");
        }
        if self.password_hash.is_none() {
            return Err("not_activated");
        }
        if self.locked_until.as_deref().is_some_and(|until| until > now) {
            return Err("locked");
        }
        Ok(())
    }

    pub fn is_locked(&self, now: &str) -> bool {
        self.locked_until.as_deref().is_some_and(|until| until > now)
    }

    pub fn initials(&self) -> String {
        self.name
            .split_whitespace()
            .filter_map(|word| word.chars().next())
            .take(2)
            .collect::<String>()
            .to_uppercase()
    }

    pub fn first_name(&self) -> &str {
        self.name.split_whitespace().next().unwrap_or(&self.name)
    }

    /// What a template is allowed to see. Deliberately not `to_json`: the
    /// derived one carries `password_hash`, and a view that renders every
    /// field is one leak away from rendering that.
    pub fn public_json(&self) -> Json {
        Json::object([
            ("id", Json::from(self.id)),
            ("name", Json::from(self.name.as_str())),
            ("email", Json::from(self.email.as_str())),
            ("initials", Json::from(self.initials())),
            ("activated", Json::from(self.password_hash.is_some())),
            ("verified", Json::from(self.email_verified_at.is_some())),
            ("last_login_at", self.last_login_at.clone().map_or(Json::from("Never"), Json::from)),
        ])
    }
}

impl Authenticatable for User {
    fn auth_identifier(&self) -> String {
        self.id.to_string()
    }
}
