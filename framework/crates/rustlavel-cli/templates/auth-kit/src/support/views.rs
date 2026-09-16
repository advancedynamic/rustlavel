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
use rustlavel::{Plugin, Setup};

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

/// The engine above, plus the route table `@route(…)` resolves against.
///
/// **For tests, and only for tests.** `App` fills the table from the router it
/// actually serves, and a route added in `main.rs` and nowhere else is in
/// *that* table — so `main.rs` must keep using [`engine`] and let `App` do it.
/// A test that renders a view without a request has no `App`, so this replays
/// the same registration `main.rs` performs: the two route files and every
/// module. Once the layouts used `@route`, every such test failed with "an
/// engine with no route table"; this is what gives them one.
///
/// If a name a template uses is missing here, the test fails loudly naming it,
/// which is the right outcome: it means `main.rs` registers something this
/// list does not, and the two have drifted.
pub fn engine_for_tests(config: &Config, root: &std::path::Path, translator: &Translator) -> Engine {
    let mut router = Router::new();
    crate::routes::auth::routes(&mut router);
    crate::routes::web::routes(&mut router);

    // Modules register through `Plugin::register`, which wants a `Setup`. A
    // builder nobody reads is enough for the routes to land.
    let mut context = Some(rustlavel::Context::builder());
    for module in crate::modules::all() {
        let plugin: Box<dyn Plugin> = module;
        plugin.register(&mut Setup { router: &mut router, config, context: &mut context });
    }

    rustlavel::engine_with_routes(engine(config, root, translator), &mut router)
}
