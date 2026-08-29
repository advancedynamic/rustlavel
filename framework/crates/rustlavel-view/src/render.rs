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
