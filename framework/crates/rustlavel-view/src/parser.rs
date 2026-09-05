//! Turning a token stream into a [`Template`].
//!
//! Every error a template author can make is caught here rather than at render
//! time, and every one of them carries the file, line, and column: a view is
//! edited by people who are not going to read a Rust backtrace.

use crate::ast::{Branch, Node, Template, TemplateRef};
use crate::expr::{self, Expr};
use crate::lexer::{self, Token};
use crate::source::{Span, line_column, syntax_error};
use rustlavel_core::{Error, Result};
use std::collections::BTreeMap;

/// Lex and parse `source` as the view called `name`.
pub fn parse(name: &str, source: &str) -> Result<Template> {
    let mut parser = Parser {
        tokens: lexer::tokenize(name, source)?,
        pos: 0,
        file: name,
        source,
        extends: None,
        sections: BTreeMap::new(),
    };

    // With no stop set, `block` runs to the end of the file or fails trying.
    let nodes = parser.block(&[])?;

    Ok(Template {
        name: name.to_string(),
        nodes,
        extends: parser.extends,
        sections: parser.sections,
    })
}

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    file: &'a str,
    source: &'a str,
    extends: Option<TemplateRef>,
    sections: BTreeMap<String, Vec<Node>>,
}

impl<'a> Parser<'a> {
    /// Parse nodes until one of `stop` is reached; the stopping directive is
    /// left for the caller to consume.
    fn block(&mut self, stop: &[&str]) -> Result<Vec<Node>> {
        let mut nodes = Vec::new();

        while let Some(token) = self.tokens.get(self.pos).cloned() {
            match &token {
                Token::Text(text) => {
                    self.pos += 1;
                    nodes.push(Node::Text(text.clone()));
                }
                Token::Echo { source, offset, escaped } => {
                    self.pos += 1;
                    let expr = expr::parse(self.span(*offset, source.len()))?;
                    nodes.push(Node::Echo { expr, escaped: *escaped });
                }
                Token::Directive { name, arguments, arguments_offset, offset } => {
                    if stop.contains(&name.as_str()) {
                        return Ok(nodes);
                    }
                    let span = self.span(*arguments_offset, arguments.len());
                    let offset = *offset;
                    self.pos += 1;
                    match name.as_str() {
                        "if" => nodes.push(self.conditional(span, offset)?),
                        "foreach" => nodes.push(self.loop_over(span, offset)?),
                        "section" => self.section(span, offset)?,
                        "extends" => self.extends(span, offset)?,
                        "yield" => nodes.push(self.yield_section(span)?),
                        "lang" => nodes.push(self.lang(span)?),
                        "route" => nodes.push(self.route(span)?),
                        "include" => {
                            let name = self.template_name(&span, "include")?;
                            nodes.push(Node::Include(self.reference(name, offset)));
                        }
                        // Every remaining name closes a block that was never
                        // opened — `@endif` on its own, and friends.
                        _ => return Err(self.unexpected(&token)),
                    }
                }
            }
        }

        Ok(nodes)
    }

    /// `@if(...) ... @elseif(...) ... @else ... @endif`.
    fn conditional(&mut self, span: Span<'_>, offset: usize) -> Result<Node> {
        const STOP: [&str; 3] = ["elseif", "else", "endif"];

        let mut branches =
            vec![Branch { condition: expr::parse(span)?, body: self.block(&STOP)? }];
        let mut otherwise = None;

        loop {
            match self.tokens.get(self.pos).cloned() {
                Some(Token::Directive { name, arguments, arguments_offset, .. })
                    if name == "elseif" =>
                {
                    if otherwise.is_some() {
                        return Err(self.here("`@elseif` cannot follow `@else`"));
                    }
                    let span = self.span(arguments_offset, arguments.len());
                    self.pos += 1;
                    branches.push(Branch {
                        condition: expr::parse(span)?,
                        body: self.block(&STOP)?,
                    });
                }
                Some(Token::Directive { name, .. }) if name == "else" => {
                    if otherwise.is_some() {
                        return Err(self.here("`@else` appears twice in one `@if`"));
                    }
                    self.pos += 1;
                    otherwise = Some(self.block(&STOP)?);
                }
                _ => break,
            }
        }

        self.close("if", "endif", offset)?;
        Ok(Node::If { branches, otherwise })
    }

