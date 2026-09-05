//! The view engine, built once and used by both the application and its tests.
//!
//! `@lang` asks the *engine* for its translator, so an engine built without
//! one renders the key instead of the word: a page that should read "Sign in"
//! reads `auth.sign_in`. That is the right behaviour for a missing phrase, and
//! the wrong thing for a test to be asserting against — a test that renders a
//! page is standing in for a visitor, and a visitor never sees a key.
//!
//! So the wiring lives here rather than in `main.rs`, where a test could not
//! reach it. `main.rs` calls this; `tests/web.rs` calls this; neither can
//! drift into building an engine the other does not have.

use rustlavel::prelude::*;

/// Load `lang/`, falling back to English for anything untranslated.
///
/// The path is a setting so a deployment can move the directory; `lang` is
/// where the kit puts it.
pub fn translator(config: &Config) -> Result<Translator> {
    let translator = Translator::new();
    translator.load_dir(config.string("app.lang_path", "lang"))?;
    translator.set_fallback("en");
    Ok(translator)
}

/// The engine `App` would have built, with somewhere for `@lang` to look.
///
/// `App::finish` builds its own only when nobody else has, so handing this to
/// `.views(...)` replaces it rather than fighting it. The *locale* is not set
/// here — it is per page, and `page::shell` puts it in the view context.
pub fn engine(config: &Config, root: &std::path::Path, translator: &Translator) -> Engine {
    rustlavel::engine_from_config(config, root)
        .with_translator(std::sync::Arc::new(translator.clone()))
}
