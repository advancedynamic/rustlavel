//! Settings an administrator changes from the Settings page.
//!
//! Two rules keep this from becoming a second, quieter configuration system.
//!
//! **Every setting is declared here**, in [`CATALOGUE`]: its key, its kind, its
//! default, and whether it is a secret. A setting the catalogue does not know
//! about cannot be written, so a typo in a form field cannot silently create a
//! row that nothing ever reads. It is also what the page renders from, so a new
//! setting is one entry rather than an entry plus a form field plus a default
//! plus a migration.
//!
//! **`.env` still wins.** A value in the environment is a deployment decision —
//! somebody set it deliberately, probably in a secret store — and an
//! administrator clicking a toggle must not silently override it. Where both
//! exist the environment is used and the form says so. This is the rule that
//! stops "why is production ignoring the settings page" being a three-hour
//! afternoon.

use rustlavel::prelude::*;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// What a setting holds, which decides how it renders and how it is read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    LongText,
    Number,
    Toggle,
    /// A fixed set of choices, rendered as a `<select>`.
    Choice,
    Colour,
    Secret,
}

/// One declared setting.
pub struct Setting {
    pub key: &'static str,
    pub kind: Kind,
    pub default: &'static str,
    /// The `.env` variable that overrides it, when there is one.
    pub env: Option<&'static str>,
    pub choices: &'static [(&'static str, &'static str)],
}

const fn s(key: &'static str, kind: Kind, default: &'static str) -> Setting {
    Setting { key, kind, default, env: None, choices: &[] }
}

const fn env(key: &'static str, kind: Kind, default: &'static str, variable: &'static str) -> Setting {
    Setting { key, kind, default, env: Some(variable), choices: &[] }
}

const fn choice(
    key: &'static str,
    default: &'static str,
    choices: &'static [(&'static str, &'static str)],
) -> Setting {
    Setting { key, kind: Kind::Choice, default, env: None, choices }
}

/// The date formats the General tab offers.
///
/// `pub(crate)` so `support::format` can prove it renders every one of them:
/// the first version of that formatter matched on the *labels* in the right
/// column and the catalogue stores the values in the left, so three of the
/// four silently fell through to the fourth.
pub(crate) const DATE_FORMATS: &[(&str, &str)] = &[
    ("d/m/Y", "DD/MM/YYYY"),
    ("m/d/Y", "MM/DD/YYYY"),
    ("Y-m-d", "YYYY-MM-DD"),
    ("d M Y", "DD Mon YYYY"),
];

pub(crate) const TIME_FORMATS: &[(&str, &str)] = &[("24", "24 Hour"), ("12", "12 Hour")];

pub(crate) const TIMEZONES: &[(&str, &str)] = &[
    ("UTC", "UTC"),
    ("Asia/Jakarta", "Asia/Jakarta (WIB)"),
    ("Asia/Makassar", "Asia/Makassar (WITA)"),
    ("Asia/Jayapura", "Asia/Jayapura (WIT)"),
    ("Asia/Singapore", "Asia/Singapore"),
    ("Europe/London", "Europe/London"),
    ("America/New_York", "America/New_York"),
];

const MAIL_DRIVERS: &[(&str, &str)] =
    &[("smtp", "SMTP"), ("log", "Log (write to the log)"), ("file", "File (write .eml files)")];

const MAIL_ENCRYPTION: &[(&str, &str)] =
    &[("tls", "TLS"), ("starttls", "STARTTLS"), ("none", "None")];

const LENGTHS: &[(&str, &str)] =
    &[("8", "8"), ("10", "10"), ("12", "12"), ("14", "14"), ("16", "16"), ("20", "20")];

const REUSE: &[(&str, &str)] = &[
    ("0", "Disabled"),
    ("3", "Last 3 passwords"),
    ("5", "Last 5 passwords"),
    ("10", "Last 10 passwords"),
];

const TIMEOUTS: &[(&str, &str)] = &[
    ("30", "30 minutes"),
    ("60", "1 hour"),
    ("120", "2 hours"),
    ("480", "8 hours"),
    ("1440", "24 hours"),
];

/// How often a backup should be taken.
///
/// A schedule is a *statement of intent*: something has to run it, and this
/// application has no clock of its own. The Backup tab says so, and says what
/// to add — a schedule that quietly does nothing is worse than no schedule.
const SCHEDULES: &[(&str, &str)] = &[
    ("disabled", "Disabled — take them by hand"),
    ("6h", "Every 6 hours"),
    ("daily", "Daily"),
    ("weekly", "Weekly (Sunday)"),
];

