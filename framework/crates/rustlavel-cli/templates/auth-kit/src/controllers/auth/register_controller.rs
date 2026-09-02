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
        if !req.config().bool("auth.registration.open", true) {
            return Ok(Response::not_found());
        }
        let context = page::shell(&req, "").await;
        req.view("auth/register", &page::old(context, &[("name", None), ("email", None)]))
    }

    pub async fn store(mut req: Request) -> Result<Response> {
        if !req.config().bool("auth.registration.open", true) {
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
            send_activation(&req, &db, &user, "Confirm your email").await?;
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

/// Issue an activation link and email it.
pub async fn send_activation(
    req: &Request,
    db: &Database,
    user: &User,
    subject: &str,
) -> Result<()> {
    let token = tokens::issue(db, user.id, ACTIVATION, None).await?;
    let url = format!("{}/activate/{token}", req.config().string("app.url", "http://localhost:8000"));

    let Some(mailer) = req.state::<rustlavel::mail::Mailer>() else {
        // No mailer configured. In development that is normal, and printing the
        // link is far better than a person staring at a page that says an email
        // is on the way when nothing is.
        warn!("no mailer is configured; the activation link for {} is {url}", user.email);
        return Ok(());
    };

    mailer
        .send(
            rustlavel::mail::Message::new().to(user.email.as_str())
                .subject(subject)
                .text(format!(
                    "Hello {},\n\nUse this link to set your password and finish setting up your \
                     account:\n\n{url}\n\nThe link works once and expires in an hour.\n\nIf you \
                     were not expecting this, you can ignore it.\n",
                    user.first_name()
                )),
        )
        .await
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
            .with("min_length", Json::from(min_length(&req)));
        req.view("auth/activate", &context)
    }

    pub async fn store(mut req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let token = req.input("token").unwrap_or_default();
        let password = req.input("password").unwrap_or_default();
        let confirmation = req.input("password_confirmation").unwrap_or_default();
        let minimum = min_length(&req);

        let errors = password_errors(&password, &confirmation, minimum);
        if !errors.is_empty() {
            let context = page::errors(page::shell(&req, "").await, &errors)
                .with("token", Json::from(token))
                .with("action", Json::from("/activate"))
                .with("submit_label", Json::from("Set password and sign in"))
                .with("min_length", Json::from(minimum))
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
        user.password_hash = Some(rustlavel::auth::hash_password(&password)?);
        user.email_verified_at = Some(now.clone());
        user.update(&db).await?;

        // Signed in straight away: they have just proved they hold the address
        // and chosen a password, which is everything the login form asks for.
        crate::controllers::auth::login_controller::LoginController::complete(&req, &db, &mut user, &now).await?;
        page::flash(&req, "success", "Welcome. Your account is ready.");
        Ok(Response::see_other("/dashboard"))
    }
}

pub fn min_length(req: &Request) -> i64 {
    req.config().int("auth.password.min_length", 12).clamp(8, 128)
}

/// The rules a new password has to pass.
///
/// Length only, and that is on purpose. Composition rules — an uppercase, a
/// digit, a symbol — push people towards `Password1!` and away from the long
/// passphrase that is actually harder to guess. NIST dropped them in SP
/// 800-63B for the same reason.
pub fn password_errors(password: &str, confirmation: &str, minimum: i64) -> Errors {
    let mut errors = crate::support::page::check(
        &[("password", password)],
        &[("password", &format!("required|min:{minimum}|max:200"))],
    );
    if !password.is_empty() && password != confirmation {
        errors.add("password_confirmation", "The two passwords do not match.");
    }
    errors
}

pub async fn expired(req: Request, reason: &str, retry_url: &str, retry_label: &str) -> Result<Response> {
    let context = page::shell(&req, "").await
        .with("reason", Json::from(reason))
        .with("retry_url", Json::from(retry_url))
        .with("retry_label", Json::from(retry_label));
    req.view("auth/expired", &context)
}
