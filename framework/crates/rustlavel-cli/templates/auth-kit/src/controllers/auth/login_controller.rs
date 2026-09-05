//! Signing in, and the several ways it can go wrong.

use rustlavel::prelude::*;

use crate::models::login_attempt::LoginAttempt;
use crate::models::user::User;
use crate::support::{lockout, page, tokens};

/// The session key holding a half-finished login: password accepted, second
/// factor still owed. It is deliberately not the identity key — a session
/// carrying this is not signed in, and nothing downstream should think it is.
pub const PENDING_KEY: &str = "_mfa_pending";

pub struct LoginController;

impl LoginController {
    pub async fn show(req: Request) -> Result<Response> {
        if req.identity().is_some() {
            return Ok(Response::redirect("/dashboard"));
        }
        let context = page::shell(&req, "").await.with(
            "registration_open",
            Json::from(crate::controllers::auth::register_controller::registration_open(&req).await),
        )
        .with(
            "magic_link",
            Json::from(crate::controllers::auth::magic_link_controller::enabled(&req).await),
        );
        req.view("auth/login", &page::old(context, &[("email", None)]))
    }

    pub async fn store(mut req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let email = req.input("email").unwrap_or_default().trim().to_lowercase();
        let password = req.input("password").unwrap_or_default();
        let now = tokens::now();

        // The address limit is checked before anything is looked up, so a
        // password-spraying run is stopped before it can tell one account
        // apart from another.
        if lockout::address_is_blocked(&req).await {
            LoginAttempt::record(&db, &email, None, false, Some("address_blocked"), &req).await?;
            return Self::refuse(req, &email, "Too many failed attempts from this network. Try again later.").await;
        }

        let user = User::first(&db, User::by_email(&email)).await?;

        // The password is verified even when no account matched, against a
        // dummy hash. Skipping it would make a missing account measurably
        // faster to reject than a wrong password, and that difference is how
        // an attacker enumerates who has an account here.
        let stored = user.as_ref().and_then(|u| u.password_hash.clone());
        let matches = match &stored {
            Some(hash) => rustlavel::auth::verify_password(&password, hash),
            None => {
                rustlavel::auth::verify_password(&password, DUMMY_HASH);
                false
            }
        };

        let Some(mut user) = user else {
            LoginAttempt::record(&db, &email, None, false, Some("unknown_email"), &req).await?;
            lockout::record_address_failure(&req).await;
            return Self::refuse(req, &email, WRONG).await;
        };

        if let Err(reason) = user.can_sign_in(&now) {
            LoginAttempt::record(&db, &email, Some(user.id), false, Some(reason), &req).await?;

            // A locked account is told it is locked. Hiding that would leave a
            // person retrying a correct password forever with no idea why.
            if reason == "locked" {
                let until = user.locked_until.clone().unwrap_or_default();
                let context = page::shell(&req, "").await
                    .with("locked", Json::from(true))
                    .with("locked_for", Json::from(lockout::remaining(&until, &now)));
                return req.view("auth/login", &page::old(context, &[("email", Some(email))]));
            }
            let message = match reason {
                "not_activated" => "This account has not been activated yet. Check your email for the invitation.",
                _ => "This account has been deactivated. Ask an administrator.",
            };
            return Self::refuse(req, &email, message).await;
        }

        if !matches {
            LoginAttempt::record(&db, &email, Some(user.id), false, Some("bad_password"), &req).await?;
            lockout::record_address_failure(&req).await;

            if lockout::record_failure(&db, &mut user, &req, &now).await? {
                let until = user.locked_until.clone().unwrap_or_default();
                let context = page::shell(&req, "").await
                    .with("locked", Json::from(true))
                    .with("locked_for", Json::from(lockout::remaining(&until, &now)));
                return req.view("auth/login", &page::old(context, &[("email", Some(email))]));
            }
            return Self::refuse(req, &email, WRONG).await;
        }

        // The password was right. If a second factor is enrolled, the session
        // holds only a pending id — not an identity — until it is satisfied.
        if crate::controllers::auth::mfa_controller::has_factor(&db, user.id).await? {
            let session = req.session();
            session.regenerate();
            session.put(PENDING_KEY, Json::from(user.id));
            return Ok(Response::see_other("/mfa"));
        }

        Self::complete(&req, &db, &mut user, &now).await?;

        if let Some(enrol) = Self::enrolment_owed(&req, &db, user.id).await? {
            return Ok(Response::see_other(enrol));
        }

        Ok(Response::see_other(&intended(&req).await))
    }