/// How many backups to keep. Applied after each successful one.
const RETENTIONS: &[(&str, &str)] = &[
    ("0", "Keep everything"),
    ("7", "Keep the last 7"),
    ("14", "Keep the last 14"),
    ("30", "Keep the last 30"),
];

// `backup.destination` and `backup.bucket` used to live here, offering "local"
// or an S3-compatible store. Nothing in this framework writes a backup anywhere
// but the local disk, and the argument the comment made against the two
// destinations a mock-up had offered applies just as well to the one that was
// left: a dropdown that offers a destination backups do not reach is how
// somebody discovers at restore time that there are no backups. The directory
// is `backup.path`, which `backup_controller` now actually reads.

/// How a number is written.
const NUMBERS: &[(&str, &str)] = &[
    ("id", "1.234.567,89 — dot for thousands"),
    ("en", "1,234,567.89 — comma for thousands"),
    ("plain", "1234567.89 — no separator"),
];

const CURRENCIES: &[(&str, &str)] =
    &[("Rp ", "Rp 1.234.567"), ("IDR ", "IDR 1.234.567"), ("$", "$1,234,567"), ("", "1.234.567")];

const ATTEMPTS: &[(&str, &str)] =
    &[("3", "3"), ("5", "5"), ("10", "10"), ("0", "No limit (not advised)")];

const LOCKOUTS: &[(&str, &str)] = &[
    ("5", "5 minutes"),
    ("15", "15 minutes"),
    ("60", "1 hour"),
    ("1440", "24 hours"),
];

const LOCALES: &[(&str, &str)] =
    &[("en", "English"), ("id", "Bahasa Indonesia"), ("ms", "Bahasa Melayu")];

/// Every setting the application has. Nothing outside this list can be written.
pub const CATALOGUE: &[Setting] = &[
    // --- General -------------------------------------------------------
    env("app.name", Kind::Text, "Rustlavel", "APP_NAME"),
    env("app.url", Kind::Text, "http://localhost:8000", "APP_URL"),
    s("app.description", Kind::LongText, ""),
    choice("app.date_format", "d M Y", DATE_FORMATS),
    choice("app.time_format", "24", TIME_FORMATS),
    choice("app.timezone", "UTC", TIMEZONES),

    // --- Email ---------------------------------------------------------
    Setting { key: "mail.driver", kind: Kind::Choice, default: "log", env: Some("MAIL_TRANSPORT"), choices: MAIL_DRIVERS },
    env("mail.host", Kind::Text, "127.0.0.1", "MAIL_HOST"),
    env("mail.port", Kind::Number, "1025", "MAIL_PORT"),
    Setting { key: "mail.encryption", kind: Kind::Choice, default: "none", env: Some("MAIL_ENCRYPTION"), choices: MAIL_ENCRYPTION },
    env("mail.username", Kind::Text, "", "MAIL_USERNAME"),
    env("mail.password", Kind::Secret, "", "MAIL_PASSWORD"),
    env("mail.from.address", Kind::Text, "noreply@example.com", "MAIL_FROM_ADDRESS"),
    env("mail.from.name", Kind::Text, "Rustlavel", "MAIL_FROM_NAME"),

    // --- Security ------------------------------------------------------
    env("auth.registration.open", Kind::Toggle, "true", "AUTH_REGISTRATION_OPEN"),
    s("auth.magic_link", Kind::Toggle, "false"),
    s("auth.verify_email", Kind::Toggle, "true"),
    s("auth.require_mfa", Kind::Toggle, "false"),
    Setting { key: "auth.password.min_length", kind: Kind::Choice, default: "12", env: Some("AUTH_PASSWORD_MIN_LENGTH"), choices: LENGTHS },
    s("auth.password.uppercase", Kind::Toggle, "false"),
    s("auth.password.lowercase", Kind::Toggle, "false"),
    s("auth.password.number", Kind::Toggle, "false"),
    s("auth.password.symbol", Kind::Toggle, "false"),
    s("auth.password.breached", Kind::Toggle, "false"),
    choice("auth.password.reuse", "0", REUSE),
    choice("auth.session.timeout", "120", TIMEOUTS),
    choice("auth.lockout.attempts", "5", ATTEMPTS),
    choice("auth.lockout.minutes", "15", LOCKOUTS),

    // --- Backup --------------------------------------------------------
    choice("backup.schedule", "disabled", SCHEDULES),
    choice("backup.retention", "0", RETENTIONS),
    env("backup.path", Kind::Text, "storage/backups", "BACKUP_PATH"),

    // --- Language ------------------------------------------------------
    // `app.locale` reaches the pages as the `lang` attribute on <html>. The
    // fallback locale and the first day of the week were beside it and reached
    // nothing: translation needs `rustlavel-i18n`, which this kit does not
    // enable, and there is no calendar here to start on a Monday.
    choice("app.locale", "en", LOCALES),
    choice("app.number_format", "id", NUMBERS),
    choice("app.currency", "Rp ", CURRENCIES),

    // --- Appearance ----------------------------------------------------
    // The one colour that reaches the whole application. Everything else on
    // this tab dresses a single surface; this one is the brand, and
    // `theme_controller` turns it into the eleven shades the pages are drawn
    // from — see `support::palette`.
    // The defaults are the operations dashboard this kit's look comes from.
    // The sidebar is deliberately dark in *both* schemes: a navy rail beside a
    // pale page is what separates the navigation from the work, and inverting
    // it in light mode is what made the old sidebar disappear into the page.
    s("theme.brand", Kind::Colour, "#0e5fa8"),
    // The two login colours default to the same navy, because the panel they
    // paint is flat in the design this comes from. They are still two colours:
    // set them apart and the panel becomes the gradient they describe.
    s("theme.login.light.from", Kind::Colour, "#0b2e4f"),
    s("theme.login.light.to", Kind::Colour, "#0b2e4f"),
    s("theme.login.dark.from", Kind::Colour, "#071d33"),
    s("theme.login.dark.to", Kind::Colour, "#071d33"),
    s("theme.sidebar.light.bg", Kind::Colour, "#0b2e4f"),
    s("theme.sidebar.light.text", Kind::Colour, "#9dbad3"),
    s("theme.sidebar.light.active_bg", Kind::Colour, "#14456f"),
    s("theme.sidebar.light.active_text", Kind::Colour, "#ffffff"),
    s("theme.sidebar.dark.bg", Kind::Colour, "#071d33"),
    s("theme.sidebar.dark.text", Kind::Colour, "#7fa3c0"),
    s("theme.sidebar.dark.active_bg", Kind::Colour, "#0e3e68"),
    s("theme.sidebar.dark.active_text", Kind::Colour, "#ffffff"),
    s("theme.logo.light", Kind::Text, ""),
    s("theme.logo.dark", Kind::Text, ""),
];

