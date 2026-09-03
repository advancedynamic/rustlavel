//! The Settings page: six tabs, one saved form each.
//!
//! Each tab is its own URL rather than a client-side strip, so a person can
//! bookmark the mail settings, open them in another window, and land back on
//! them after saving. None of that survives a tab strip built in JavaScript,
//! and all of it is what somebody administering an application expects.
//!
//! Saving goes through [`Settings::put_all`], which refuses any key the
//! catalogue does not declare. A form that can write any key is a form that can
//! write anything, so the list of what exists lives in one place and this
//! controller cannot widen it.

use rustlavel::prelude::*;

use crate::support::page;
use crate::support::settings::{CATALOGUE, Kind, Settings, declared};

use super::users_controller::with_current_user;

/// The tabs, in the order they are drawn.
const TABS: &[(&str, &str, &str)] = &[
    ("general", "General", "app."),
    ("email", "Email", "mail."),
    ("security", "Security", "auth."),
    // No prefix, and deliberately not the empty string: an empty prefix makes
    // `starts_with` match *every* setting, so a POST to this tab would have
    // rewritten the whole catalogue. This tab stores nothing through the shared
    // save path; its actions have routes of their own.
    ("backup", "Backup", "\u{0}none"),
    ("language", "Language", "app.locale"),
    ("appearance", "Appearance", "theme."),
];

pub struct AdminSettingsController;

impl AdminSettingsController {
    /// `GET /admin/settings` — the first tab.
    pub async fn index(req: Request) -> Result<Response> {
        Self::show_tab(req, "general").await
    }

    /// `GET /admin/settings/{tab}`
    pub async fn tab(req: Request) -> Result<Response> {
        let slug = req.param("tab").unwrap_or("general").to_string();
        Self::show_tab(req, &slug).await
    }

    async fn show_tab(req: Request, slug: &str) -> Result<Response> {
        let Some((slug, _, prefix)) = TABS.iter().find(|(name, _, _)| *name == slug) else {
            return Ok(Response::not_found());
        };
        let db = req.state::<Database>().expect("the database is registered in main.rs").clone();
        let settings = Self::store(&req)?;

        let mut context = page::shell(&req, "settings").await;
        context = with_current_user(context, &req, &db).await?;
        context = context.with("tabs", Json::Array(Self::tab_list(slug)));

        // Every tab reads its own values under one name, so a template says
        // `s.app_name.value` rather than reaching for a variable the controller
        // had to remember to pass.
        context = context.with("s", settings.view(prefix).await?);

        if *slug == "backup" {
            context =
                crate::controllers::admin::backup_controller::BackupController::context(&req, context)
                    .await?;
        }
        if *slug == "email" {
            context = context.with("test_to", Json::from(""));
        }
        if *slug == "language" {
            context = context.with("languages", Self::languages(&req, &settings).await);
        }

        req.view(&format!("settings/tabs/{slug}"), &context)
    }

    /// `POST /admin/settings/{tab}` — save whatever that tab declares.
    ///
    /// The fields are taken from the catalogue rather than from the submitted
    /// body: a browser sends nothing at all for an unticked checkbox, so a
    /// handler that saved only what arrived could never turn a toggle *off*.
    pub async fn save(mut req: Request) -> Result<Response> {
        let slug = req.param("tab").unwrap_or("general").to_string();
        let Some((_, label, prefix)) = TABS.iter().find(|(name, _, _)| *name == slug) else {
            return Ok(Response::not_found());
        };
        let settings = Self::store(&req)?.clone();

        let mut values = Vec::new();
        for setting in CATALOGUE.iter().filter(|s| s.key.starts_with(prefix)) {
            // A setting the environment decides is not editable here, and
            // writing it would leave a stored row that nothing ever reads.
            if Settings::overridden(setting.key) {
                continue;
            }
            let submitted = req.input(setting.key);
            let value = match setting.kind {
                // Absent means off. This is the line that makes a toggle work.
                Kind::Toggle => {
                    if submitted.is_some() { "true".to_string() } else { "false".to_string() }
                }
                // Coerced here as well as in the appearance handlers, because
                // this path is reachable by anyone who can post a form and a
                // stored value that is not a colour is junk in a stylesheet's
                // clothing. `theme_controller::colour` sanitises again on the
                // way out, so this is the second of two nets rather than the
                // only one.
                Kind::Colour => match submitted {
                    Some(value) => crate::controllers::theme_controller::colour(&value),
                    None => continue,
                },
                _ => match submitted {
                    Some(value) => value,
                    None => continue,
                },
            };
            values.push((setting.key.to_string(), value));
        }

        let written = settings.put_all(&values).await?;

        // The keys, not the values. A settings tab holds a mail password, and
        // an audit trail that records what it was changed to is a place the
        // password now lives in the clear.
        if written > 0 {
            if let Some(audit) = crate::support::audit::of(&req, "settings.updated") {
                audit
                    .describe(format!("Updated the {} settings", label.to_lowercase()))
                    .with("tab", Json::from(slug.as_str()))
                    .with("changed", Json::from(written as i64))
                    .record()
                    .await;
            }
        }
        page::flash(&req, "success", format!("{label} settings saved ({written} changed)."));
        Ok(Response::see_other(format!("/admin/settings/{slug}")))
    }

