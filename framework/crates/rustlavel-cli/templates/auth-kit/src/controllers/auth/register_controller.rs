//! Creating an account, confirming the address, and choosing a first password.
//!
//! Two ways in, one destination. A visitor may register themselves when
//! `auth.registration.open` allows it, and an administrator may create the
//! account instead. Either way the person receives a link, and it is on the
//! far side of that link that they choose a password — so a password is never
//! typed into a form belonging to an unconfirmed address, and an administrator
//! never knows anybody's password.

use rustlavel::prelude::*;
use rustlavel::validation::Errors;

use crate::models::user::User;
use crate::models::user_token::ACTIVATION;
use crate::support::{page, tokens};

pub struct RegisterController;

impl RegisterController {
    pub async fn show(req: Request) -> Result<Response> {
        if !registration_open(&req).await {
            return Ok(Response::not_found());
        }
        let context = page::shell(&req, "").await;
        req.view("auth/register", &page::old(context, &[("name", None), ("email", None)]))
    }

    pub async fn store(mut req: Request) -> Result<Response> {
        if !registration_open(&req).await {
            return Ok(Response::not_found());
        }
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();

        let name = req.input("name").unwrap_or_default();
        let email = req.input("email").unwrap_or_default().trim().to_lowercase();

        let errors = page::check(
            &[("name", &name), ("email", &email)],
            &[("name", "required|max:120"), ("email", "required|email|max:190")],
        );
        if !errors.is_empty() {
            let context = page::errors(page::shell(&req, "").await, &errors);
            return req.view(
                "auth/register",
                &page::old(context, &[("name", Some(name)), ("email", Some(email))]),
            );
        }

        // An address that already has an account gets the same page as one that
        // does not. Saying "that email is taken" turns this form into a way to
        // ask whether somebody has an account here.
        if User::first(&db, User::by_email(&email)).await?.is_none() {
            let mut user = User {
                name: name.trim().to_string(),
                email: email.clone(),
                is_active: true,
                ..Default::default()
            };
            user.insert(&db).await?;
            let token = send_activation(&req, &db, &user, "Confirm your email").await?;

            // With verification switched off the person is sent straight to
            // the far side of their own link instead of being told to go and
            // find it in their mail. The link is still issued and still
            // emailed, so nothing about the account differs — what changes is
            // whether reading the address is a condition of getting in.
            if !verify_email(&req).await {
                return Ok(Response::see_other(format!("/activate/{token}")));
            }
        }

        Self::sent(req, &email).await
    }

    async fn sent(req: Request, email: &str) -> Result<Response> {
        let context = page::shell(&req, "").await
            .with("email", Json::from(email))
            .with("expires_in", Json::from("one hour"));
        req.view("auth/sent", &context)
    }
}

/// Issue an activation link and email it. Returns the token, for the one
/// caller that also needs to put the person on the far side of the link
/// themselves — see [`verify_email`].
pub async fn send_activation(
    req: &Request,
    db: &Database,
    user: &User,
    subject: &str,
) -> Result<String> {
    let token = tokens::issue(db, user.id, ACTIVATION, None).await?;
    let url = format!("{}/activate/{token}", req.config().string("app.url", "http://localhost:8000"));

    // No mailer configured is normal in development, and printing the link is
    // far better than a person staring at a page that says an email is on the
    // way when nothing is.
    if req.state::<rustlavel::mail::Mailer>().is_none() {
        warn!("no mailer is configured; the activation link for {} is {url}", user.email);
        return Ok(token);
    }

    crate::support::mail::send(
        req,
        rustlavel::mail::Message::new()
            .to(user.email.as_str())
            .subject(subject)
            .text(format!(
                "Hello {},\n\nUse this link to set your password and finish setting up your \
                 account:\n\n{url}\n\nThe link works once and expires in an hour.\n\nIf you \
                 were not expecting this, you can ignore it.\n",
                user.first_name()
            )),
    )
    .await?;
    Ok(token)
}

pub struct ActivationController;

