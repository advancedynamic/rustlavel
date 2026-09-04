//! Forgetting a password, and setting a new one.
//!
//! The whole flow rests on one idea: a person who can read the address on the
//! account may take control of it. That is why the link is single-use, short
//! lived, and stored only as a hash — and why using one signs every other
//! session out, since a reset is what somebody does when they think an account
//! has been taken.

use rustlavel::prelude::*;

use crate::models::user::User;
use crate::models::user_token::PASSWORD_RESET;
use crate::support::{page, tokens};

pub struct PasswordController;

impl PasswordController {
    pub async fn forgot(req: Request) -> Result<Response> {
        let context = page::shell(&req, "").await;
        req.view("auth/forgot", &page::old(context, &[("email", None)]))
    }

    pub async fn send(mut req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let email = req.input("email").unwrap_or_default().trim().to_lowercase();

        let errors = page::check(&[("email", &email)], &[("email", "required|email")]);
        if !errors.is_empty() {
            let context = page::errors(page::shell(&req, "").await, &errors);
            return req.view("auth/forgot", &page::old(context, &[("email", Some(email))]));
        }

        // An unknown address gets the same page as a known one, and takes the
        // same route through the code. Anything else makes this form a way to
        // ask who has an account here.
        if let Some(user) = User::first(&db, User::by_email(&email)).await? {
            let token = tokens::issue(&db, user.id, PASSWORD_RESET, None).await?;
            let url = format!(
                "{}/reset-password/{token}",
                req.config().string("app.url", "http://localhost:8000")
            );

            match req.state::<rustlavel::mail::Mailer>().is_some() {
                true => {
                    crate::support::mail::send(
                        &req,
                        rustlavel::mail::Message::new().to(user.email.as_str()).subject("Reset your password").text(format!(
                            "Hello {},\n\nSomebody asked to reset the password on your \
                             account. Use this link:\n\n{url}\n\nIt works once and expires in an \
                             hour. If it was not you, nothing has changed and you can ignore \
                             this.\n",
                            user.first_name()
                        )),
                    )
                    .await?;
                }
                false => warn!("no mailer is configured; the reset link for {email} is {url}"),
            }
        }

        let context = page::shell(&req, "").await
            .with("email", Json::from(email.as_str()))
            .with("expires_in", Json::from("one hour"));
        req.view("auth/sent", &context)
    }

    pub async fn reset_form(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let token = req.param("token").unwrap_or_default().to_string();
        let now = tokens::now();

        let Some(record) = crate::models::user_token::UserToken::first(
            &db,
            crate::models::user_token::UserToken::usable(PASSWORD_RESET, &token, &now),
        )
        .await?
        else {
            return crate::controllers::auth::register_controller::expired(
                req,
                "That reset link has expired or has already been used.",
                "/forgot-password",
                "Ask for a new link",
            ).await;
        };

        let name = User::find(&db, record.user_id)
            .await?
            .map(|user| user.first_name().to_string())
            .unwrap_or_else(|| "there".into());

        let context = page::shell(&req, "").await
            .with("name", Json::from(name))
            .with("token", Json::from(token))
            .with("action", Json::from("/reset-password"))
            .with("submit_label", Json::from("Set the new password"))
            .with(
                "min_length",
                Json::from(crate::controllers::auth::register_controller::Policy::current(&req).await.minimum),
            );
        req.view("auth/activate", &context)
    }

    pub async fn reset(mut req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let token = req.input("token").unwrap_or_default();
        let password = req.input("password").unwrap_or_default();
        let confirmation = req.input("password_confirmation").unwrap_or_default();
        let policy = crate::controllers::auth::register_controller::Policy::current(&req).await;
        let minimum = policy.minimum;
        let mut errors = policy.errors(&password, &confirmation);

        // Reuse is checked against the token's owner *before* the token is
        // spent, so a refusal leaves the person their link instead of sending
        // them back to ask for another one.
        let keep = crate::support::passwords::keep(&req).await;
        if errors.is_empty() && keep > 0
            && let Some(record) = crate::models::user_token::UserToken::first(
                &db,
                crate::models::user_token::UserToken::usable(PASSWORD_RESET, &token, &tokens::now()),
            )
            .await?
            && crate::support::passwords::was_used_before(&db, record.user_id, &password, keep).await?
        {
            errors.add("password", crate::support::passwords::reuse_message(keep));
        }

        if !errors.is_empty() {
            let context = page::errors(page::shell(&req, "").await, &errors)
                .with("token", Json::from(token))
                .with("action", Json::from("/reset-password"))
                .with("submit_label", Json::from("Set the new password"))
                .with("min_length", Json::from(minimum))
                .with("name", Json::from("there"));
            return req.view("auth/activate", &context);
        }

        let Some(record) = tokens::claim(&db, PASSWORD_RESET, &token).await? else {
            return crate::controllers::auth::register_controller::expired(
                req,
                "That reset link has expired or has already been used.",
                "/forgot-password",
                "Ask for a new link",
            ).await;
        };
        let Some(mut user) = User::find(&db, record.user_id).await? else {
            return crate::controllers::auth::register_controller::expired(
                req,
                "That account no longer exists.",
                "/login",
                "Back to sign in",
            ).await;
        };

        let now = tokens::now();
        let hash = rustlavel::auth::hash_password(&password)?;
        crate::support::passwords::remember_previous(&db, user.id, user.password_hash.as_deref(), keep).await?;
        user.password_hash = Some(hash);
        // A reset is what somebody does when they think their account has been
        // taken, so every other session goes with it. The epoch is checked by
        // the session guard on each request.
        user.session_epoch = Some(rustlavel::auth::random::hex(16));
        // The reset itself proves they hold the address.
        user.email_verified_at.get_or_insert_with(|| now.clone());
        // And it clears a lockout: waiting out a lock is not the only way back
        // in when you can prove you own the mailbox.
        user.failed_attempts = 0;
        user.locked_until = None;
        user.update(&db).await?;

        use crate::controllers::auth::login_controller::LoginController;
        LoginController::complete(&req, &db, &mut user, &now).await?;
        if let Some(enrol) = LoginController::enrolment_owed(&req, &db, user.id).await? {
            return Ok(Response::see_other(enrol));
        }
        page::flash(&req, "success", "Your password has been changed. Other devices have been signed out.");
        Ok(Response::see_other("/dashboard"))
    }
}