pub fn declared(key: &str) -> Option<&'static Setting> {
    CATALOGUE.iter().find(|setting| setting.key == key)
}

/// The settings, read once and kept until something writes.
///
/// Cheap to clone; every clone shares one cache, so a write anywhere is seen
/// everywhere. Registered in application state and resolved per request.
#[derive(Clone)]
pub struct Settings {
    db: Database,
    cache: Arc<RwLock<Option<BTreeMap<String, String>>>>,
    key: Arc<rustlavel::auth::Encrypter>,
}

impl Settings {
    pub fn new(db: Database, encrypter: rustlavel::auth::Encrypter) -> Self {
        Settings { db, cache: Arc::new(RwLock::new(None)), key: Arc::new(encrypter) }
    }

    pub fn from_config(db: Database, config: &Config) -> Result<Self> {
        Ok(Settings::new(db, rustlavel::auth::Encrypter::from_config(config)?))
    }

    /// Load every row, decrypting the secrets.
    async fn load(&self) -> Result<BTreeMap<String, String>> {
        let rows = self.db.table("settings").get(&self.db).await?;
        let mut values = BTreeMap::new();

        for row in &rows {
            let Ok(key) = row.get::<String>("key") else { continue };
            let raw = row.get::<String>("value").unwrap_or_default();
            let secret = row.get::<i64>("is_secret").map(|n| n != 0).unwrap_or(false);

            let value = if secret && !raw.is_empty() {
                // A secret that will not decrypt is treated as absent rather
                // than as a panic: rotating APP_KEY should degrade the mail
                // password to "unset", not stop the application booting.
                match self.key.decrypt(&raw) {
                    Ok(plain) => plain,
                    Err(_) => {
                        warn!("the stored value for `{key}` could not be decrypted; treating it as unset");
                        continue;
                    }
                }
            } else {
                raw
            };
            values.insert(key, value);
        }
        Ok(values)
    }

    async fn all(&self) -> Result<BTreeMap<String, String>> {
        if let Some(cached) = self.cache.read().expect("settings lock").clone() {
            return Ok(cached);
        }
        let loaded = self.load().await?;
        *self.cache.write().expect("settings lock") = Some(loaded.clone());
        Ok(loaded)
    }

    /// Forget the cache, so the next read goes to the database.
    pub fn forget(&self) {
        *self.cache.write().expect("settings lock") = None;
    }

