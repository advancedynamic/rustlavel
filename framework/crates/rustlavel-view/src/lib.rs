//! rustlavel-view: templating in the spirit of Blade, written from scratch.
//!
//! A template is parsed once into an AST and then interpreted. That choice is
//! what makes [`Engine::with_reload`] possible: with reload on, a designer
//! edits `resources/views/pages/home.rl.html`, refreshes the browser, and sees
//! the result without a recompile — the single ergonomic advantage Blade has
//! over compiled Rust templates, and the one worth keeping.
//!
//! ```ignore
//! use rustlavel_view::{Context, Engine};
//!
//! let engine = Engine::default().with_reload(!config.is_production());
//! let html = engine.render("pages.home", &Context::new().with("title", "Home"))?;
//! ```
//!
//! # The syntax
//!
//! ```html
//! {{ user.name }}            escaped output — always, no opt-out
//! {!! post.body !!}          raw output, for HTML you already trust
//! {{-- a note --}}           stripped before rendering
//!
//! @if(user.admin) ... @elseif(user.editor) ... @else ... @endif
//! @foreach(posts as post) {{ loop.index }} {{ post.title }} @endforeach
//!
//! @extends("layouts.app")
//! @section("title", page.name)
//! @section("body") ... @endsection
//! @yield("body")
//! @include("parts.nav")
//! ```
//!
//! Inside `@foreach`, `loop` carries `index`, `iteration`, `first`, `last`, and
//! `count`. A structural directive alone on a line takes the line with it, so
//! the rendered HTML looks like HTML.
//!
//! `@include` renders a partial in place, with the scope it was included from —
//! a partial used inside a loop can read the loop's item and its `loop`
//! variable, which is what makes `@include` worth having at all.
//!
//! # What is missing on purpose
//!
//! `@php` does not exist and will not. A template that can run code stops being
//! reviewable as markup, hides logic from the compiler and from tests, and
//! turns "just a view change" into a deployment risk. Compute the value in
//! Rust, put it in the [`Context`], and render it. Templates using `@php` are
//! rejected with an error that says exactly this.
//!
//! Expressions are equally limited — paths, literals, comparisons, `&&`, `||`,
//! `!` — for the same reason. There is no arithmetic and there are no calls.
//!
//! # Escaping
//!
//! `{{ }}` escapes `&`, `<`, `>`, `"` and `'`, every time, with no way to turn
//! it off. Markup that is genuinely trusted asks for [`escape`]'s absence out
//! loud with `{!! !!}`, which makes the dangerous case the one that stands out
//! in a diff.

pub mod ast;
pub mod context;
pub mod engine;
pub mod escape;
pub mod expr;
pub mod lexer;
pub mod parser;
pub mod source;
pub mod value;

mod render;

pub use ast::{Branch, Node, Template, TemplateRef};
pub use context::{Context, Scope};
pub use engine::{DEFAULT_ROOT, EXTENSION, Engine};
pub use escape::escape;
pub use expr::{BinaryOp, Expr};
pub use value::{to_text, truthy};