    /// `@foreach(items as item) ... @endforeach`.
    fn loop_over(&mut self, span: Span<'_>, offset: usize) -> Result<Node> {
        let (subject, binding) = expr::parse_foreach(span)?;
        let body = self.block(&["endforeach"])?;
        self.close("foreach", "endforeach", offset)?;
        Ok(Node::Foreach { subject, binding, body })
    }

    /// `@section("name") ... @endsection`, or the one-line
    /// `@section("name", expr)` form.
    fn section(&mut self, span: Span<'_>, offset: usize) -> Result<()> {
        let arguments = expr::parse_arguments(span)?;
        let name = self.literal(arguments.first(), &span, "section")?;

        let body = match arguments.len() {
            1 => {
                let body = self.block(&["endsection"])?;
                self.close("section", "endsection", offset)?;
                body
            }
            2 => vec![Node::Echo { expr: arguments[1].clone(), escaped: true }],
            _ => {
                return Err(span.error(0, "`@section` takes a name and an optional value"));
            }
        };

        // Two sections of one name in one file is always a mistake, and a
        // silent "last one wins" is a miserable afternoon.
        if self.sections.insert(name.clone(), body).is_some() {
            return Err(syntax_error(
                self.file,
                self.source,
                offset,
                format!("`{name}` is defined by two `@section` blocks in this template"),
            ));
        }
        Ok(())
    }

    /// `@extends("layouts.app")`.
    fn extends(&mut self, span: Span<'_>, offset: usize) -> Result<()> {
        let name = self.template_name(&span, "extends")?;
        if self.extends.is_some() {
            return Err(span.error(0, "a template can only `@extends` one layout"));
        }
        self.extends = Some(self.reference(name, offset));
        Ok(())
    }

    /// `@yield("name")` or `@yield("name", "fallback")`.
    fn yield_section(&mut self, span: Span<'_>) -> Result<Node> {
        let arguments = expr::parse_arguments(span)?;
        if arguments.len() > 2 {
            return Err(span.error(0, "`@yield` takes a name and an optional fallback"));
        }
        Ok(Node::Yield {
            section: self.literal(arguments.first(), &span, "yield")?,
            default: arguments.get(1).cloned(),
        })
    }

    /// `@lang("auth.sign_in")`, or with placeholders:
    /// `@lang("auth.welcome", "name", user.name)`.
    ///
    /// Alternating name/value after the key rather than a map literal, because
    /// this expression grammar has no map literal and inventing one for a
    /// single directive is a worse trade than a convention. The names are
    /// quoted like the key; the values are ordinary expressions, so a
    /// placeholder can carry anything the page already has.
    fn lang(&mut self, span: Span<'_>) -> Result<Node> {
        let arguments = expr::parse_arguments(span)?;
        let key = self.literal(arguments.first(), &span, "lang")?;

        let rest = &arguments[1.min(arguments.len())..];
        if rest.len() % 2 != 0 {
            return Err(span.error(
                0,
                "`@lang` takes a key and then pairs of name and value, like \
                 `@lang(\"mail.greeting\", \"name\", user.name)` — one name here has no value",
            ));
        }

        let mut replacements = Vec::with_capacity(rest.len() / 2);
        for pair in rest.chunks(2) {
            let name = match &pair[0] {
                Expr::Str(name) => name.clone(),
                _ => {
                    return Err(span.error(
                        0,
                        "a `@lang` placeholder name has to be quoted, like \
                         `@lang(\"mail.greeting\", \"name\", user.name)`",
                    ))
                }
            };
            replacements.push((name, pair[1].clone()));
        }

        Ok(Node::Lang { key, replacements })
    }

