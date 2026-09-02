use rustlavel::prelude::*;

use crate::models::user::User;
use crate::support::{page, passkeys, tokens};

pub struct SettingsController;

impl SettingsController {
    pub async fn security(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let user_id = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let Some(user) = User::find(&db, user_id).await? else { return Ok(Response::see_other("/login")) };

        let mut context = page::shell(&req, "security").await;
        context = page::with_user(context, &req, &user).await?;

        // Authenticator app: enrolled and confirmed, enrolled and not, or neither.
        let rows = db.table("user_totp").filter("user_id", user_id).get(&db).await?;
        let confirmed = rows.iter().find(|r| r.get::<String>("confirmed_at").is_ok_and(|c| !c.is_empty()));
        let pending = rows.iter().find(|r| !r.get::<String>("confirmed_at").is_ok_and(|c| !c.is_empty()));

        context = context
            .with("totp_enabled", Json::from(confirmed.is_some()))
            .with(
                "totp_enabled_at",
                Json::from(
                    confirmed
                        .and_then(|r| r.get::<String>("confirmed_at").ok())
                        .map(|at| tokens::humanise(&at))
                        .unwrap_or_default(),
                ),
            )
            .with("totp_pending", Json::from(pending.is_some()));

        if let Some(row) = pending {
            let encrypter = rustlavel::auth::Encrypter::from_config(req.config())?;
            let secret = encrypter.decrypt(&row.get::<String>("secret_encrypted")?)?;
            let totp = rustlavel::auth::totp::Totp::from_base32(&secret)?;
            let issuer = req.config().string("app.name", "Rustlavel");

            // Rendered here as SVG rather than fetched from a chart service:
            // that would post the TOTP secret to a third party, and a
            // JavaScript library would need a looser policy than this site has.
            let uri = totp.provisioning_uri(&user.email, &issuer);
            let svg = rustlavel::auth::qr::encode(&uri)?.to_svg_titled(5, 2, "Scan with your authenticator app");

            context = context
                .with("totp_secret", Json::from(totp.secret_base32()))
                .with("totp_qr", Json::from(svg));
        }

        // Passkeys.
        let store = passkeys::DbPasskeys::new(db.clone());
        let keys = store.list_for(user_id).await?;
        context = context
            .with("passkeys_empty", Json::from(keys.is_empty()))
            .with("passkey_count", Json::from(keys.len() as i64))
            .with("passkeys", Json::Array(keys));

        // Recovery codes: how many are left, and any set just generated.
        let remaining = db
            .table("user_recovery_codes")
            .filter("user_id", user_id)
            .filter_null("used_at")
            .count(&db)
            .await?;

        let fresh = req
            .session()
            .forget("_fresh_recovery_codes")
            .and_then(|value| value.as_array().map(|items| items.to_vec()))
            .unwrap_or_default();
        let fresh_text = fresh
            .iter()
            .filter_map(Json::as_str)
            .collect::<Vec<_>>()
            .join("\n");

        context = context
            .with("recovery_remaining", if remaining > 0 { Json::from(remaining) } else { Json::Null })
            .with("fresh_recovery_codes_empty", Json::from(fresh.is_empty()))
            .with("fresh_recovery_codes", Json::Array(fresh))
            .with("fresh_recovery_codes_text", Json::from(fresh_text));

        req.view("settings/security", &context)
    }

    /// Store the theme choice in a cookie the layout reads on the next render.
    ///
    /// A cookie rather than JavaScript, so the very first paint is already the
    /// right colour — the usual fix for that flash is an inline script in the
    /// head, which is precisely what this application's policy forbids.
    pub async fn theme(mut req: Request) -> Result<Response> {
        let theme = req.input("theme").unwrap_or_default();
        let theme = if theme == "dark" { "dark" } else { "light" };
        let back = req.header("referer").filter(|r| r.starts_with('/')).unwrap_or("/dashboard").to_string();

        Ok(Response::see_other(back).with_cookie(
            Cookie::new("theme", theme)
                .path("/")
                .http_only(false)
                .same_site(rustlavel::SameSite::Lax)
                .max_age(std::time::Duration::from_secs(365 * 24 * 60 * 60)),
        ))
    }

    /// End every session but this one.
    pub async fn revoke_sessions(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let user_id = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();
        let Some(mut user) = User::find(&db, user_id).await? else { return Ok(Response::see_other("/login")) };

        user.session_epoch = Some(rustlavel::auth::random::hex(16));
        user.update(&db).await?;
        req.session().put("_epoch", Json::from(user.session_epoch.clone().unwrap_or_default()));

        page::flash(&req, "success", "Every other device has been signed out.");
        Ok(Response::see_other("/settings/security"))
    }
}

/// Starting and stopping "view as".
pub struct ImpersonationController;

impl ImpersonationController {
    pub async fn start(req: Request) -> Result<Response> {
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let target = req.param_as::<i64>("id").unwrap_or_default();
        let me = req.identity().and_then(|id| id.id_as::<i64>()).unwrap_or_default();

        // The permission is checked by the route's middleware; this is the
        // check the middleware cannot make.
        let Some(user) = User::find(&db, target).await? else { return Ok(Response::not_found()) };

        // Refusing to impersonate somebody who could impersonate you back is
        // what stops one administrator's session becoming every
        // administrator's. Without it, taking one account takes them all.
        if let Some(store) = req.state::<rustlavel::rbac::Permissions>()
            && store.has_permission(target, "users.impersonate").await?
        {
            page::flash(&req, "error", "That user may impersonate others, so they cannot be impersonated.");
            return Ok(Response::see_other("/admin/users"));
        }

        rustlavel::auth::Impersonation::start(req.session(), target.to_string())?;
        warn!("user {me} is now viewing the site as user {target} ({})", user.email);
        Ok(Response::see_other("/dashboard"))
    }

    pub async fn stop(req: Request) -> Result<Response> {
        rustlavel::auth::Impersonation::stop(req.session());
        Ok(Response::see_other("/admin/users"))
    }
}
