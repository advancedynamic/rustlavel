//! Signing in with a link emailed to the address on the account.
//!
//! Off by default, and switched on at Settings → Security → *Enable Magic Link
//! Login*. When it is off the routes answer 404 rather than 403: a page that
//! does not exist here is not a page whose absence is worth explaining.
//!
//! The link is a password in an email, so it is treated as one. It lasts an
//! hour, works once, is stored only as a SHA-256, and issuing a new one spends
//! the old one — that last part is what [`tokens::issue`] does for every
//! purpose. What it does **not** do is skip anything the password form checks:
//! the account still has to be active and unlocked, a second factor is still
//! owed, and a site that requires enrolment still gets it.

use rustlavel::prelude::*;

use crate::models::login_attempt::LoginAttempt;
use crate::models::user::User;
use crate::models::user_token::MAGIC_LINK;
use crate::support::{lockout, page, tokens};

use super::login_controller::{LoginController, PENDING_KEY};

pub struct MagicLinkController;

impl MagicLinkController {
    /// The "email me a link" form.
    pub async fn show(req: Request) -> Result<Response> {
        if !enabled(&req).await {
            return Ok(Response::not_found());
        }
        let context = page::shell(&req, "").await;
        req.view("auth/magic", &page::old(context, &[("email", None)]))
    }

    /// Issue a link and email it.
    pub async fn store(mut req: Request) -> Result<Response> {
        if !enabled(&req).await {
            return Ok(Response::not_found());
        }
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let email = req.input("email").unwrap_or_default().trim().to_lowercase();

        let errors = page::check(&[("email", &email)], &[("email", "required|email|max:190")]);
        if !errors.is_empty() {
            let context = page::errors(page::shell(&req, "").await, &errors);
            return req.view("auth/magic", &page::old(context, &[("email", Some(email))]));
        }

        // The same throttle the password form is behind. Without it this route
        // is an unauthenticated way to make the application send mail to any
        // address somebody names.
        if lockout::address_is_blocked(&req).await {
            return Self::sent(req, &email).await;
        }
        lockout::record_address_failure(&req).await;

        // A link is issued only for an account that could sign in anyway; every
        // other case falls through to the same page, because "we have not heard
        // of that address" is something this form must not say.
        if let Some(user) = User::first(&db, User::by_email(&email)).await?
            && user.can_sign_in(&tokens::now()).is_ok()
        {
            send_link(&req, &db, &user).await?;
        }

        Self::sent(req, &email).await
    }

    /// Spend a link and sign the person in.
    pub async fn consume(req: Request) -> Result<Response> {
        if !enabled(&req).await {
            return Ok(Response::not_found());
        }
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let token = req.param("token").unwrap_or_default().to_string();
        let now = tokens::now();

        let Some(record) = tokens::claim(&db, MAGIC_LINK, &token).await? else {
            return super::register_controller::expired(
                req,
                "That sign-in link has expired or has already been used.",
                "/magic-link",
                "Ask for a new link",
            )
            .await;
        };
        let Some(mut user) = User::find(&db, record.user_id).await? else {
            return super::register_controller::expired(req, "That account no longer exists.", "/login", "Back to sign in").await;
        };

        // Re-checked at the moment of use, not at the moment of issue: an hour
        // is long enough for an account to be deactivated or locked.
        if let Err(reason) = user.can_sign_in(&now) {
            LoginAttempt::record(&db, &user.email.clone(), Some(user.id), false, Some(reason), &req).await?;
            return super::register_controller::expired(
                req,
                "That account cannot sign in at the moment.",
                "/login",
                "Back to sign in",
            )
            .await;
        }

        // A link proves the address, not the second factor. Somebody with an
        // authenticator enrolled still owes it — otherwise this route would be
        // a way around their own two-factor.
        if super::mfa_controller::has_factor(&db, user.id).await? {
            let session = req.session();
            session.regenerate();
            session.put(PENDING_KEY, Json::from(user.id));
            return Ok(Response::see_other("/mfa"));
        }

        LoginController::complete(&req, &db, &mut user, &now).await?;
        if let Some(enrol) = LoginController::enrolment_owed(&req, &db, user.id).await? {
            return Ok(Response::see_other(enrol));
        }
        Ok(Response::see_other("/dashboard"))
    }

    /// The same page whether or not the address has an account.
    async fn sent(req: Request, email: &str) -> Result<Response> {
        let context = page::shell(&req, "").await
            .with("email", Json::from(email))
            .with("expires_in", Json::from("one hour"));
        req.view("auth/sent", &context)
    }
}

async fn send_link(req: &Request, db: &Database, user: &User) -> Result<()> {
    let token = tokens::issue(db, user.id, MAGIC_LINK, None).await?;
    let url = format!("{}/magic/{token}", req.config().string("app.url", "http://localhost:8000"));

    if req.state::<rustlavel::mail::Mailer>().is_none() {
        warn!("no mailer is configured; the sign-in link for {} is {url}", user.email);
        return Ok(());
    }

    crate::support::mail::send(
        req,
        rustlavel::mail::Message::new()
            .to(user.email.as_str())
            .subject("Your sign-in link")
            .text(format!(
                "Hello {},\n\nUse this link to sign in:\n\n{url}\n\nThe link works once and \
                 expires in an hour. If you did not ask for it, you can ignore it — nobody \
                 can sign in as you without it.\n",
                user.first_name()
            )),
    )
    .await
}

/// Whether magic-link sign-in is switched on.
pub async fn enabled(req: &Request) -> bool {
    match req.state::<crate::support::settings::Settings>() {
        Some(settings) => settings.bool("auth.magic_link").await,
        None => req.config().bool("auth.magic_link", false),
    }
}
