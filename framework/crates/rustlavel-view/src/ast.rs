//! The parsed shape of a template.

use crate::expr::Expr;
use rustlavel_core::Error;
use std::collections::BTreeMap;

/// One renderable piece of a template.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// Literal markup.
    Text(String),
    /// `{{ expr }}` when escaped, `{!! expr !!}` when not.
    Echo { expr: Expr, escaped: bool },
    /// `@if` / `@elseif` / `@else`: the first branch whose condition is truthy
    /// wins, otherwise `otherwise` runs if there is one.
    If { branches: Vec<Branch>, otherwise: Option<Vec<Node>> },
    /// `@foreach(subject as binding)`.
    Foreach { subject: Expr, binding: String, body: Vec<Node> },
    /// `@yield("name")`, with the optional fallback from `@yield("name", expr)`.
    Yield { section: String, default: Option<Expr> },
    /// `@include("partial")`.
    Include(TemplateRef),
    /// `@lang("auth.sign_in")`, and `@lang("greeting", "name", user.name)` when
    /// the phrase has placeholders.
    ///
    /// The key is resolved when the page is rendered, never when the template
    /// is parsed. A parsed template is cached and shared between requests, so
    /// baking one language's words into it would serve those words to
    /// everybody — see `Engine::translator`.
    Lang { key: String, replacements: Vec<(String, Expr)> },
    /// `@route("users.show", "id", user.id)` — the path of a named route.
    Route { name: String, params: Vec<(String, Expr)> },
}

/// One arm of an `@if` chain.
#[derive(Debug, Clone, PartialEq)]
pub struct Branch {
    pub condition: Expr,
    pub body: Vec<Node>,
}

/// A reference from one template to another.
///
/// It keeps the position it was written at, so "that layout does not exist" can
/// be reported with the same precision as a syntax error instead of surfacing
/// as a bare file-not-found from somewhere deep in the renderer.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateRef {
    /// The referenced view name, as written.
    pub name: String,
    /// The view the reference was written in.
    pub file: String,
    pub line: usize,
    pub column: usize,
}

impl TemplateRef {
    /// Blame this reference for a failure.
    pub fn error(&self, message: impl Into<String>) -> Error {
        Error::Template {
            file: self.file.clone(),
            line: self.line,
            column: self.column,
            message: message.into(),
        }
    }
}

/// A parsed template: what it renders, plus the layout wiring around it.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    /// The view name it was loaded under, e.g. `pages.home`.
    pub name: String,
    /// Everything outside a `@section`.
    pub nodes: Vec<Node>,
    /// The layout named by `@extends`, if any.
    pub extends: Option<TemplateRef>,
    /// Section bodies, keyed by name and hoisted out of `nodes`.
    ///
    /// Sections are captured rather than rendered in place: they exist to be
    /// pulled into a layout by `@yield`, so a template that defines one and is
    /// rendered on its own simply does not emit it.
    pub sections: BTreeMap<String, Vec<Node>>,
}
