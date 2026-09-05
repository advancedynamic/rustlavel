//! Walking a parsed template and producing HTML.
//!
//! Rendering is deliberately hard to fail: a missing variable is an empty
//! string, and a condition on missing data is false. The only errors that can
//! escape are structural — a partial that does not exist, or one that includes
//! itself forever.

use crate::ast::Node;
use crate::context::Scope;
use crate::engine::Engine;
use crate::escape::escape;
use crate::value::{to_text, truthy};
use rustlavel_core::{Error, Json, Result};
use std::collections::HashMap;

/// How deep `@include` may nest before we call it a cycle.
const MAX_INCLUDE_DEPTH: usize = 64;

/// Renders one template tree, resolving `@yield` against the sections
/// collected from the whole `@extends` chain.
pub(crate) struct Renderer<'a> {
    engine: &'a Engine,
    sections: HashMap<&'a str, &'a [Node]>,
}

impl<'a> Renderer<'a> {
    pub(crate) fn new(engine: &'a Engine, sections: HashMap<&'a str, &'a [Node]>) -> Self {
        Renderer { engine, sections }
    }

    pub(crate) fn nodes(
        &self,
        nodes: &[Node],
        scope: &mut Scope<'_>,
        out: &mut String,
        depth: usize,
    ) -> Result<()> {
        for node in nodes {
            self.node(node, scope, out, depth)?;
        }
        Ok(())
    }

    fn node(
        &self,
        node: &Node,
        scope: &mut Scope<'_>,
        out: &mut String,
        depth: usize,
    ) -> Result<()> {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Echo { expr, escaped } => {
                let text = to_text(&expr.eval(scope));
                out.push_str(&if *escaped { escape(&text) } else { text });
            }
            Node::If { branches, otherwise } => {
                for branch in branches {
                    if truthy(&branch.condition.eval(scope)) {
                        return self.nodes(&branch.body, scope, out, depth);
                    }
                }
                if let Some(body) = otherwise {
                    self.nodes(body, scope, out, depth)?;
                }
            }
            Node::Foreach { subject, binding, body } => {
                self.foreach(subject.eval(scope), binding, body, scope, out, depth)?;
            }
            Node::Yield { section, default } => match self.sections.get(section.as_str()) {
                Some(nodes) => self.nodes(nodes, scope, out, depth)?,
                // A layout that yields a section nobody filled renders its
                // fallback, or nothing at all — never an error, because half a
                // page is more useful than none.
                None => {
                    if let Some(expr) = default {
                        out.push_str(&escape(&to_text(&expr.eval(scope))));
                    }
                }
            },
            Node::Include(reference) => {
                if depth >= MAX_INCLUDE_DEPTH {
                    return Err(reference.error(
                        "`@include` is nested too deeply — does a partial include itself?",
                    ));
                }
                let template = self.engine.load(&reference.name).map_err(|error| match error {
                    // A syntax error inside the partial already points at the
                    // partial; only a failure to find it belongs to this line.
                    Error::Template { .. } => error,
                    other => reference.error(other.to_string()),
                })?;
                if template.extends.is_some() {
                    return Err(reference.error(format!(
                        "`{}` uses `@extends`; an included partial is rendered in place and \
                         cannot extend a layout",
                        reference.name
                    )));
                }
                self.nodes(&template.nodes, scope, out, depth + 1)?;
            }
            Node::Route { name, params } => {
                let filled: Vec<(&str, String)> = params
                    .iter()
                    .map(|(parameter, expr)| (parameter.as_str(), to_text(&expr.eval(scope))))
                    .collect();

                // A name nobody registered, or a parameter the pattern needs
                // and did not get, is an error rather than an empty string.
                // `href=""` reloads the page it is on: a link that looks like a
                // link, goes nowhere, and is found by a person rather than by a
                // build. The template says which name it asked for, because by
                // the time this is read the template is what somebody is
                // looking at.
                let path = match self.engine.routes() {
                    Some(table) => table.url(name, &filled).ok_or_else(|| {
                        rustlavel_core::Error::msg(format!(
                            "`@route(\"{name}\")` names no route this application registered, or \
                             was not given every parameter the route's path needs. `rustlavel \
                             route:list` shows the names."
                        ))
                    })?,
                    None => {
                        return Err(rustlavel_core::Error::msg(format!(
                            "`@route(\"{name}\")` was rendered by an engine with no route table. \
                             `App` fills it in when it finishes the router; an engine built by \
                             hand and used outside one has to be given it."
                        )));
                    }
                };
                out.push_str(&escape(&path));
            }
            Node::Lang { key, replacements } => {
                // The locale comes from the page, not from the engine: one
                // engine serves every request, and two people reading at once
                // may be reading in different languages. `app_locale` is the
                // reserved name the application puts it under — with nothing
                // there, the translator falls back to its own default.
                let locale = to_text(&scope.lookup("app_locale").cloned().unwrap_or(Json::Null));

                let filled: Vec<(&str, String)> = replacements
                    .iter()
                    .map(|(name, expr)| (name.as_str(), to_text(&expr.eval(scope))))
                    .collect();

                let text = match self.engine.translator() {
                    Some(translator) => translator.line(&locale, key, &filled),
                    // No translator: the key, which is what a missing
                    // translation shows too. Both are visible, and both are
                    // meant to be.
                    None => key.clone(),
                };
                // Escaped, because a translation is text. A phrase that needs
                // markup around it belongs in the template with the words
                // pulled out of it, not in a language file.
                out.push_str(&escape(&text));
            }
        }
        Ok(())
    }

    /// Run a `@foreach` body once per item, with `loop` bound alongside.
    ///
    /// A subject that is not an array renders nothing. That covers the null a
    /// missing key produces, and it means an empty result set and an absent one
    /// look the same in the page — which is what a designer expects.
    fn foreach(
        &self,
        subject: Json,
        binding: &str,
        body: &[Node],
        scope: &mut Scope<'_>,
        out: &mut String,
        depth: usize,
    ) -> Result<()> {
        let Some(items) = subject.as_array() else {
            return Ok(());
        };

        let count = items.len();
        for (index, item) in items.iter().enumerate() {
            scope.push(binding, item.clone());
            scope.push("loop", loop_variable(index, count));

            let result = self.nodes(body, scope, out, depth);

            // Pop before propagating: a nested loop's bindings must not outlive
            // the loop even on the error path.
            scope.pop();
            scope.pop();
            result?;
        }
        Ok(())
    }
}

