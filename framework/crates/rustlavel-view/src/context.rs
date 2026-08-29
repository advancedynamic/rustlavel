//! The data handed to a template, and the lookup rules on top of it.

use rustlavel_core::Json;
use std::collections::BTreeMap;

/// The variables a single render can see.
///
/// Not to be confused with `rustlavel_core::Context`, which is the
/// application's long-lived typed state. This one is per-render, cheap, and
/// intentionally untyped: a view is the one place where the shape of the data
/// is decided by whoever is editing the HTML.
///
/// ```
/// use rustlavel_core::Json;
/// use rustlavel_view::Context;
///
/// let context = Context::new()
///     .with("title", "Dashboard")
///     .with("user", Json::object([("name", Json::from("Ada"))]));
///
/// assert_eq!(context.get("title").unwrap(), &Json::from("Dashboard"));
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Context {
    data: BTreeMap<String, Json>,
}

impl Context {
    pub fn new() -> Self {
        Context::default()
    }

    /// Add a variable, chaining: `Context::new().with("user", user)`.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Json>) -> Self {
        self.insert(key, value);
        self
    }

    /// Add a variable to an existing context.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Json>) {
        self.data.insert(key.into(), value.into());
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        self.data.get(key)
    }

    /// Turn a JSON object into a context, so a handler that already built one
    /// does not have to take it apart again. Anything else yields an empty
    /// context — a template's top level is a set of named variables.
    pub fn from_json(value: Json) -> Self {
        match value {
            Json::Object(map) => Context { data: map.into_iter().collect() },
            _ => Context::default(),
        }
    }
}

/// The context plus the local bindings `@foreach` introduces.
///
/// Locals are a stack rather than a map because shadowing is the behaviour
/// nested loops need: an inner `loop` (or a rebound item) must hide the outer
/// one for exactly as long as its body runs, then reveal it again.
pub struct Scope<'a> {
    context: &'a Context,
    locals: Vec<(String, Json)>,
}

impl<'a> Scope<'a> {
    pub fn new(context: &'a Context) -> Self {
        Scope { context, locals: Vec::new() }
    }

    /// Bind a name for the duration of a block.
    pub fn push(&mut self, name: impl Into<String>, value: Json) {
        self.locals.push((name.into(), value));
    }

    /// Drop the most recent binding.
    pub fn pop(&mut self) {
        self.locals.pop();
    }

    /// Resolve a dotted path such as `user.address.city` or `items.0.title`.
    ///
    /// Only the first segment can be a local; the rest walks the value itself,
    /// which is why array indices work the same way they do in `Json::get`.
    pub fn lookup(&self, path: &str) -> Option<&Json> {
        let (root, rest) = match path.split_once('.') {
            Some((root, rest)) => (root, Some(rest)),
            None => (path, None),
        };

        let value = self
            .locals
            .iter()
            .rev()
            .find(|(name, _)| name == root)
            .map(|(_, value)| value)
            .or_else(|| self.context.get(root))?;

        match rest {
            Some(rest) => value.get(rest),
            None => Some(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_dotted_paths_into_nested_data() {
        let context = Context::new().with(
            "user",
            Json::object([
                ("name", Json::from("Ada")),
                ("tags", Json::from(vec!["math", "engines"])),
            ]),
        );
        let scope = Scope::new(&context);

        assert_eq!(scope.lookup("user.name"), Some(&Json::from("Ada")));
        assert_eq!(scope.lookup("user.tags.1"), Some(&Json::from("engines")));
        assert!(scope.lookup("user.missing").is_none());
        assert!(scope.lookup("nobody").is_none());
    }

    #[test]
    fn a_local_shadows_the_context_until_it_is_popped() {
        let context = Context::new().with("item", "outer");
        let mut scope = Scope::new(&context);

        scope.push("item", Json::from("inner"));
        assert_eq!(scope.lookup("item"), Some(&Json::from("inner")));

        scope.pop();
        assert_eq!(scope.lookup("item"), Some(&Json::from("outer")));
    }

    #[test]
    fn a_json_object_becomes_the_whole_context() {
        let context = Context::from_json(Json::object([("count", Json::from(3))]));

        assert_eq!(context.get("count"), Some(&Json::from(3)));
        assert_eq!(Context::from_json(Json::from(1)), Context::new());
    }
}
