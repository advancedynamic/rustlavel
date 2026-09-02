//! Second factors: authenticator apps, passkeys, and recovery codes.
//!
//! The shape of the login is: the password gets you a *pending* session, and
//! only satisfying a factor turns that into an identity. Nothing downstream
//! ever sees a half-finished login, because a half-finished login is not an
//! identity at all.

use rustlavel::prelude::*;
use rustlavel::auth::totp::{RecoveryCodes, Totp, consume_recovery_code, step_of};

use crate::models::user::User;
use crate::models::login_attempt::LoginAttempt;
use crate::support::{page, passkeys, tokens};

use super::login_controller::{LoginController, PENDING_KEY};

pub struct MfaController;

/// Whether this user has any second factor enrolled.
pub async fn has_factor(db: &Database, user_id: i64) -> Result<bool> {
    let totp = db
        .table("user_totp")
        .filter("user_id", user_id)
        .filter_not_null("confirmed_at")
        .count(db)
        .await?;
    if totp > 0 {
        return Ok(true);
    }
    Ok(db.table("user_passkeys").filter("user_id", user_id).count(db).await? > 0)
}

/// The user id waiting on a second factor, if this session holds one.
fn pending(req: &Request) -> Option<i64> {
    req.session().get(PENDING_KEY).and_then(|value| value.as_i64())
}

/// The confirmed authenticator secret for a user, decrypted.
async fn confirmed_totp(req: &Request, db: &Database, user_id: i64) -> Result<Option<(Totp, i64, Option<i64>)>> {
    let rows = db
        .table("user_totp")
        .filter("user_id", user_id)
        .filter_not_null("confirmed_at")
        .get(db)
        .await?;
    let Some(row) = rows.first() else { return Ok(None) };

    let encrypter = rustlavel::auth::Encrypter::from_config(req.config())?;
    let secret = encrypter.decrypt(&row.get::<String>("secret_encrypted")?)?;
    let last_step = row.get::<i64>("last_step").ok();
    Ok(Some((Totp::from_base32(&secret)?, row.get::<i64>("id")?, last_step)))
}

impl MfaController {
    /// The "one more step" page.
    pub async fn challenge(req: Request) -> Result<Response> {
        let Some(user_id) = pending(&req) else { return Ok(Response::see_other("/login")) };
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();

        let has_totp = confirmed_totp(&req, &db, user_id).await?.is_some();
        let has_passkey = passkeys::DbPasskeys::new(db).count_for(user_id).await? > 0;

        let context = page::shell(&req, "").await
            .with("has_totp", Json::from(has_totp))
            .with("has_passkey", Json::from(has_passkey));
        req.view("auth/challenge", &context)
    }

    /// A code from an authenticator app.
    pub async fn verify(mut req: Request) -> Result<Response> {
        let Some(user_id) = pending(&req) else { return Ok(Response::see_other("/login")) };
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let code = req.input("code").unwrap_or_default();

        let Some((totp, row_id, last_step)) = confirmed_totp(&req, &db, user_id).await? else {
            return Ok(Response::see_other("/login"));
        };

        let unix = tokens::unix_now() as u64;
        if !totp.verify(&code, unix) {
            return Self::refuse(req, &db, user_id, "That code is not right. Codes change every 30 seconds.").await;
        }

        // A code stays valid for its whole step, so without this the same six
        // digits — shoulder-surfed, or read off a phishing page — work again
        // for the rest of the window.
        let step = step_of(unix, totp.period()) as i64;
        if last_step.is_some_and(|last| last >= step) {
            return Self::refuse(req, &db, user_id, "That code has already been used. Wait for the next one.").await;
        }
        db.table("user_totp").filter("id", row_id).update(&db, &[("last_step", step.into())]).await?;

        Self::admit(req, &db, user_id).await
    }

