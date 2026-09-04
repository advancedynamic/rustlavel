//! rustlavel-i18n: translations.
//!
//! ```ignore
//! t("welcome.title")
//! t_with("orders.count", &[("count", "3")])
//! choice("orders.count", 3)          // picks a plural form
//! ```
//!
//! Translations live in `lang/<locale>.json`, loaded once at boot and shared.
//! Keys are dotted paths into that document, so a file can be organised by
//! area rather than being one flat list.

pub mod middleware;

use rustlavel_core::{Config, Error, Json, Result};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

/// The loaded translations for every locale.
#[derive(Clone, Default)]
pub struct Translator {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    locales: RwLock<BTreeMap<String, Json>>,
    /// The locale used when none is chosen per request.
    default: RwLock<String>,
    /// Used when a key is missing from the active locale, before falling back
    /// to the key itself.
    fallback: RwLock<String>,
}

impl Translator {
    pub fn new() -> Self {
        let translator = Translator::default();
        *translator.inner.default.write().expect("locale lock") = "en".to_string();
        *translator.inner.fallback.write().expect("locale lock") = "en".to_string();
        translator
    }

    /// Load every `lang/*.json`, keyed by file stem.
    pub fn load_dir(&self, dir: impl AsRef<Path>) -> Result<usize> {
        let dir = dir.as_ref();
        if !dir.is_dir() {
            return Ok(0);
        }

        let mut loaded = 0;
        let mut entries: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        entries.sort();

        for path in entries {
            let Some(locale) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            let source = std::fs::read_to_string(&path)?;
            let parsed = Json::parse(&source).map_err(|e| {
                Error::msg(format!("{}: {e}", path.display()))
            })?;
            self.insert(locale, parsed);
            loaded += 1;
        }

        Ok(loaded)
    }

    /// Add or replace one locale's translations.
    pub fn insert(&self, locale: &str, translations: Json) {
        self.inner
            .locales
            .write()
            .expect("locale lock")
            .insert(locale.to_string(), translations);
    }

    pub fn set_default(&self, locale: &str) {
        *self.inner.default.write().expect("locale lock") = locale.to_string();
    }

    pub fn set_fallback(&self, locale: &str) {
        *self.inner.fallback.write().expect("locale lock") = locale.to_string();
    }

    pub fn default_locale(&self) -> String {
        self.inner.default.read().expect("locale lock").clone()
    }

    pub fn has_locale(&self, locale: &str) -> bool {
        self.inner.locales.read().expect("locale lock").contains_key(locale)
    }

    pub fn locales(&self) -> Vec<String> {
        self.inner.locales.read().expect("locale lock").keys().cloned().collect()
    }

    /// Translate a key in the default locale.
    pub fn get(&self, key: &str) -> String {
        self.get_in(&self.default_locale(), key, &[])
    }

    /// Translate with `:placeholder` replacements.
    pub fn get_with(&self, key: &str, replacements: &[(&str, &str)]) -> String {
        self.get_in(&self.default_locale(), key, replacements)
    }

    /// Translate in a specific locale.
    ///
    /// A missing key falls back to the fallback locale, then to the key itself
    /// — a page with a visible `orders.title` is a bug report; a blank space is
    /// a mystery.
    pub fn get_in(&self, locale: &str, key: &str, replacements: &[(&str, &str)]) -> String {
        let locales = self.inner.locales.read().expect("locale lock");
        let fallback = self.inner.fallback.read().expect("locale lock").clone();

        let found = locales
            .get(locale)
            .and_then(|tree| lookup(tree, key))
            .or_else(|| locales.get(&fallback).and_then(|tree| lookup(tree, key)));

        match found {
            Some(text) => interpolate(&text, replacements),
            None => key.to_string(),
        }
    }

    /// Pick a plural form by count.
    ///
    /// Forms are separated by `|`, and may carry explicit ranges the way
    /// Laravel's do: `{0} none|[1,19] some|[20,*] many`. Without ranges, two
    /// forms mean singular and plural.
    pub fn choice(&self, key: &str, count: i64, replacements: &[(&str, &str)]) -> String {
        self.choice_in(&self.default_locale(), key, count, replacements)
    }

    pub fn choice_in(
        &self,
        locale: &str,
        key: &str,
        count: i64,
        replacements: &[(&str, &str)],
    ) -> String {
        let line = self.get_in(locale, key, &[]);
        let count_text = count.to_string();

        let mut all = replacements.to_vec();
        if !all.iter().any(|(name, _)| *name == "count") {
            all.push(("count", &count_text));
        }

        interpolate(&select_form(&line, count), &all)
    }
}