    /// Where a newly signed-in person has to go before anywhere else, if
    /// anywhere.
    ///
    /// Settings → Security can require a second factor of everybody. Somebody
    /// with nothing enrolled is sent to enrol rather than to the dashboard —
    /// and signed in first, deliberately: the enrolment page is behind the
    /// auth middleware, so refusing the session would send them to a page they
    /// cannot open. This is a nudge with a flash message rather than a wall;
    /// making it a wall means middleware on every other route, which belongs in
    /// `src/routes/` rather than here.
    ///
    /// Every path that signs somebody in calls this — the login form, an
    /// activation link, a password reset, a magic link — because a requirement
    /// that only the login form enforces is a requirement with four ways
    /// around it.
    pub async fn enrolment_owed(req: &Request, db: &Database, user_id: i64) -> Result<Option<String>> {
        if !mfa_required(req).await {
            return Ok(None);
        }
        if crate::controllers::auth::mfa_controller::has_factor(db, user_id).await? {
            return Ok(None);
        }
        page::flash(
            req,
            "warning",
            "This site requires two-factor authentication. Set up an authenticator app \
             or a passkey to finish securing your account.",
        );
        Ok(Some("/settings/security".to_string()))
    }

    /// Finish a login that has cleared every check.
    pub async fn complete(req: &Request, db: &Database, user: &mut User, now: &str) -> Result<()> {
        Guard::new(req.session().clone()).login(user);
        // The epoch this session was opened under. `support::epoch` compares it
        // on every later request, which is what makes "other devices have been
        // signed out" true — a reset rotates the account's epoch, and every
        // session still carrying the old one is ended on its next request.
        req.session().put(
            crate::support::epoch::EPOCH_KEY,
            Json::from(user.session_epoch.clone().unwrap_or_default()),
        );
        lockout::record_success(db, user, req, now).await?;
        LoginAttempt::record(db, &user.email.clone(), Some(user.id), true, None, req).await?;

        // Every way in lands here — the password form, an activation link, a
        // reset, a magic link — so this is the one place a sign-in has to be
        // recorded. `record` rather than `save`: losing an entry is bad,
        // refusing to sign somebody in because the trail is unavailable is
        // worse.
        // The name goes into the session here, which is the one moment it is
        // already loaded — every later entry reads it back for free.
        crate::support::audit::remember(req, &user.name);

        if let Some(audit) = crate::support::audit::of(req, "logged_in") {
            let address = req.ip().unwrap_or_else(|| "an unknown address".into());
            audit
                .by(user.id, user.name.clone())
                .on("User", user.id)
                .describe(format!("{} logged in from {address}", user.name))
                .record()
                .await;
        }
        Ok(())
    }

    pub async fn destroy(req: Request) -> Result<Response> {
        // Recorded before the session is emptied: afterwards there is no
        // identity left on the request to say who left.
        if let Some(audit) = crate::support::audit::of(&req, "logged_out") {
            let name = req
                .try_session()
                .and_then(|session| session.get_string(crate::support::audit::NAME_KEY))
                .unwrap_or_else(|| "Somebody".into());
            audit.describe(format!("{name} logged out")).record().await;
        }
        Guard::new(req.session().clone()).logout();
        Ok(Response::see_other("/login"))
    }

    /// Re-render the form with a message, keeping the address but never the
    /// password.
    async fn refuse(req: Request, email: &str, message: &str) -> Result<Response> {
        let context = page::shell(&req, "").await
            .with("error_summary", Json::from(message))
            .with(
                "registration_open",
                Json::from(crate::controllers::auth::register_controller::registration_open(&req).await),
            )
            .with(
                "magic_link",
                Json::from(crate::controllers::auth::magic_link_controller::enabled(&req).await),
            );
        req.view("auth/login", &page::old(context, &[("email", Some(email.to_string()))]))
    }
}

/// One message for a wrong password and for an address with no account.
///
/// Saying which would turn the login form into a directory of who has an
/// account here, which matters for any site where membership itself is
/// private.
const WRONG: &str = "Those details do not match an account.";

/// A real argon2 hash of a value nobody knows, so the no-account path costs
/// the same as the wrong-password path.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHR2YWx1ZQ$Zm9yIHRpbWluZyBvbmx5IG5ldmVyIG1hdGNoZXM";

/// Whether every account on this site owes a second factor.
///
/// False when no settings store is registered, because a site that has never
/// been able to turn this on has not turned it on.
async fn mfa_required(req: &Request) -> bool {
    match req.state::<crate::support::settings::Settings>() {
        Some(settings) => settings.bool("auth.require_mfa").await,
        None => false,
    }
}

/// Where to go after signing in: back where they were headed, if anywhere.
async fn intended(req: &Request) -> String {
    let saved = req
        .session()
        .forget("_intended")
        .and_then(|value| value.as_str().map(str::to_string))
        // Only a path on this site. A full URL here would be an open redirect,
        // which is how a phishing link borrows a real domain.
        .filter(|path| path.starts_with('/') && !path.starts_with("//"));

    match saved {
        Some(path) => path,
        // Not `/dashboard` any more: where the application opens is a setting,
        // and signing in used to ignore it — so an administrator could point
        // home at `/reports` and still land on the dashboard every morning.
        None => crate::support::home::path(req).await,
    }
}
