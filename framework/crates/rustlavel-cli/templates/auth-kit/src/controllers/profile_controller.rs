use rustlavel::prelude::*;

use crate::models::login_attempt::LoginAttempt;
use crate::models::user::User;
use crate::support::{page, tokens};

pub struct ProfileController;

impl ProfileController {
    pub async fn show(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let user_id = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let Some(user) = User::find(&db, user_id).await? else { return Ok(Response::see_other("/login")) };

        req.view("profile", &Self::context(&req, &db, &user).await?)
    }

    async fn context(req: &Request, db: &Database, user: &User) -> Result<ViewContext> {
        let mut context = page::shell(req, "profile").await;
        context = page::with_user(context, req, user).await?;

        let roles = req.permission_list().await.unwrap_or_default();
        let role_names = match req.state::<rustlavel::rbac::Permissions>() {
            Some(store) => store.roles_for(user.id).await.unwrap_or_default(),
            None => Vec::new(),
        };
        let _ = roles;

        let history = LoginAttempt::get(db, LoginAttempt::for_user(user.id).limit(20)).await?;
        let mfa = crate::controllers::auth::mfa_controller::has_factor(db, user.id).await?;

        Ok(context
            .with("name", Json::from(user.name.as_str()))
            .with("email", Json::from(user.email.as_str()))
            .with("email_unverified", Json::from(user.email_verified_at.is_none()))
            .with("created_at", Json::from("—"))
            .with("mfa_enabled", Json::from(mfa))
            .with("min_length", Json::from(crate::controllers::auth::register_controller::min_length(req)))
            .with("roles_empty", Json::from(role_names.is_empty()))
            .with("roles", Json::Array(role_names.iter().map(|r| Json::from(r.as_str())).collect()))
            .with(
                "logins",
                Json::Array(
                    history.iter().map(crate::controllers::dashboard_controller::attempt_json).collect(),
                ),
            ))
    }

    pub async fn update(mut req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let user_id = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let Some(mut user) = User::find(&db, user_id).await? else { return Ok(Response::see_other("/login")) };

        let name = req.input("name").unwrap_or_default();
        let email = req.input("email").unwrap_or_default().trim().to_lowercase();

        let errors = page::check(
            &[("name", &name), ("email", &email)],
            &[("name", "required|max:120"), ("email", "required|email|max:190")],
        );
        if !errors.is_empty() {
            let context = page::errors(Self::context(&req, &db, &user).await?, &errors);
            return req.view("profile", &context);
        }

        user.name = name.trim().to_string();

        if email != user.email {
            // The address is not changed here. It changes when the link sent to
            // the new one is clicked — otherwise a typo, or somebody at a
            // borrowed keyboard, moves the account to an address nobody owns.
            if User::first(&db, User::by_email(&email)).await?.is_none() {
                let token = tokens::issue(
                    &db,
                    user.id,
                    crate::models::user_token::EMAIL_CHANGE,
                    Some(email.clone()),
                )
                .await?;
                let url = format!(
                    "{}/profile/email/{token}",
                    req.config().string("app.url", "http://localhost:8000")
                );
                if let Some(mailer) = req.state::<rustlavel::mail::Mailer>() {
                    mailer
                        .send(rustlavel::mail::Message::new().to(email.as_str()).subject("Confirm your new email address").text(format!(
                            "Use this link to confirm this address on your account:\n\n{url}\n\n\
                             Until you do, the old address keeps working. The link expires in an hour.\n"
                        )))
                        .await?;
                } else {
                    warn!("no mailer is configured; the email-change link is {url}");
                }
            }
            page::flash(&req, "success", "Saved. Check the new address for a confirmation link.");
        } else {
            page::flash(&req, "success", "Saved.");
        }

        user.update(&db).await?;
        Ok(Response::see_other("/profile"))
    }

    /// Finish an address change from the emailed link.
    pub async fn confirm_email(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let token = req.param("token").unwrap_or_default().to_string();

        let Some(record) = tokens::claim(&db, crate::models::user_token::EMAIL_CHANGE, &token).await? else {
            return crate::controllers::auth::register_controller::expired(
                req,
                "That confirmation link has expired or has already been used.",
                "/profile",
                "Back to your profile",
            ).await;
        };
        let Some(mut user) = User::find(&db, record.user_id).await? else {
            return Ok(Response::see_other("/login"));
        };

        if let Some(address) = record.payload {
            // Checked again here: somebody else may have taken the address in
            // the hour since the link was sent.
            if User::first(&db, User::by_email(&address)).await?.is_none() {
                user.email = address;
                user.email_verified_at = Some(tokens::now());
                user.update(&db).await?;
                page::flash(&req, "success", "Your email address has been updated.");
                return Ok(Response::see_other("/profile"));
            }
        }
        page::flash(&req, "error", "That address is no longer available.");
        Ok(Response::see_other("/profile"))
    }

    pub async fn change_password(mut req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let user_id = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let Some(mut user) = User::find(&db, user_id).await? else { return Ok(Response::see_other("/login")) };

        let current = req.input("current_password").unwrap_or_default();
        let password = req.input("password").unwrap_or_default();
        let confirmation = req.input("password_confirmation").unwrap_or_default();
        let minimum = crate::controllers::auth::register_controller::min_length(&req);

        let mut errors =
            crate::controllers::auth::register_controller::password_errors(&password, &confirmation, minimum);

        // The current password, every time. Without it a borrowed unlocked
        // laptop is a permanent account takeover, which is the whole reason
        // this check exists.
        let ok = user
            .password_hash
            .as_deref()
            .is_some_and(|hash| rustlavel::auth::verify_password(&current, hash));
        if !ok {
            errors.add("current_password", "That is not your current password.");
        }

        if !errors.is_empty() {
            let context = page::errors(Self::context(&req, &db, &user).await?, &errors);
            return req.view("profile", &context);
        }

        user.password_hash = Some(rustlavel::auth::hash_password(&password)?);
        user.session_epoch = Some(rustlavel::auth::random::hex(16));
        user.update(&db).await?;

        // This session survives; the epoch change ends the others.
        req.session().put("_epoch", Json::from(user.session_epoch.clone().unwrap_or_default()));
        page::flash(&req, "success", "Your password has been changed. Other devices have been signed out.");
        Ok(Response::see_other("/profile"))
    }
}
