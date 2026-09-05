//! What the `@route` directive asks for a URL.
//!
//! A trait here for the same reason [`Translate`](crate::translate::Translate)
//! is one: this crate depends on `rustlavel-core` alone, and the router lives
//! in `rustlavel-http`. Depending on it from the template engine would drag the
//! HTTP stack in for the sake of one lookup.
//!
//! The `rustlavel` crate implements this over the finished router, so an
//! application names a route once and every template can reach it.
//!
//! **The point of it.** Before this, the kit declared thirty named routes and
//! hard-coded fifty-four paths in its templates: the names were used by
//! `route:list` and nothing else, and moving a route meant a search-and-replace
//! that announced its misses by 404 rather than by failing to build.

/// Somewhere URLs come from.
pub trait Routes: Send + Sync {
    /// The path for a named route, with its parameters filled in.
    ///
    /// `None` when the name is unknown or a parameter the pattern needs was not
    /// given. The renderer turns that into an error rather than an empty
    /// string: `href=""` is a link that looks like a link and goes nowhere,
    /// which is the kind of breakage that reaches a person before it reaches a
    /// developer.
    fn url(&self, name: &str, params: &[(&str, String)]) -> Option<String>;
}