    /// One value: the environment first, then the stored row, then the default.
    pub async fn get(&self, key: &str) -> String {
        let declared = declared(key);

        if let Some(variable) = declared.and_then(|setting| setting.env) {
            let from_env = std::env::var(variable).unwrap_or_default();
            if !from_env.is_empty() {
                return from_env;
            }
        }

        let stored = self.all().await.ok().and_then(|values| values.get(key).cloned());
        match stored.filter(|value| !value.is_empty()) {
            Some(value) => value,
            None => declared.map(|setting| setting.default.to_string()).unwrap_or_default(),
        }
    }

    pub async fn bool(&self, key: &str) -> bool {
        matches!(self.get(key).await.as_str(), "1" | "true" | "yes" | "on")
    }

    pub async fn int(&self, key: &str, fallback: i64) -> i64 {
        self.get(key).await.parse().unwrap_or(fallback)
    }

    /// Whether the environment is deciding this one, so the form can say so
    /// rather than pretend the box it is drawing does anything.
    pub fn overridden(key: &str) -> bool {
        declared(key)
            .and_then(|setting| setting.env)
            .is_some_and(|variable| !std::env::var(variable).unwrap_or_default().is_empty())
    }

    /// Write one setting. Refuses a key the catalogue does not declare.
    pub async fn put(&self, key: &str, value: &str) -> Result<()> {
        let Some(setting) = declared(key) else {
            return Err(Error::msg(format!(
                "`{key}` is not a setting. Add it to CATALOGUE in src/support/settings.rs first — \
                 a form that can write any key is a form that can write anything."
            )));
        };

        let secret = setting.kind == Kind::Secret;
        // An empty secret means "leave it alone": the form renders a password
        // box with dots in it and submitting the page unchanged must not wipe
        // the stored password.
        if secret && value.is_empty() {
            return Ok(());
        }

        let stored = if secret { self.key.encrypt(value)? } else { value.to_string() };
        let now = crate::support::tokens::now();

        let existing = self.db.table("settings").filter("key", key).first(&self.db).await?;
        match existing {
            Some(_) => {
                self.db
                    .table("settings")
                    .filter("key", key)
                    .update(&self.db, &[("value", stored.into()), ("updated_at", now.into())])
                    .await?;
            }
            None => {
                self.db
                    .table("settings")
                    .insert_without_id(
                        &self.db,
                        &[
                            ("key", key.into()),
                            ("value", stored.into()),
                            ("is_secret", secret.into()),
                            ("created_at", now.clone().into()),
                            ("updated_at", now.into()),
                        ],
                    )
                    .await?;
            }
        }

        self.forget();
        Ok(())
    }

    /// Write several, then invalidate once.
    pub async fn put_all(&self, values: &[(String, String)]) -> Result<usize> {
        let mut written = 0;
        for (key, value) in values {
            if declared(key).is_some() {
                self.put(key, value).await?;
                written += 1;
            }
        }
        Ok(written)
    }

    /// Every setting as the page renders it: value, source and choices.
    pub async fn view(&self, prefix: &str) -> Result<Json> {
        let mut fields = Vec::new();
        for setting in CATALOGUE.iter().filter(|s| s.key.starts_with(prefix)) {
            let value = self.get(setting.key).await;
            let choices: Vec<Json> = setting
                .choices
                .iter()
                .map(|(value_, label)| {
                    Json::object([
                        ("value", Json::from(*value_)),
                        ("label", Json::from(*label)),
                        ("selected", Json::from(*value_ == value)),
                    ])
                })
                .collect();

            fields.push((
                // Dots are the view engine's path separator, so a key with one
                // in it would be unreachable from a template.
                setting.key.replace('.', "_"),
                Json::object([
                    ("key", Json::from(setting.key)),
                    ("value", Json::from(value.as_str())),
                    ("on", Json::from(matches!(value.as_str(), "1" | "true" | "yes" | "on"))),
                    ("locked", Json::from(Settings::overridden(setting.key))),
                    ("env", setting.env.map_or(Json::Null, Json::from)),
                    ("choices", Json::Array(choices)),
                ]),
            ));
        }
        Ok(Json::object(fields))
    }

    /// Everything, for the export button. Secrets are named but not included.
    pub async fn export(&self) -> Result<Json> {
        let mut fields = Vec::new();
        for setting in CATALOGUE {
            let value = if setting.kind == Kind::Secret {
                Json::from("(not exported)")
            } else {
                Json::from(self.get(setting.key).await)
            };
            fields.push((setting.key, value));
        }
        Ok(Json::object(fields))
    }
}
