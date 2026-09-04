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
//!
//! @lang("auth.sign_in")                            a translated phrase
//! @lang("mail.greeting", "name", user.name)        with a `:name` placeholder
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
//! `@lang` looks like the exception and is not. It calls nothing the template
//! chose: the key is a literal fixed at parse time, the only thing it can reach
//! is a phrase table, and the worst a wrong key can do is print itself. What
//! `@php` and calls would let a template do — read the filesystem, hit the
//! database, decide something — none of that is reachable from here. The
//! alternative is worse in the way this crate cares about: without it, every
//! translated string has to be computed in Rust and passed in, which puts the
//! wording of the page somewhere nobody looks for wording.
//!
//! The locale is not a parameter to the directive. It comes from the context
//! under the reserved name `app_locale`, because one [`Engine`] serves every
//! request and two people reading at once may be reading different languages —
//! and because a parsed template is cached and shared, so the words cannot be
//! decided until the moment the page is written. With no `app_locale` and no
//! translator, `@lang` prints its key.
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
pub mod translate;
pub mod value;

mod render;

pub use ast::{Branch, Node, Template, TemplateRef};
pub use context::{Context, Scope};
pub use translate::Translate;
pub use engine::{DEFAULT_ROOT, EXTENSION, Engine};
pub use escape::escape;
pub use expr::{BinaryOp, Expr};
pub use value::{to_text, truthy};