impl ActivationController {
    /// The set-a-password form behind an activation link.
    pub async fn show(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let token = req.param("token").unwrap_or_default().to_string();
        let now = tokens::now();

        // Peeked at rather than spent: the link is consumed when the password
        // is submitted, so a mail client that prefetches links does not burn it.
        let Some(record) = crate::models::user_token::UserToken::first(
            &db,
            crate::models::user_token::UserToken::usable(ACTIVATION, &token, &now),
        )
        .await?
        else {
            return expired(req, "That activation link has expired or has already been used.", "/login", "Back to sign in").await;
        };

        let Some(user) = User::find(&db, record.user_id).await? else {
            return expired(req, "That account no longer exists.", "/login", "Back to sign in").await;
        };

        let context = page::shell(&req, "").await
            .with("name", Json::from(user.first_name()))
            .with("token", Json::from(token))
            .with("action", Json::from("/activate"))
            .with("submit_label", Json::from("Set password and sign in"))
            .with("min_length", Json::from(Policy::current(&req).await.minimum));
        req.view("auth/activate", &context)
    }

    pub async fn store(mut req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let token = req.input("token").unwrap_or_default();
        let password = req.input("password").unwrap_or_default();
        let confirmation = req.input("password_confirmation").unwrap_or_default();
        let policy = Policy::current(&req).await;

        let mut errors = policy.errors(&password, &confirmation);

        // Checked before the link is spent: a refusal here must not cost the
        // person the only link they have.
        let keep = crate::support::passwords::keep(&req).await;
        if errors.is_empty() && keep > 0
            && let Some(record) = crate::models::user_token::UserToken::first(
                &db,
                crate::models::user_token::UserToken::usable(ACTIVATION, &token, &tokens::now()),
            )
            .await?
            && crate::support::passwords::was_used_before(&db, record.user_id, &password, keep).await?
        {
            errors.add("password", crate::support::passwords::reuse_message(keep));
        }

        if !errors.is_empty() {
            let context = page::errors(page::shell(&req, "").await, &errors)
                .with("token", Json::from(token))
                .with("action", Json::from("/activate"))
                .with("submit_label", Json::from("Set password and sign in"))
                .with("min_length", Json::from(policy.minimum))
                .with("name", Json::from("there"));
            return req.view("auth/activate", &context);
        }

        let Some(record) = tokens::claim(&db, ACTIVATION, &token).await? else {
            return expired(req, "That activation link has expired or has already been used.", "/login", "Back to sign in").await;
        };
        let Some(mut user) = User::find(&db, record.user_id).await? else {
            return expired(req, "That account no longer exists.", "/login", "Back to sign in").await;
        };

        let now = tokens::now();
        let hash = rustlavel::auth::hash_password(&password)?;
        crate::support::passwords::remember_previous(&db, user.id, user.password_hash.as_deref(), keep).await?;
        user.password_hash = Some(hash);
        user.email_verified_at = Some(now.clone());
        user.update(&db).await?;

        // Signed in straight away: they have just proved they hold the address
        // and chosen a password, which is everything the login form asks for.
        use crate::controllers::auth::login_controller::LoginController;
        LoginController::complete(&req, &db, &mut user, &now).await?;
        if let Some(enrol) = LoginController::enrolment_owed(&req, &db, user.id).await? {
            return Ok(Response::see_other(enrol));
        }
        page::flash(&req, "success", "Welcome. Your account is ready.");
        Ok(Response::see_other("/dashboard"))
    }
}

/// The minimum length from configuration alone.
///
/// [`Policy::current`] is the real answer, because the Settings page is where
/// an administrator changes this. What is left here is the fallback for a
/// request with no `Settings` in state, and the value the two callers outside
/// this file still use.
/// Whether anybody may create an account.
///
/// From the settings store, so the toggle on the Security tab does something —
/// falling back to configuration when there is no store, which is the case in a
/// test that never registered one. `Settings` still lets `.env` win, so a
/// deployment that closed registration deliberately cannot be reopened by a
/// click.
pub async fn registration_open(req: &Request) -> bool {
    match req.state::<crate::support::settings::Settings>() {
        Some(settings) => settings.bool("auth.registration.open").await,
        None => req.config().bool("auth.registration.open", true),
    }
}

/// Whether a new account has to read its own address before it can be used.
///
/// Off means the register form hands the person their activation link directly.
/// It stays on by default: an unverified address is one nobody has proved they
/// can read, which makes password reset a way in for whoever typed it.
pub async fn verify_email(req: &Request) -> bool {
    match req.state::<crate::support::settings::Settings>() {
        Some(settings) => settings.bool("auth.verify_email").await,
        None => req.config().bool("auth.verify_email", true),
    }
}

/// The shortest password this request will accept.
///
/// **Settings first, and that is the whole point of it.** This used to read
/// only `Config`, while `Policy::current` — the code that actually refuses a
/// password — read the settings store. Raising the minimum on the Security tab
/// therefore left every form still advertising the old number, and a person
/// typing twelve characters into a field that asked for twelve was told no.
pub async fn min_length(req: &Request) -> i64 {
    match req.state::<crate::support::settings::Settings>() {
        Some(settings) => settings.int("auth.password.min_length", 12).await,
        None => req.config().int("auth.password.min_length", 12),
    }
    .clamp(8, 128)
}