/// The `loop` variable Blade templates expect inside `@foreach`.
fn loop_variable(index: usize, count: usize) -> Json {
    Json::object([
        ("index", Json::from(index)),
        // `iteration` is the 1-based twin of `index`, so a template never has
        // to do arithmetic it cannot do.
        ("iteration", Json::from(index + 1)),
        ("first", Json::Bool(index == 0)),
        ("last", Json::Bool(index + 1 == count)),
        ("count", Json::from(count)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Context;

    /// Render source directly: none of these need to touch the disk, so the
    /// engine's root is never read.
    fn render(source: &str, context: &Context) -> String {
        Engine::default().render_source("test", source, context).unwrap()
    }

    #[test]
    fn interpolation_is_escaped_by_default() {
        let context = Context::new().with("comment", "<script>alert('xss')</script>");

        assert_eq!(
            render("<p>{{ comment }}</p>", &context),
            "<p>&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;</p>"
        );
    }

    #[test]
    fn raw_output_is_opt_in_and_untouched() {
        let context = Context::new().with("body", "<em>hi</em>");

        assert_eq!(render("{!! body !!}", &context), "<em>hi</em>");
        assert_eq!(render("{{ body }}", &context), "&lt;em&gt;hi&lt;/em&gt;");
    }

    #[test]
    fn an_unknown_variable_renders_an_empty_string() {
        assert_eq!(render("[{{ nobody }}][{{ a.b.c }}]", &Context::new()), "[][]");
    }

    #[test]
    fn comments_never_reach_the_output() {
        assert_eq!(render("a{{-- TODO: fix me --}}b", &Context::new()), "ab");
    }

    #[test]
    fn conditionals_pick_the_first_truthy_branch() {
        let template = "@if(count > 10)many@elseif(count > 0)some@else none@endif";

        assert_eq!(render(template, &Context::new().with("count", 50)), "many");
        assert_eq!(render(template, &Context::new().with("count", 1)), "some");
        assert_eq!(render(template, &Context::new().with("count", 0)), " none");
        assert_eq!(render(template, &Context::new()), " none");
    }

    #[test]
    fn a_loop_exposes_its_position() {
        let context = Context::new().with("items", vec!["a", "b", "c"]);
        let template = "@foreach(items as item){{ loop.index }}:{{ item }}\
             @if(loop.first)(first)@endif@if(loop.last)(last of {{ loop.count }})@endif @endforeach";

        assert_eq!(
            render(template, &context),
            "0:a(first) 1:b 2:c(last of 3) "
        );
    }

    #[test]
    fn an_empty_or_missing_collection_renders_nothing() {
        let empty: Vec<&str> = Vec::new();

        assert_eq!(render("@foreach(items as i)x@endforeach", &Context::new()), "");
        assert_eq!(
            render("@foreach(items as i)x@endforeach", &Context::new().with("items", empty)),
            ""
        );
    }

    #[test]
    fn nested_loops_shadow_and_then_restore_the_loop_variable() {
        let context = Context::new().with(
            "rows",
            Json::Array(vec![
                Json::from(vec!["a", "b"]),
                Json::from(vec!["c"]),
            ]),
        );
        let template = "@foreach(rows as row)\
             [{{ loop.index }}@foreach(row as cell) {{ loop.index }}{{ cell }}@endforeach\
             |{{ loop.index }}]@endforeach";

        assert_eq!(render(template, &context), "[0 0a 1b|0][1 0c|1]");
    }

    #[test]
    fn the_item_binding_shadows_a_context_variable_of_the_same_name() {
        let context =
            Context::new().with("item", "outer").with("items", vec!["inner"]);

        assert_eq!(
            render("{{ item }}@foreach(items as item){{ item }}@endforeach{{ item }}", &context),
            "outerinnerouter"
        );
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;
    use crate::context::Context;
    use std::sync::Arc;

    /// The whole point: a template names a route and gets its path.
    #[test]
    fn a_named_route_renders_its_path() {
        struct Table;
        impl crate::routes::Routes for Table {
            fn url(&self, name: &str, params: &[(&str, String)]) -> Option<String> {
                match (name, params.first()) {
                    ("dashboard", _) => Some("/dashboard".into()),
                    ("users.show", Some((_, id))) => Some(format!("/users/{id}")),
                    _ => None,
                }
            }
        }

        let engine = Engine::new(std::env::temp_dir());
        let _ = engine.routes_cell().set(Arc::new(Table));

        let context = Context::new().with("who", 7.0);
        assert_eq!(
            engine.render_source("t", r#"<a href="@route("dashboard")">go</a>"#, &context).unwrap(),
            r#"<a href="/dashboard">go</a>"#
        );
        assert_eq!(
            engine.render_source("t", r#"@route("users.show", "id", who)"#, &context).unwrap(),
            "/users/7"
        );
    }

    /// A name nobody registered must stop the page, not render `href=""` — a
    /// link that looks like a link and reloads the page it is on.
    #[test]
    fn an_unknown_route_name_is_an_error_not_an_empty_string() {
        struct Nothing;
        impl crate::routes::Routes for Nothing {
            fn url(&self, _: &str, _: &[(&str, String)]) -> Option<String> {
                None
            }
        }

        let engine = Engine::new(std::env::temp_dir());
        let _ = engine.routes_cell().set(Arc::new(Nothing));

        let error = engine
            .render_source("t", r#"@route("typo.here")"#, &Context::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("typo.here"), "{error}");
        assert!(error.contains("route:list"), "the message has to say how to find the names: {error}");
    }
}

#[cfg(test)]
mod lang_tests {
    use super::*;
    use crate::context::Context;
    use crate::translate::Translate;
    use std::sync::Arc;

    /// Two languages, so a test can tell them apart.
    struct Dictionary;

    impl Translate for Dictionary {
        fn line(&self, locale: &str, key: &str, replacements: &[(&str, String)]) -> String {
            let phrase = match (locale, key) {
                ("id", "auth.sign_in") => "Masuk",
                ("id", "auth.welcome") => "Halo, :name",
                (_, "auth.sign_in") => "Sign in",
                (_, "auth.welcome") => "Hello, :name",
                // What a real translator does with a key it does not have.
                _ => return key.to_string(),
            };
            let mut text = phrase.to_string();
            for (name, value) in replacements {
                text = text.replace(&format!(":{name}"), value);
            }
            text
        }
    }

    fn engine() -> Engine {
        Engine::default().with_translator(Arc::new(Dictionary))
    }

    fn render_in(locale: &str, source: &str) -> String {
        let context = Context::new().with("app_locale", Json::from(locale));
        engine().render_source("test", source, &context).unwrap()
    }

    #[test]
    fn a_key_is_translated_into_the_page_locale() {
        assert_eq!(render_in("en", r#"@lang("auth.sign_in")"#), "Sign in");
        assert_eq!(render_in("id", r#"@lang("auth.sign_in")"#), "Masuk");
    }

    /// **The one that matters.** A parsed template is cached and shared between
    /// requests, so a directive that resolved its words at parse time would
    /// serve the first reader's language to everybody after them. Both renders
    /// below go through the same `Engine`, and the second must not inherit the
    /// first's words.
    #[test]
    fn one_cached_template_serves_two_languages() {
        let engine = engine();
        let source = r#"@lang("auth.sign_in")"#;

        let english = Context::new().with("app_locale", Json::from("en"));
        let indonesian = Context::new().with("app_locale", Json::from("id"));

        assert_eq!(engine.render_source("shared", source, &english).unwrap(), "Sign in");
        assert_eq!(engine.render_source("shared", source, &indonesian).unwrap(), "Masuk");
        // And back, in case the second render is what poisoned it.
        assert_eq!(engine.render_source("shared", source, &english).unwrap(), "Sign in");
    }

    #[test]
    fn a_placeholder_is_filled_from_the_page() {
        let context = Context::new()
            .with("app_locale", Json::from("id"))
            .with("user", Json::object([("name", Json::from("Ada"))]));
        let out = engine()
            .render_source("test", r#"@lang("auth.welcome", "name", user.name)"#, &context)
            .unwrap();
        assert_eq!(out, "Halo, Ada");
    }

    /// A page with no locale still renders: the translator decides.
    #[test]
    fn a_page_with_no_locale_falls_back_to_the_translator() {
        let out = engine().render_source("test", r#"@lang("auth.sign_in")"#, &Context::new()).unwrap();
        assert_eq!(out, "Sign in");
    }

    /// An application that never registered a translator gets the key, which is
    /// visible and fixable. A blank button is neither.
    #[test]
    fn without_a_translator_the_key_shows_itself() {
        let out = Engine::default()
            .render_source("test", r#"@lang("auth.sign_in")"#, &Context::new())
            .unwrap();
        assert_eq!(out, "auth.sign_in");
    }

    /// Translations are text. A phrase carrying markup must not become markup.
    #[test]
    fn a_translation_is_escaped() {
        struct Hostile;
        impl Translate for Hostile {
            fn line(&self, _: &str, _: &str, _: &[(&str, String)]) -> String {
                "<script>alert(1)</script>".to_string()
            }
        }
        let out = Engine::default()
            .with_translator(Arc::new(Hostile))
            .render_source("test", r#"@lang("anything")"#, &Context::new())
            .unwrap();
        assert!(!out.contains("<script>"), "{out}");
        assert!(out.contains("&lt;script&gt;"), "{out}");
    }
}
