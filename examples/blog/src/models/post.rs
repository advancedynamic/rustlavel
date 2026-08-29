use rustlavel::prelude::*;

/// A blog post.
///
/// The struct is the schema: rename a field and the queries that use it stop
/// compiling, rather than returning null at runtime.
#[derive(Model, Default, Debug, Clone, PartialEq)]
#[model(table = "posts")]
pub struct Post {
    #[model(primary_key, generated)]
    pub id: i64,
    pub title: String,
    pub body: String,
    pub published: bool,
}

impl Post {
    /// Published posts, newest first.
    ///
    /// A scope is a plain function returning a builder — no macro, and the
    /// compiler checks the column names.
    pub fn published() -> QueryBuilder {
        Post::query().filter("published", true).latest("id")
    }
}