    fn route(&mut self, span: Span<'_>) -> Result<Node> {
        let arguments = expr::parse_arguments(span)?;
        let name = self.literal(arguments.first(), &span, "route")?;

        let rest = &arguments[1.min(arguments.len())..];
        if rest.len() % 2 != 0 {
            return Err(span.error(
                0,
                "`@route` takes a route name and then pairs of parameter and value, like \
                 `@route(\"users.show\", \"id\", user.id)` — one parameter here has no value",
            ));
        }

        let mut params = Vec::with_capacity(rest.len() / 2);
        for pair in rest.chunks(2) {
            let name = match &pair[0] {
                Expr::Str(name) => name.clone(),
                _ => {
                    return Err(span.error(
                        0,
                        "a `@lang` placeholder name has to be quoted, like \
                         `@lang(\"mail.greeting\", \"name\", user.name)`",
                    ))
                }
            };
            params.push((name, pair[1].clone()));
        }

        Ok(Node::Route { name, params })
    }

    /// Read the single quoted view name a directive expects.
    fn template_name(&self, span: &Span<'_>, directive: &str) -> Result<String> {
        let arguments = expr::parse_arguments(*span)?;
        if arguments.len() != 1 {
            return Err(span.error(0, format!("`@{directive}` takes exactly one view name")));
        }
        self.literal(arguments.first(), span, directive)
    }

    /// Insist on a quoted literal: a view name is resolved at parse time, so it
    /// cannot come from the data.
    fn literal(&self, expr: Option<&Expr>, span: &Span<'_>, directive: &str) -> Result<String> {
        match expr {
            Some(Expr::Str(text)) => Ok(text.clone()),
            _ => Err(span.error(
                0,
                format!("`@{directive}` needs a quoted name, like `@{directive}(\"layouts.app\")`"),
            )),
        }
    }

    fn reference(&self, name: String, offset: usize) -> TemplateRef {
        let (line, column) = line_column(self.source, offset);
        TemplateRef { name, file: self.file.to_string(), line, column }
    }