/// The rules a new password has to pass, as Settings → Security has them.
///
/// **The complexity flags are all off by default, and that is a decision
/// rather than an oversight.** Composition rules — an upper-case letter, a
/// digit, a symbol — push people towards `Password1!` and away from the long
/// passphrase that is actually harder to guess, which is why NIST dropped them
/// in SP 800-63B. They are here because organisations are required to have
/// them, not because they help; length is the rule that does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub minimum: i64,
    pub uppercase: bool,
    pub lowercase: bool,
    pub number: bool,
    pub symbol: bool,
}

impl Policy {
    /// Length only — the shape every one of these rules had before the
    /// Settings page existed, and what a request with no store falls back to.
    pub fn length_only(minimum: i64) -> Policy {
        Policy {
            minimum: minimum.clamp(8, 128),
            uppercase: false,
            lowercase: false,
            number: false,
            symbol: false,
        }
    }

    /// What this request should enforce.
    pub async fn current(req: &Request) -> Policy {
        let Some(settings) = req.state::<crate::support::settings::Settings>() else {
            return Policy::length_only(min_length(req).await);
        };

        Policy {
            // Through `min_length`, not a second read of the same key: the
            // number a form advertises and the number this refuses on has to
            // be one number, and it was not.
            minimum: min_length(req).await,
            uppercase: settings.bool("auth.password.uppercase").await,
            lowercase: settings.bool("auth.password.lowercase").await,
            number: settings.bool("auth.password.number").await,
            symbol: settings.bool("auth.password.symbol").await,
        }
    }

    /// Everything wrong with this password, in the shape the forms render.
    pub fn errors(&self, password: &str, confirmation: &str) -> Errors {
        let mut errors = crate::support::page::check(
            &[("password", password)],
            &[("password", &format!("required|min:{}|max:200", self.minimum))],
        );

        // An empty password is already "required"; piling four more messages on
        // top of that tells a person nothing they did not know.
        if password.is_empty() {
            return errors;
        }

        // Named rather than counted. "Must contain 3 of 4 character classes" is
        // a puzzle; "needs a number" is an instruction.
        let mut missing = Vec::new();
        if self.uppercase && !password.chars().any(char::is_uppercase) {
            missing.push("an upper-case letter");
        }
        if self.lowercase && !password.chars().any(char::is_lowercase) {
            missing.push("a lower-case letter");
        }
        if self.number && !password.chars().any(|c| c.is_ascii_digit()) {
            missing.push("a number");
        }
        // Anything that is not a letter or a digit, so `£` and `–` count. A
        // fixed list of punctuation would refuse a password a non-English
        // keyboard produces without ever saying why.
        if self.symbol && !password.chars().any(|c| !c.is_alphanumeric()) {
            missing.push("a special character");
        }
        if !missing.is_empty() {
            errors.add("password", format!("The password needs {}.", and_list(&missing)));
        }

        if password != confirmation {
            errors.add("password_confirmation", "The two passwords do not match.");
        }
        errors
    }
}

/// `a`, `a and b`, `a, b and c`.
fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The rules a new password has to pass, by length alone.
///
/// Kept for the callers that have no `Settings` to hand; anything that can
/// reach the request should use [`Policy::current`] and get the complexity
/// rules with it.
pub fn password_errors(password: &str, confirmation: &str, minimum: i64) -> Errors {
    Policy::length_only(minimum).errors(password, confirmation)
}

pub async fn expired(req: Request, reason: &str, retry_url: &str, retry_label: &str) -> Result<Response> {
    let context = page::shell(&req, "").await
        .with("reason", Json::from(reason))
        .with("retry_url", Json::from(retry_url))
        .with("retry_label", Json::from(retry_label));
    req.view("auth/expired", &context)
}

#[cfg(test)]
mod tests {
    use super::{Policy, and_list, password_errors};

    /// Every complexity rule on, so one test can turn them off one at a time.
    fn strict() -> Policy {
        Policy {
            minimum: 12,
            uppercase: true,
            lowercase: true,
            number: true,
            symbol: true,
        }
    }