/// Read a dotted path out of a translation document.
fn lookup(tree: &Json, key: &str) -> Option<String> {
    match tree.get(key)? {
        Json::String(text) => Some(text.clone()),
        Json::Number(n) => Some(n.to_string()),
        // An object at the key means the caller stopped one level too high.
        _ => None,
    }
}

/// Replace `:name` placeholders.
///
/// Longer names are substituted first, so `:name_full` is not eaten by `:name`.
fn interpolate(text: &str, replacements: &[(&str, &str)]) -> String {
    if replacements.is_empty() || !text.contains(':') {
        return text.to_string();
    }

    let mut ordered = replacements.to_vec();
    ordered.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));

    let mut out = text.to_string();
    for (name, value) in ordered {
        out = out.replace(&format!(":{name}"), value);
    }
    out
}

/// Choose the form matching `count`.
fn select_form(line: &str, count: i64) -> String {
    let forms: Vec<&str> = line.split('|').collect();
    if forms.len() == 1 {
        return line.to_string();
    }

    // An explicit range wins wherever one is given.
    for form in &forms {
        let trimmed = form.trim();
        if let Some((condition, text)) = split_condition(trimmed)
            && matches_condition(&condition, count) {
                return text.trim().to_string();
            }
    }

    // No ranges matched: fall back to the two-form singular/plural convention,
    // ignoring any forms that carried conditions.
    let plain: Vec<&str> = forms
        .iter()
        .filter(|form| split_condition(form.trim()).is_none())
        .copied()
        .collect();

    match plain.len() {
        0 => line.to_string(),
        1 => plain[0].trim().to_string(),
        _ => {
            if count == 1 { plain[0].trim().to_string() } else { plain[1].trim().to_string() }
        }
    }
}

/// Split `{0} none` or `[1,19] some` into its condition and text.
fn split_condition(form: &str) -> Option<(String, &str)> {
    let close = match form.chars().next()? {
        '{' => '}',
        '[' => ']',
        _ => return None,
    };
    let end = form.find(close)?;
    Some((form[1..end].to_string(), &form[end + close.len_utf8()..]))
}

fn matches_condition(condition: &str, count: i64) -> bool {
    match condition.split_once(',') {
        None => condition.trim().parse::<i64>().is_ok_and(|exact| exact == count),
        Some((low, high)) => {
            let low = low.trim().parse::<i64>().ok();
            let high = if high.trim() == "*" { Some(i64::MAX) } else { high.trim().parse().ok() };
            match (low, high) {
                (Some(low), Some(high)) => (low..=high).contains(&count),
                _ => false,
            }
        }
    }
}