    /// The recovery-code form.
    pub async fn recovery_form(req: Request) -> Result<Response> {
        if pending(&req).is_none() {
            return Ok(Response::see_other("/login"));
        }
        let context = page::shell(&req, "").await;
        req.view("auth/recovery", &context)
    }

    pub async fn recovery(mut req: Request) -> Result<Response> {
        let Some(user_id) = pending(&req) else { return Ok(Response::see_other("/login")) };
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let code = req.input("code").unwrap_or_default();

        let rows = db
            .table("user_recovery_codes")
            .filter("user_id", user_id)
            .filter_null("used_at")
            .get(&db)
            .await?;
        let mut hashes: Vec<String> =
            rows.iter().filter_map(|row| row.get::<String>("code_hash").ok()).collect();
        let before = hashes.len();

        if !consume_recovery_code(&code, &mut hashes) {
            return Self::refuse(req, &db, user_id, "That recovery code is not valid.").await;
        }

        // Mark the one that went missing from the list as spent.
        if let Some(spent) = rows.iter().find(|row| {
            row.get::<String>("code_hash").is_ok_and(|hash| !hashes.contains(&hash))
        }) {
            db.table("user_recovery_codes")
                .filter("id", spent.get::<i64>("id")?)
                .update(&db, &[("used_at", tokens::now().into())])
                .await?;
        }

        let response = Self::admit(req, &db, user_id).await?;
        warn!(
            "user {user_id} signed in with a recovery code; {} of {before} remain",
            hashes.len()
        );
        Ok(response)
    }

    /// Turn a pending login into a real one.
    async fn admit(req: Request, db: &Database, user_id: i64) -> Result<Response> {
        let Some(mut user) = User::find(db, user_id).await? else {
            return Ok(Response::see_other("/login"));
        };
        req.session().forget(PENDING_KEY);
        LoginController::complete(&req, db, &mut user, &tokens::now()).await?;
        Ok(Response::see_other("/dashboard"))
    }

    async fn refuse(req: Request, db: &Database, user_id: i64, message: &str) -> Result<Response> {
        let email = User::find(db, user_id).await?.map(|u| u.email).unwrap_or_default();
        LoginAttempt::record(db, &email, Some(user_id), false, Some("mfa_failed"), &req).await?;

        let has_totp = confirmed_totp(&req, db, user_id).await?.is_some();
        let has_passkey = passkeys::DbPasskeys::new(db.clone()).count_for(user_id).await? > 0;
        let context = page::shell(&req, "").await
            .with("has_totp", Json::from(has_totp))
            .with("has_passkey", Json::from(has_passkey))
            .with("error_code", Json::from(message))
            .with("error_summary", Json::from(message));
        req.view("auth/challenge", &context)
    }

    /// `POST /mfa/passkey/options` — the options for a login assertion.
    pub async fn passkey_options(req: Request) -> Result<Response> {
        let Some(user_id) = pending(&req) else {
            return Ok(Response::new(Status::UNAUTHORIZED)
                .with_json(Json::object([("message", Json::from("Start by signing in with your password."))])));
        };
        let (credentials, challenges) = passkeys::stores(&req);
        let party = passkeys::relying_party(&req)?;

        let options = party
            .start_authentication_for(&user_id.to_string().into_bytes(), &*challenges, &*credentials)
            .await?;
        Ok(Response::json(options.json()))
    }

