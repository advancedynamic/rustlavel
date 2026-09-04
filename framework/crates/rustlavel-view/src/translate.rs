//! What the `@lang` directive asks for a word.
//!
//! A trait here rather than a dependency on `rustlavel-i18n`, and the reason is
//! the dependency graph: this crate depends on `rustlavel-core` alone, while
//! the i18n crate depends on `rustlavel-http` for its locale-detection
//! middleware. Depending on it from here would drag the HTTP stack into the
//! template engine for the sake of one lookup.
//!
//! `rustlavel-i18n` implements this for `Translator`, so an application hands
//! the engine its translator and nothing in between has to know what a
//! translator is.

/// Somewhere words come from.
pub trait Translate: Send + Sync {
    /// The phrase `key` in `locale`, with `:name` placeholders filled in.
    ///
    /// A key with no translation should come back as the key itself rather
    /// than as an empty string: a page reading `auth.sign_in` is a page
    /// somebody fixes, and a page with a blank button is one nobody notices.
    fn line(&self, locale: &str, key: &str, replacements: &[(&str, String)]) -> String;
}