/// Build a translator from the application's configuration.
pub fn from_config(config: &Config, root: &Path) -> Result<Translator> {
    let translator = Translator::new();
    translator.set_default(&config.string("app.locale", "en"));
    translator.set_fallback(&config.string("app.fallback_locale", "en"));
    translator.load_dir(root.join(config.string("app.lang_path", "lang")))?;
    Ok(translator)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translator() -> Translator {
        let translator = Translator::new();
        translator.insert(
            "en",
            Json::parse(
                r#"{
                    "welcome": {"title": "Welcome", "greeting": "Hello, :name"},
                    "orders": {"count": "{0} No orders|[1,19] :count orders|[20,*] Many orders"},
                    "items": {"count": ":count item|:count items"}
                }"#,
            )
            .unwrap(),
        );
        translator.insert(
            "id",
            Json::parse(r#"{"welcome": {"title": "Selamat datang"}}"#).unwrap(),
        );
        translator
    }

    #[test]
    fn translates_a_dotted_key() {
        assert_eq!(translator().get("welcome.title"), "Welcome");
    }

    #[test]
    fn interpolates_placeholders() {
        assert_eq!(
            translator().get_with("welcome.greeting", &[("name", "Ada")]),
            "Hello, Ada"
        );
    }

    #[test]
    fn a_missing_key_shows_itself_rather_than_vanishing() {
        assert_eq!(translator().get("nothing.here"), "nothing.here");
    }

    #[test]
    fn an_untranslated_key_falls_back_to_the_fallback_locale() {
        let translator = translator();

        assert_eq!(translator.get_in("id", "welcome.title", &[]), "Selamat datang");
        // `welcome.greeting` exists only in English.
        assert_eq!(
            translator.get_in("id", "welcome.greeting", &[("name", "Ada")]),
            "Hello, Ada"
        );
    }

    #[test]
    fn chooses_a_plural_form_by_range() {
        let translator = translator();

        assert_eq!(translator.choice("orders.count", 0, &[]), "No orders");
        assert_eq!(translator.choice("orders.count", 3, &[]), "3 orders");
        assert_eq!(translator.choice("orders.count", 50, &[]), "Many orders");
    }

    #[test]
    fn two_forms_mean_singular_and_plural() {
        let translator = translator();

        assert_eq!(translator.choice("items.count", 1, &[]), "1 item");
        assert_eq!(translator.choice("items.count", 5, &[]), "5 items");
    }

    #[test]
    fn longer_placeholders_are_replaced_first() {
        assert_eq!(
            interpolate("Hi :name_full (:name)", &[("name", "Ada"), ("name_full", "Ada Lovelace")]),
            "Hi Ada Lovelace (Ada)"
        );
    }

    #[test]
    fn loads_a_directory_of_locales() {
        let dir = std::env::temp_dir().join(format!("rustlavel-i18n-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("en.json"), r#"{"a":"A"}"#).unwrap();
        std::fs::write(dir.join("id.json"), r#"{"a":"A dalam Bahasa"}"#).unwrap();

        let translator = Translator::new();
        assert_eq!(translator.load_dir(&dir).unwrap(), 2);
        assert_eq!(translator.locales(), vec!["en", "id"]);
        assert_eq!(translator.get_in("id", "a", &[]), "A dalam Bahasa");
    }

    #[test]
    fn a_malformed_translation_file_names_itself() {
        let dir = std::env::temp_dir().join(format!("rustlavel-i18n-broken-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("en.json"), "{not json").unwrap();

        let error = Translator::new().load_dir(&dir).unwrap_err().to_string();
        assert!(error.contains("en.json"), "{error}");
    }

    #[test]
    fn a_missing_lang_directory_is_not_an_error() {
        assert_eq!(Translator::new().load_dir("/definitely/not/here").unwrap(), 0);
    }
}

/// What the `@lang` directive in a template asks for.
///
/// The template engine defines this trait and knows nothing about this crate —
/// it depends on `rustlavel-core` alone, and this crate pulls in
/// `rustlavel-http` for its locale middleware. Implementing it here is what
/// keeps the HTTP stack out of the template engine.
///
/// `get_in` rather than `get`: `get` reads a process-global default locale, and
/// a page is rendered for one reader whose language is their own.
impl rustlavel_view::Translate for Translator {
    fn line(&self, locale: &str, key: &str, replacements: &[(&str, String)]) -> String {
        let borrowed: Vec<(&str, &str)> =
            replacements.iter().map(|(name, value)| (*name, value.as_str())).collect();

        match locale.is_empty() {
            // No locale on the page: the translator's own default, which is
            // what `get_with` uses.
            true => self.get_with(key, &borrowed),
            false => self.get_in(locale, key, &borrowed),
        }
    }
}

#[cfg(test)]
mod translate_tests {
    use super::*;
    use rustlavel_view::Translate;

    fn dictionary() -> Translator {
        let translator = Translator::new();
        translator.insert(
            "en",
            Json::parse(r#"{"auth":{"sign_in":"Sign in","welcome":"Hello, :name"}}"#).unwrap(),
        );
        translator.insert(
            "id",
            Json::parse(r#"{"auth":{"sign_in":"Masuk","welcome":"Halo, :name"}}"#).unwrap(),
        );
        translator
    }

    #[test]
    fn a_line_is_read_in_the_locale_it_is_asked_for() {
        let translator = dictionary();
        assert_eq!(translator.line("en", "auth.sign_in", &[]), "Sign in");
        assert_eq!(translator.line("id", "auth.sign_in", &[]), "Masuk");
    }

    #[test]
    fn a_placeholder_is_filled() {
        let translator = dictionary();
        let filled = translator.line("id", "auth.welcome", &[("name", "Ada".to_string())]);
        assert_eq!(filled, "Halo, Ada");
    }

    /// A page that carries no locale is rendered in the default rather than
    /// failing or coming back blank.
    #[test]
    fn an_empty_locale_falls_back_to_the_default() {
        let translator = dictionary();
        translator.set_default("id");
        assert_eq!(translator.line("", "auth.sign_in", &[]), "Masuk");
    }

    /// The key shows itself, so a phrase nobody wrote is visible on the page
    /// rather than being a hole in it.
    #[test]
    fn a_missing_key_comes_back_as_itself() {
        assert_eq!(dictionary().line("en", "auth.nothing", &[]), "auth.nothing");
    }
}
