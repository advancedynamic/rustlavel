//! Loading, caching, and rendering the templates in `resources/views`.

use crate::ast::{Node, Template};
use crate::context::{Context, Scope};
use crate::parser;
use crate::render::Renderer;
use crate::translate::Translate;
use rustlavel_core::{Error, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Where views live by default, mirroring Laravel's layout.
pub const DEFAULT_ROOT: &str = "resources/views";

/// The file extension a view carries.
///
/// The double extension is deliberate: `home.rl.html` is still an `.html` file
/// to an editor, so a designer keeps syntax highlighting, formatting, and
/// emmet while working on a template.
pub const EXTENSION: &str = "rl.html";

/// How many layouts an `@extends` chain may stack.
const MAX_LAYOUT_DEPTH: usize = 16;

/// Loads templates from a directory and renders them.
///
/// One engine is built at boot and shared; `render` takes `&self` so handlers
/// can render concurrently.
pub struct Engine {
    root: PathBuf,
    reload: bool,
    /// Parsed templates, keyed by view name.
    cache: RwLock<HashMap<String, Arc<Template>>>,
    /// Where `@lang` gets its words. Application-wide, like the engine itself;
    /// the *locale* is per page and travels in the context instead.
    translator: Option<Arc<dyn Translate>>,
}

impl Engine {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Engine {
            root: root.into(),
            reload: false,
            cache: RwLock::new(HashMap::new()),
            translator: None,
        }
    }

    /// Re-read every template from disk on every render.
    ///
    /// This is the whole reason the engine interprets an AST instead of
    /// generating Rust: with reload on, a designer edits a template, hits
    /// refresh, and sees the change — no recompile, no restart. Leave it off in
    /// production, where the parse should happen once.
    pub fn with_reload(mut self, reload: bool) -> Self {
        self.reload = reload;
        self
    }

    /// Give `@lang` somewhere to look.
    ///
    /// Without one the directive renders the key it was given, which is the
    /// same thing a missing translation does — a page saying `auth.sign_in` is
    /// a page somebody fixes.
    pub fn with_translator(mut self, translator: Arc<dyn Translate>) -> Self {
        self.translator = Some(translator);
        self
    }

    /// The translator, for the renderer.
    pub(crate) fn translator(&self) -> Option<&dyn Translate> {
        self.translator.as_deref()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn reloads(&self) -> bool {
        self.reload
    }

    /// Whether a view exists on disk.
    pub fn exists(&self, name: &str) -> bool {
        self.path_of(name).is_ok_and(|path| path.is_file())
    }

    /// Forget every parsed template.
    pub fn clear_cache(&self) {
        if let Ok(mut cache) = self.cache.write() {
            cache.clear();
        }
    }

    /// Render the view `name` — `"pages.home"` or `"pages/home"` — with
    /// `context` as its data.
    pub fn render(&self, name: &str, context: &Context) -> Result<String> {
        self.render_template(self.load(name)?, context)
    }

    /// Render a template held in memory, still resolving `@include` and
    /// `@extends` through this engine's root.
    ///
    /// Useful for a mail body assembled in code, and for tests that have no
    /// reason to touch the disk.
    pub fn render_source(&self, name: &str, source: &str, context: &Context) -> Result<String> {
        self.render_template(Arc::new(parser::parse(name, source)?), context)
    }

    /// Parse a view, going through the cache unless reload mode is on.
    pub(crate) fn load(&self, name: &str) -> Result<Arc<Template>> {
        // A poisoned lock is not worth failing a page over: parse again.
        if !self.reload
            && let Ok(cache) = self.cache.read()
            && let Some(template) = cache.get(name)
        {
            return Ok(Arc::clone(template));
        }

        let path = self.path_of(name)?;
        let source = std::fs::read_to_string(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::msg(format!("view `{name}` not found — expected `{}`", path.display()))
            } else {
                Error::Io(error)
            }
        })?;

        let template = Arc::new(parser::parse(name, &source)?);
        if !self.reload
            && let Ok(mut cache) = self.cache.write()
        {
            cache.insert(name.to_string(), Arc::clone(&template));
        }
        Ok(template)
    }

    /// Map a view name onto a file, refusing anything that could leave the
    /// view root — a name can reach this from a route parameter.
    fn path_of(&self, name: &str) -> Result<PathBuf> {
        let looks_wrong = name.is_empty()
            || name.contains('\\')
            || name.split(['.', '/']).any(|segment| segment.is_empty() || segment == "..");
        if looks_wrong {
            return Err(Error::msg(format!(
                "`{name}` is not a valid view name — use dots or slashes, like `pages.home`"
            )));
        }
        Ok(self.root.join(format!("{}.{EXTENSION}", name.replace('.', "/"))))
    }

    /// Render `template`, pulling its sections up through its layouts.
    fn render_template(&self, template: Arc<Template>, context: &Context) -> Result<String> {
        let chain = self.layout_chain(template)?;

        // The child is walked first so its sections win; a section defined in a
        // layout is only a default for the pages that do not override it.
        let mut sections: HashMap<&str, &[Node]> = HashMap::new();
        for template in &chain {
            for (name, nodes) in &template.sections {
                sections.entry(name.as_str()).or_insert(nodes.as_slice());
            }
        }

        // Only the outermost layout is rendered: everything a child contributes
        // reaches the page through `@yield`.
        let layout = chain.last().expect("a chain always holds the template it started from");
        let mut out = String::new();
        let mut scope = Scope::new(context);
        Renderer::new(self, sections).nodes(&layout.nodes, &mut scope, &mut out, 0)?;
        Ok(out)
    }

    /// Follow `@extends` from a template up to its outermost layout.
    fn layout_chain(&self, template: Arc<Template>) -> Result<Vec<Arc<Template>>> {
        let mut chain = vec![template];

        while let Some(parent) = chain[chain.len() - 1].extends.clone() {
            if chain.iter().any(|template| template.name == parent.name) {
                return Err(parent.error(format!(
                    "`{}` is already in this `@extends` chain — layouts cannot loop",
                    parent.name
                )));
            }
            if chain.len() >= MAX_LAYOUT_DEPTH {
                return Err(parent.error(format!(
                    "layouts are stacked more than {MAX_LAYOUT_DEPTH} deep — flatten them"
                )));
            }
            let layout = self.load(&parent.name).map_err(|error| match error {
                Error::Template { .. } => error,
                other => parent.error(other.to_string()),
            })?;
            chain.push(layout);
        }

        Ok(chain)
    }
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new(DEFAULT_ROOT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_core::Json;

    /// Each test writes its own directory: tests run concurrently, and a shared
    /// fixture would be rewritten underneath a test that is reading it.
    fn fixture(test: &str, views: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rustlavel-view-{test}"));
        let _ = std::fs::remove_dir_all(&root);
        for (name, source) in views {
            let path = root.join(format!("{}.{EXTENSION}", name.replace('.', "/")));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }
        root
    }

    #[test]
    fn renders_a_view_from_disk() {
        let engine = Engine::new(fixture("disk", &[("pages.home", "<h1>{{ title }}</h1>")]));
        let context = Context::new().with("title", "Home & away");

        assert_eq!(
            engine.render("pages.home", &context).unwrap(),
            "<h1>Home &amp; away</h1>"
        );
        // Slashes and dots name the same file.
        assert_eq!(
            engine.render("pages/home", &context).unwrap(),
            "<h1>Home &amp; away</h1>"
        );
    }

    #[test]
    fn a_layout_receives_the_sections_a_page_defines() {
        let root = fixture(
            "layout",
            &[
                (
                    "layouts.app",
                    "<title>@yield(\"title\", \"Rustlavel\")</title>\n<main>@yield(\"body\")</main>\n",
                ),
                (
                    "pages.about",
                    "@extends(\"layouts.app\")\n\
                     @section(\"title\", heading)\n\
                     @section(\"body\")\n\
                     <p>{{ heading }}</p>\n\
                     @endsection\n",
                ),
            ],
        );
        let engine = Engine::new(root);
        let context = Context::new().with("heading", "About");

        assert_eq!(
            engine.render("pages.about", &context).unwrap(),
            "<title>About</title>\n<main><p>About</p>\n</main>\n"
        );
    }

    #[test]
    fn a_yield_with_no_section_falls_back_or_stays_empty() {
        let root = fixture(
            "yield-default",
            &[
                ("layouts.app", "[@yield(\"title\", \"Untitled\")][@yield(\"body\")]"),
                ("pages.bare", "@extends(\"layouts.app\")"),
            ],
        );

        assert_eq!(
            Engine::new(root).render("pages.bare", &Context::new()).unwrap(),
            "[Untitled][]"
        );
    }

    #[test]
    fn layouts_can_be_stacked_and_the_innermost_page_wins() {
        let root = fixture(
            "nested-layout",
            &[
                ("layouts.base", "<html>@yield(\"body\")</html>"),
                (
                    "layouts.admin",
                    "@extends(\"layouts.base\")@section(\"body\")<nav></nav>@yield(\"panel\")@endsection",
                ),
                (
                    "pages.dashboard",
                    "@extends(\"layouts.admin\")@section(\"panel\")<p>hi</p>@endsection",
                ),
            ],
        );

        assert_eq!(
            Engine::new(root).render("pages.dashboard", &Context::new()).unwrap(),
            "<html><nav></nav><p>hi</p></html>"
        );
    }

    #[test]
    fn an_include_renders_a_partial_with_the_surrounding_scope() {
        let root = fixture(
            "include",
            &[
                ("pages.list", "@foreach(users as user)@include(\"parts.user\")@endforeach"),
                ("parts.user", "<li>{{ user.name }} ({{ loop.iteration }})</li>"),
            ],
        );
        let context = Context::new().with(
            "users",
            Json::Array(vec![
                Json::object([("name", Json::from("Ada"))]),
                Json::object([("name", Json::from("Grace"))]),
            ]),
        );

        assert_eq!(
            Engine::new(root).render("pages.list", &context).unwrap(),
            "<li>Ada (1)</li><li>Grace (2)</li>"
        );
    }

    #[test]
    fn a_missing_view_is_reported_by_name() {
        let engine = Engine::new(fixture("missing", &[]));
        let message = engine.render("pages.nope", &Context::new()).unwrap_err().to_string();

        assert!(message.contains("pages.nope"), "{message}");
        assert!(message.contains("pages/nope.rl.html"), "{message}");
    }

    #[test]
    fn a_missing_partial_is_blamed_on_the_line_that_included_it() {
        let root = fixture("missing-partial", &[("pages.home", "<p>ok</p>\n@include(\"gone\")\n")]);

        match Engine::new(root).render("pages.home", &Context::new()).unwrap_err() {
            Error::Template { file, line, message, .. } => {
                assert_eq!((file.as_str(), line), ("pages.home", 2));
                assert!(message.contains("gone"), "{message}");
            }
            other => panic!("expected a template error, got {other:?}"),
        }
    }

    #[test]
    fn a_syntax_error_names_the_view_and_the_line_it_is_on() {
        let root = fixture(
            "syntax",
            &[("pages.broken", "<ul>\n  <li>ok</li>\n  @foreach(items as item)\n</ul>\n")],
        );

        match Engine::new(root).render("pages.broken", &Context::new()).unwrap_err() {
            Error::Template { file, line, column, message } => {
                assert_eq!((file.as_str(), line, column), ("pages.broken", 3, 3));
                assert!(message.contains("@endforeach"), "{message}");
            }
            other => panic!("expected a template error, got {other:?}"),
        }
    }

    #[test]
    fn a_view_name_cannot_escape_the_view_root() {
        let engine = Engine::new(fixture("traversal", &[]));

        for name in ["../secrets", "pages..home", "", "pages\\home"] {
            assert!(engine.render(name, &Context::new()).is_err(), "`{name}` should not resolve");
        }
    }

    #[test]
    fn a_cached_template_survives_the_file_changing_until_reload_is_on() {
        let root = fixture("cache", &[("pages.home", "first")]);
        let path = root.join(format!("pages/home.{EXTENSION}"));

        let cached = Engine::new(&root);
        assert_eq!(cached.render("pages.home", &Context::new()).unwrap(), "first");
        std::fs::write(&path, "second").unwrap();
        assert_eq!(cached.render("pages.home", &Context::new()).unwrap(), "first");
        cached.clear_cache();
        assert_eq!(cached.render("pages.home", &Context::new()).unwrap(), "second");

        // With reload on, the edit shows up on the next request — no restart,
        // which is the point of the mode.
        let live = Engine::new(&root).with_reload(true);
        assert_eq!(live.render("pages.home", &Context::new()).unwrap(), "second");
        std::fs::write(&path, "third").unwrap();
        assert_eq!(live.render("pages.home", &Context::new()).unwrap(), "third");
    }

    #[test]
    fn a_layout_loop_is_caught_instead_of_hanging() {
        let root = fixture(
            "loop",
            &[
                ("a", "@extends(\"b\")"),
                ("b", "@extends(\"a\")"),
            ],
        );

        let message = Engine::new(root).render("a", &Context::new()).unwrap_err().to_string();
        assert!(message.contains("cannot loop"), "{message}");
    }

    #[test]
    fn exists_reports_what_is_on_disk() {
        let engine = Engine::new(fixture("exists", &[("pages.home", "hi")]));

        assert!(engine.exists("pages.home"));
        assert!(!engine.exists("pages.gone"));
        assert!(!engine.exists("../etc/passwd"));
    }
}