    /// `POST /admin/settings/email/test` — prove the settings work.
    ///
    /// Sends through the mailer the application booted with, which is the
    /// honest thing to test: it is the one that will send the password resets.
    /// That does mean the saved settings only take effect on restart, and the
    /// page says so rather than letting somebody believe a failing test means
    /// their new host is wrong.
    pub async fn send_test(mut req: Request) -> Result<Response> {
        let to = req.input("to").unwrap_or_default().trim().to_string();
        if to.is_empty() || !to.contains('@') {
            page::flash(&req, "error", "Give an address to send the test to.");
            return Ok(Response::see_other("/admin/settings/email"));
        }

        if req.state::<rustlavel::mail::Mailer>().is_none() {
            page::flash(&req, "error", "No mailer is configured, so there is nothing to test.");
            return Ok(Response::see_other("/admin/settings/email"));
        }

        let settings = Self::store(&req)?;
        let name = settings.get("app.name").await;
        let driver = settings.get("mail.driver").await;

        let message = rustlavel::mail::Message::new()
            .to(to.as_str())
            .subject(format!("{name}: test message"))
            .text(format!(
                "This is a test from {name}.\n\nIf you are reading it, the mail settings work.\n\n\
                 Driver: {driver}\nSent: {}\n",
                crate::support::tokens::now()
            ));

        // Through the same helper as every other message, so the test proves the
        // From address on the Email tab as well as the transport.
        match crate::support::mail::send(&req, message).await {
            Ok(()) => page::flash(&req, "success", format!("A test message has gone to {to}.")),
            // The reason is shown rather than swallowed: "it did not send" is
            // not something anybody can act on, and the SMTP server's own words
            // usually name the problem exactly.
            Err(error) => page::flash(&req, "error", format!("The test did not send: {error}")),
        }
        Ok(Response::see_other("/admin/settings/email"))
    }

    /// `GET /admin/settings/export` — everything, as JSON, minus the secrets.
    pub async fn export(req: Request) -> Result<Response> {
        let settings = Self::store(&req)?;
        let body = settings.export().await?;

        Ok(Response::json(body)
            .with_header("content-disposition", "attachment; filename=\"settings.json\""))
    }

    fn tab_list(current: &str) -> Vec<Json> {
        TABS.iter()
            .map(|(slug, label, _)| {
                Json::object([
                    ("slug", Json::from(*slug)),
                    ("label", Json::from(*label)),
                    ("current", Json::from(*slug == current)),
                ])
            })
            .collect()
    }

    /// Which languages have a translation file, for the Language tab.
    async fn languages(req: &Request, settings: &Settings) -> Json {
        let root = req.config().string("view.lang", "lang");
        let chosen = settings.get("app.locale").await;
        let _ = chosen;

        let listed = declared("app.locale").map(|setting| setting.choices).unwrap_or(&[]);
        Json::Array(
            listed
                .iter()
                .map(|(code, label)| {
                    let path = std::path::Path::new(&root).join(format!("{code}.json"));
                    let phrases = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|source| Json::parse(&source).ok())
                        .and_then(|value| value.as_object().map(|fields| fields.len() as i64))
                        .unwrap_or(0);

                    Json::object([
                        ("code", Json::from(*code)),
                        ("label", Json::from(*label)),
                        ("present", Json::from(path.is_file())),
                        ("phrases", Json::from(phrases)),
                    ])
                })
                .collect(),
        )
    }

    /// The settings store, or a clear failure. Never a silent default.
    pub fn store(req: &Request) -> Result<Settings> {
        req.state::<Settings>().cloned().ok_or_else(|| {
            Error::msg(
                "the settings store is not registered. Add \
                 `.state(Settings::from_config(db.clone(), app.config())?)` in main.rs.",
            )
        })
    }
}