    /// A fragment of the source, borrowed from it rather than from `self` so
    /// the parser can keep mutating while the span is in hand.
    fn span(&self, offset: usize, len: usize) -> Span<'a> {
        Span::new(self.file, self.source, offset, len)
    }

    /// Consume the directive that closes a block, blaming the opener when it
    /// is missing — the opener is what the author has to go fix.
    fn close(&mut self, opener: &str, closer: &str, offset: usize) -> Result<()> {
        match self.tokens.get(self.pos) {
            Some(Token::Directive { name, .. }) if name == closer => {
                self.pos += 1;
                Ok(())
            }
            _ => Err(syntax_error(
                self.file,
                self.source,
                offset,
                format!("`@{opener}` is never closed — add `@{closer}`"),
            )),
        }
    }

    /// An error at the token the parser is looking at.
    fn here(&self, message: impl Into<String>) -> Error {
        let offset = match self.tokens.get(self.pos) {
            Some(Token::Directive { offset, .. }) | Some(Token::Echo { offset, .. }) => *offset,
            _ => self.source.len(),
        };
        syntax_error(self.file, self.source, offset, message)
    }

    fn unexpected(&self, token: &Token) -> Error {
        match token {
            Token::Directive { name, offset, .. } => {
                let opener = match name.as_str() {
                    "endif" | "elseif" | "else" => "@if",
                    "endforeach" => "@foreach",
                    "endsection" => "@section",
                    _ => "a block",
                };
                syntax_error(
                    self.file,
                    self.source,
                    *offset,
                    format!("`@{name}` has no matching `{opener}`"),
                )
            }
            _ => self.here("unexpected token"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(source: &str) -> Template {
        parse("test", source).unwrap()
    }

    fn error(source: &str) -> (usize, usize, String) {
        match parse("test", source).unwrap_err() {
            Error::Template { line, column, message, .. } => (line, column, message),
            other => panic!("expected a template error, got {other:?}"),
        }
    }

    #[test]
    fn builds_an_if_chain_with_every_arm() {
        let nodes = template("@if(a)A@elseif(b)B@else C@endif").nodes;

        match &nodes[0] {
            Node::If { branches, otherwise } => {
                assert_eq!(branches.len(), 2);
                assert_eq!(branches[0].condition, Expr::Path("a".into()));
                assert_eq!(branches[1].body, vec![Node::Text("B".into())]);
                assert_eq!(otherwise.as_deref(), Some(&[Node::Text(" C".into())][..]));
            }
            other => panic!("expected an if, got {other:?}"),
        }
    }

    #[test]
    fn builds_a_foreach_with_its_binding() {
        let nodes = template("@foreach(user.posts as post){{ post.title }}@endforeach").nodes;

        match &nodes[0] {
            Node::Foreach { subject, binding, body } => {
                assert_eq!(subject, &Expr::Path("user.posts".into()));
                assert_eq!(binding, "post");
                assert_eq!(body.len(), 1);
            }
            other => panic!("expected a foreach, got {other:?}"),
        }
    }

    #[test]
    fn hoists_sections_out_of_the_node_stream() {
        let parsed = template("@extends(\"layouts.app\")\n@section(\"body\")hi@endsection");

        assert_eq!(parsed.extends.as_ref().map(|r| r.name.as_str()), Some("layouts.app"));
        assert_eq!(parsed.sections["body"], vec![Node::Text("hi".into())]);
        assert!(parsed.nodes.is_empty(), "{:?}", parsed.nodes);
    }

    #[test]
    fn the_short_section_form_echoes_its_value() {
        let parsed = template("@section(\"title\", page.name)");

        assert_eq!(
            parsed.sections["title"],
            vec![Node::Echo { expr: Expr::Path("page.name".into()), escaped: true }]
        );
    }

    #[test]
    fn records_where_a_reference_was_written() {
        let parsed = template("<p>\n  @include(\"parts.nav\")\n");

        match &parsed.nodes[1] {
            Node::Include(reference) => {
                assert_eq!(reference.name, "parts.nav");
                assert_eq!((reference.line, reference.column), (2, 3));
            }
            other => panic!("expected an include, got {other:?}"),
        }
    }

    #[test]
    fn an_unclosed_block_is_reported_at_the_line_it_opened() {
        let (line, column, message) = error("<ul>\n@if(ready)\n  <li></li>\n</ul>\n");

        assert_eq!((line, column), (2, 1));
        assert!(message.contains("`@if` is never closed"), "{message}");
    }

    #[test]
    fn a_stray_closing_directive_names_what_it_should_have_closed() {
        let (line, _, message) = error("<p>ok</p>\n@endforeach\n");

        assert_eq!(line, 2);
        assert!(message.contains("@foreach"), "{message}");
    }

    #[test]
    fn a_view_name_has_to_be_a_literal() {
        let (_, _, message) = error("@include(page)");

        assert!(message.contains("quoted name"), "{message}");
    }

    #[test]
    fn two_sections_of_one_name_are_rejected() {
        let (_, _, message) =
            error("@section(\"a\")1@endsection\n@section(\"a\")2@endsection\n");

        assert!(message.contains("two `@section`"), "{message}");
    }

    /// The key is resolved at parse time and cached with the template, so it
    /// cannot come from the data — the same rule `@include` follows.
    #[test]
    fn a_lang_key_has_to_be_a_literal() {
        let (_, _, message) = error(r#"@lang(key)"#);
        assert!(message.contains("`@lang` needs a quoted name"), "{message}");
    }

    /// A placeholder without a value is a typo that would otherwise render the
    /// name as if it were the phrase.
    #[test]
    fn a_lang_placeholder_needs_a_value() {
        let (_, _, message) = error(r#"@lang("mail.greeting", "name")"#);
        assert!(message.contains("no value"), "{message}");
    }

    #[test]
    fn a_lang_placeholder_name_has_to_be_quoted() {
        let (_, _, message) = error(r#"@lang("mail.greeting", name, user.name)"#);
        assert!(message.contains("has to be quoted"), "{message}");
    }

    #[test]
    fn a_lang_directive_keeps_its_key_and_pairs() {
        let template = template(r#"@lang("auth.welcome", "name", user.name)"#);
        match &template.nodes[0] {
            Node::Lang { key, replacements } => {
                assert_eq!(key, "auth.welcome");
                assert_eq!(replacements.len(), 1);
                assert_eq!(replacements[0].0, "name");
            }
            other => panic!("expected a Lang node, got {other:?}"),
        }
    }
}