    /// `POST /mfa/passkey/verify` — check the assertion and admit.
    pub async fn passkey_verify(mut req: Request) -> Result<Response> {
        let Some(user_id) = pending(&req) else {
            return Ok(Response::new(Status::UNAUTHORIZED)
                .with_json(Json::object([("message", Json::from("Start by signing in with your password."))])));
        };
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let body = req.json().cloned().unwrap_or(Json::Null);
        let response = rustlavel::webauthn::AuthenticationResponse::from_json(&body)?;

        let (credentials, challenges) = passkeys::stores(&req);
        let party = passkeys::relying_party(&req)?;

        let authentication = match party.finish_authentication(&response, &*challenges, &*credentials).await {
            Ok(authentication) => authentication,
            Err(error) => {
                let email = User::find(&db, user_id).await?.map(|u| u.email).unwrap_or_default();
                LoginAttempt::record(&db, &email, Some(user_id), false, Some("passkey_failed"), &req).await?;
                return Ok(Response::new(Status::UNAUTHORIZED)
                    .with_json(Json::object([("message", Json::from(error.to_string()))])));
            }
        };

        // The assertion is valid, but valid for whom? A credential belonging to
        // another account would otherwise let anybody past this session's
        // second factor.
        if authentication.user_handle() != user_id.to_string().as_bytes() {
            return Ok(Response::new(Status::UNAUTHORIZED)
                .with_json(Json::object([("message", Json::from("That passkey belongs to a different account."))])));
        }

        let Some(mut user) = User::find(&db, user_id).await? else {
            return Ok(Response::new(Status::UNAUTHORIZED)
                .with_json(Json::object([("message", Json::from("That account no longer exists."))])));
        };
        req.session().forget(PENDING_KEY);
        LoginController::complete(&req, &db, &mut user, &tokens::now()).await?;
        Ok(Response::json(Json::object([("redirect", Json::from("/dashboard"))])))
    }
}

/// Everything a signed-in person does to their own factors.
pub struct MfaSettingsController;

impl MfaSettingsController {
    /// Begin enrolling an authenticator app.
    pub async fn start_totp(req: Request) -> Result<Response> {
        let user_id = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();

        // A fresh secret each time this is started. Re-offering an abandoned
        // one would mean a secret that was shown, perhaps screenshotted, and
        // then handed out again.
        let totp = Totp::generate();
        let encrypter = rustlavel::auth::Encrypter::from_config(req.config())?;
        let encrypted = encrypter.encrypt(&totp.secret_base32())?;

        db.table("user_totp").filter("user_id", user_id).filter_null("confirmed_at").delete(&db).await?;
        db.table("user_totp")
            .insert_without_id(
                &db,
                &[
                    ("user_id", user_id.into()),
                    ("secret_encrypted", encrypted.into()),
                    ("created_at", tokens::now().into()),
                    ("updated_at", tokens::now().into()),
                ],
            )
            .await?;

        page::flash(&req, "success", "Scan the code with your authenticator app, then enter what it shows.");
        Ok(Response::see_other("/settings/security"))
    }

    /// Confirm the first code, which is what actually turns it on.
    pub async fn confirm_totp(mut req: Request) -> Result<Response> {
        let user_id = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let code = req.input("code").unwrap_or_default();

        let rows = db
            .table("user_totp")
            .filter("user_id", user_id)
            .filter_null("confirmed_at")
            .get(&db)
            .await?;
        let Some(row) = rows.first() else {
            page::flash(&req, "error", "There is no enrolment in progress. Start again.");
            return Ok(Response::see_other("/settings/security"));
        };

        let encrypter = rustlavel::auth::Encrypter::from_config(req.config())?;
        let totp = Totp::from_base32(&encrypter.decrypt(&row.get::<String>("secret_encrypted")?)?)?;
        let unix = tokens::unix_now() as u64;

        if !totp.verify(&code, unix) {
            page::flash(&req, "error", "That code is not right. Check your phone's clock is correct, then try again.");
            return Ok(Response::see_other("/settings/security"));
        }

        db.table("user_totp")
            .filter("id", row.get::<i64>("id")?)
            .update(
                &db,
                &[
                    ("confirmed_at", tokens::now().into()),
                    ("last_step", (step_of(unix, totp.period()) as i64).into()),
                ],
            )
            .await?;

        page::flash(&req, "success", "Two-factor authentication is on. Generate recovery codes next.");
        Ok(Response::see_other("/settings/security"))
    }