    fn message(policy: &Policy, password: &str) -> String {
        policy
            .errors(password, password)
            .all()
            .values()
            .flatten()
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn length_alone_accepts_a_passphrase() {
        let policy = Policy::length_only(12);
        assert!(policy.errors("correct horse battery staple", "correct horse battery staple").is_empty());

        // And the one composition rules push people towards is no better here.
        assert!(policy.errors("Password1234!", "Password1234!").is_empty());
    }

    #[test]
    fn the_minimum_comes_from_the_setting_rather_than_a_constant() {
        // Eleven characters: accepted at 8 and at 10, refused at 12 and 16.
        let password = "abcdefghijk";
        assert_eq!(password.len(), 11);

        for minimum in [8, 10] {
            assert!(
                Policy::length_only(minimum).errors(password, password).is_empty(),
                "a {minimum}-character minimum should have accepted eleven characters"
            );
        }
        for minimum in [12, 16, 20] {
            assert!(
                !Policy::length_only(minimum).errors(password, password).is_empty(),
                "a {minimum}-character minimum should have refused eleven characters"
            );
        }

        // A setting below the floor cannot weaken the kit, and one above the
        // ceiling cannot lock everybody out.
        assert_eq!(Policy::length_only(2).minimum, 8);
        assert_eq!(Policy::length_only(9_000).minimum, 128);
    }

    #[test]
    fn each_complexity_rule_refuses_on_its_own() {
        // One rule at a time, against a password that fails only that one.
        let cases = [
            (Policy { uppercase: true, ..Policy::length_only(12) }, "lower case only", "an upper-case letter"),
            (Policy { lowercase: true, ..Policy::length_only(12) }, "UPPER CASE ONLY", "a lower-case letter"),
            (Policy { number: true, ..Policy::length_only(12) }, "no digits here", "a number"),
            (Policy { symbol: true, ..Policy::length_only(12) }, "onlyletters123", "a special character"),
        ];

        for (policy, password, wanted) in cases {
            let complaint = message(&policy, password);
            assert!(
                complaint.contains(wanted),
                "{password:?} should have been refused for lacking {wanted}, got {complaint:?}"
            );

            // The same password passes once the rule is off, which is what
            // makes the failure the rule's doing and not the length's.
            assert!(Policy::length_only(12).errors(password, password).is_empty());
        }
    }

    #[test]
    fn every_rule_at_once_names_everything_missing() {
        let complaint = message(&strict(), "aaaaaaaaaaaaaa");
        assert!(complaint.contains("an upper-case letter"), "{complaint}");
        assert!(complaint.contains("a number"), "{complaint}");
        assert!(complaint.contains("a special character"), "{complaint}");
        // Lower case it has, so lower case is not named.
        assert!(!complaint.contains("a lower-case letter"), "{complaint}");

        // And a password that satisfies all four is accepted.
        assert!(strict().errors("Tr0ubador&horse", "Tr0ubador&horse").is_empty());
    }

    #[test]
    fn a_symbol_is_anything_that_is_not_a_letter_or_a_digit() {
        let policy = Policy { symbol: true, ..Policy::length_only(12) };
        for password in ["hyphenated-word", "spaces are fine", "pound £sterling", "emoji 🔐 counts"] {
            assert!(
                policy.errors(password, password).is_empty(),
                "{password:?} contains a non-alphanumeric character and should have passed"
            );
        }
    }

    #[test]
    fn an_empty_password_is_told_one_thing() {
        // Not five. "Required" plus four composition complaints is noise.
        let errors = strict().errors("", "");
        assert_eq!(errors.all().len(), 1);
    }

    #[test]
    fn a_mismatched_confirmation_is_reported_against_its_own_field() {
        let errors = Policy::length_only(12).errors("correct horse battery", "correct horse batter");
        assert!(!errors.is_empty());
        assert!(errors.all().contains_key("password_confirmation"));
    }

    #[test]
    fn the_length_only_wrapper_still_behaves_as_it_did() {
        // The two callers outside this file go through it.
        assert!(password_errors("correct horse battery staple", "correct horse battery staple", 12).is_empty());
        assert!(!password_errors("short", "short", 12).is_empty());
        assert!(!password_errors("correct horse battery", "typo horse battery", 12).is_empty());
    }

    #[test]
    fn a_list_of_missing_things_reads_as_a_sentence() {
        assert_eq!(and_list(&["a number"]), "a number");
        assert_eq!(and_list(&["a number", "a symbol"]), "a number and a symbol");
        assert_eq!(
            and_list(&["an upper-case letter", "a number", "a symbol"]),
            "an upper-case letter, a number and a symbol"
        );
    }
}