    /// Turn it off, which requires the password.
    pub async fn disable_totp(mut req: Request) -> Result<Response> {
        let user_id = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let password = req.input("password").unwrap_or_default();

        // Removing a factor is exactly what somebody at a borrowed keyboard
        // would do first, so it costs a password.
        let Some(user) = User::find(&db, user_id).await? else { return Ok(Response::see_other("/login")) };
        let ok = user
            .password_hash
            .as_deref()
            .is_some_and(|hash| rustlavel::auth::verify_password(&password, hash));
        if !ok {
            page::flash(&req, "error", "That password is not right.");
            return Ok(Response::see_other("/settings/security"));
        }

        db.table("user_totp").filter("user_id", user_id).delete(&db).await?;
        page::flash(&req, "warning", "The authenticator app has been removed from your account.");
        Ok(Response::see_other("/settings/security"))
    }

    /// A fresh set of recovery codes, shown once.
    pub async fn recovery_codes(req: Request) -> Result<Response> {
        let user_id = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();

        let codes = RecoveryCodes::generate(8);
        // Everything already issued stops working. A person generating a new
        // set is usually doing it because the old ones may have been seen.
        db.table("user_recovery_codes").filter("user_id", user_id).delete(&db).await?;
        for hash in codes.hashed() {
            db.table("user_recovery_codes")
                .insert_without_id(
                    &db,
                    &[
                        ("user_id", user_id.into()),
                        ("code_hash", hash.into()),
                        ("created_at", tokens::now().into()),
                        ("updated_at", tokens::now().into()),
                    ],
                )
                .await?;
        }

        // Flashed rather than stored: they are shown on the next render and
        // never again, because the only copy after that is the person's.
        req.session().put(
            "_fresh_recovery_codes",
            Json::Array(codes.codes().iter().map(|c| Json::from(c.as_str())).collect()),
        );
        Ok(Response::see_other("/settings/security"))
    }

    /// `POST /settings/security/passkeys/options`
    pub async fn passkey_options(req: Request) -> Result<Response> {
        let user_id = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let Some(user) = User::find(&db, user_id).await? else { return Ok(Response::not_found()) };

        let (credentials, challenges) = passkeys::stores(&req);
        let party = passkeys::relying_party(&req)?;
        let entity = passkeys::user_entity(user.id, &user.email, &user.name);

        let options = party.start_registration(&entity, &*challenges, &*credentials).await?;
        Ok(Response::json(options.json()))
    }

    /// `POST /settings/security/passkeys` — store a newly registered passkey.
    pub async fn store_passkey(mut req: Request) -> Result<Response> {
        let user_id = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let Some(user) = User::find(&db, user_id).await? else { return Ok(Response::not_found()) };

        let body = req.json().cloned().unwrap_or(Json::Null);
        let response = rustlavel::webauthn::RegistrationResponse::from_json(&body)?;
        let label = body.get("label").and_then(Json::as_str).unwrap_or("Passkey").to_string();

        let (credentials, challenges) = passkeys::stores(&req);
        let party = passkeys::relying_party(&req)?;
        let entity = passkeys::user_entity(user.id, &user.email, &user.name);

        match party.finish_registration(&entity, &response, &*challenges, &*credentials).await {
            Ok(registration) => {
                let id = rustlavel::auth::base64::encode_url(registration.credential().id());
                db.table("user_passkeys")
                    .filter("credential_id", id)
                    .update(&db, &[("label", label.chars().take(120).collect::<String>().into())])
                    .await?;
                Ok(Response::json(Json::object([("ok", Json::from(true))])))
            }
            Err(error) => Ok(Response::new(Status::UNPROCESSABLE)
                .with_json(Json::object([("message", Json::from(error.to_string()))]))),
        }
    }

    pub async fn delete_passkey(req: Request) -> Result<Response> {
        let user_id = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let id = req.param_as::<i64>("id").unwrap_or_default();

        passkeys::DbPasskeys::new(db).delete(user_id, id).await?;
        page::flash(&req, "warning", "That passkey has been removed.");
        Ok(Response::see_other("/settings/security"))
    }
}
