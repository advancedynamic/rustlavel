//! Derive macros for Rustlavel.
//!
//! `#[derive(Model)]` is the Eloquent-shaped half of the ORM: it turns a plain
//! struct into something that knows its table, can be read from a row, and can
//! be saved back — without the runtime reflection Laravel relies on.

mod parse;

use parse::{Attributes, Field, Struct};
use proc_macro::TokenStream;

/// Derive `Model` for a struct with named fields.
///
/// ```ignore
/// #[derive(Model)]
/// #[model(table = "users")]
/// pub struct User {
///     #[model(primary_key, generated)]
///     pub id: i64,
///     pub name: String,
///     pub email: String,
///     #[model(skip)]
///     pub transient: Option<String>,
/// }
/// ```
///
/// Defaults follow Laravel's conventions: the table is the pluralised,
/// snake_cased struct name, and a field called `id` is the primary key.
///
/// The generated code reaches the database package through `::rustlavel::db`,
/// since that is the dependency an application declares. A crate depending on
/// `rustlavel-db` directly overrides it with `#[model(crate = "rustlavel_db")]`.
#[proc_macro_derive(Model, attributes(model))]
pub fn derive_model(input: TokenStream) -> TokenStream {
    match parse::parse_struct(input).and_then(expand) {
        Ok(tokens) => tokens,
        // A derive cannot return a Result, so the error becomes a compile error
        // pointing at the real problem instead of a wall of trait bounds.
        Err(message) => format!("::core::compile_error!({:?});", message).parse().unwrap(),
    }
}

fn expand(parsed: Struct) -> Result<TokenStream, String> {
    let Struct { name, fields, attributes } = parsed;

    let stored: Vec<&Field> = fields.iter().filter(|f| !f.attributes.skip).collect();
    if stored.is_empty() {
        return Err(format!("`{name}` has no persisted fields"));
    }

    let table = attributes.table.clone().unwrap_or_else(|| pluralize(&snake_case(&name)));

    // Applications depend on the meta-crate, not on the database package
    // directly, so that is the path the generated code takes. A crate using
    // rustlavel-db on its own says so with `#[model(crate = "rustlavel_db")]`.
    let db = attributes.krate.clone().unwrap_or_else(|| "::rustlavel::db".to_string());

    let primary = stored
        .iter()
        .find(|f| f.attributes.primary_key)
        .or_else(|| stored.iter().find(|f| column_of(f) == "id"))
        .ok_or_else(|| {
            format!(
                "`{name}` has no primary key. Add a field called `id`, or mark one \
                 `#[model(primary_key)]`."
            )
        })?;
    let primary_column = column_of(primary);
    let primary_field = primary.name.clone();
    let primary_type = primary.type_text.clone();

    // Columns the database fills in are read back but never written.
    let writable: Vec<&&Field> = stored
        .iter()
        .filter(|f| !f.attributes.generated && !f.attributes.primary_key && column_of(f) != "id")
        .filter(|f| !matches!(column_of(f).as_str(), "created_at" | "updated_at"))
        .collect();

    let column_list = stored
        .iter()
        .map(|f| format!("\"{}\"", column_of(f)))
        .collect::<Vec<_>>()
        .join(", ");

    // Only fall back to `Default` when some fields are skipped; emitting it
    // unconditionally makes clippy complain at every derive site.
    let rest = if stored.len() == fields.len() {
        String::new()
    } else {
        "            ..::core::default::Default::default()\n".to_string()
    };

    let from_row = stored
        .iter()
        .map(|field| {
            format!(
                "            {}: row.get({:?})?,",
                field.name,
                column_of(field)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let to_values = writable
        .iter()
        .map(|field| {
            format!(
                "            ({:?}, {db}::Value::from(self.{}.clone())),",
                column_of(field),
                field.name
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let generated = format!(
        r#"
impl {db}::model::Model for {name} {{
    type Key = {primary_type};

    const TABLE: &'static str = {table:?};
    const PRIMARY_KEY: &'static str = {primary_column:?};
    const COLUMNS: &'static str = {column_list:?};

    fn from_row(row: &{db}::Row) -> {db}::Result<Self> {{
        Ok({name} {{
{from_row}
{rest}        }})
    }}

    fn key(&self) -> Self::Key {{
        self.{primary_field}.clone()
    }}

    fn set_key(&mut self, key: Self::Key) {{
        self.{primary_field} = key;
    }}

    fn values(&self) -> ::std::vec::Vec<(&'static str, {db}::Value)> {{
        ::std::vec![
{to_values}
        ]
    }}
}}
"#
    );

    generated.parse().map_err(|e| format!("generated code did not parse: {e}"))
}

/// The column a field maps to: `#[model(column = "...")]`, else the field name.
fn column_of(field: &Field) -> String {
    field.attributes.column.clone().unwrap_or_else(|| field.name.clone())
}

fn snake_case(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 4);
    for (index, ch) in input.chars().enumerate() {
        if ch.is_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn pluralize(word: &str) -> String {
    if word.ends_with('s') && !word.ends_with("us") && !word.ends_with("ss") {
        return word.to_string();
    }
    if let Some(stem) = word.strip_suffix('y')
        && !stem.ends_with(['a', 'e', 'i', 'o', 'u']) {
            return format!("{stem}ies");
        }
    if word.ends_with(['s', 'x', 'z']) || word.ends_with("ch") || word.ends_with("sh") {
        return format!("{word}es");
    }
    format!("{word}s")
}

// Silence the unused warning for a field the expansion reads through helpers.
const _: Option<&Attributes> = None;

// The parser and helpers are unit-tested here; the generated code is exercised
// by integration tests in rustlavel-db, which is the only place it can compile.


/// `#[rustlavel::main]` — start the application without naming a runtime.
///
/// Async Rust needs a runtime, and the standard library ships none: `async fn`
/// compiles to a state machine that nothing in `std` will drive. So one has to
/// exist, and here it is Tokio.
///
/// What a person using this framework should not have to do is *know* that.
/// Adding `rustlavel` and then being told to add a second, unrelated-looking
/// crate before anything compiles is the kind of paper cut that makes a
/// framework feel like a pile of parts. This attribute and the `rustlavel::tokio`
/// re-export exist so the runtime is a detail of the framework rather than a
/// step in the reader's setup.
///
/// ```ignore
/// use rustlavel::prelude::*;
///
/// #[rustlavel::main]
/// async fn main() -> Result<()> {
///     App::new()?.routes(routes::web::routes).run().await
/// }
/// ```
///
/// `#[tokio::main]` still works for anyone who wants the runtime in their own
/// hands — this replaces nothing, it only removes a required step.
#[proc_macro_attribute]
pub fn main(_attributes: TokenStream, item: TokenStream) -> TokenStream {
    runtime_wrapper(item, "main")
}

/// `#[rustlavel::test]` — the same, for an async test.
#[proc_macro_attribute]
pub fn test(_attributes: TokenStream, item: TokenStream) -> TokenStream {
    runtime_wrapper(item, "test")
}

/// Rewrite `async fn name(..) -> T { body }` as a blocking `fn` that builds a
/// runtime and blocks on the body.
///
/// Written by hand against the token stream, like everything else in this
/// crate: no syn, no quote. The shape being rewritten is narrow — an `async fn`
/// with no arguments — so the parsing can be too, and anything that does not
/// match gets a message naming what was expected rather than a torrent of
/// errors from further down the compile.
fn runtime_wrapper(item: TokenStream, which: &str) -> TokenStream {
    let source = item.to_string();

    let Some(async_at) = source.find("async fn ") else {
        return compile_error(&format!(
            "#[rustlavel::{which}] goes on an `async fn`. This one is not async — if it does \
             not await anything, it does not need the attribute."
        ));
    };

    // Everything before `async` is attributes and visibility, and has to be
    // kept: `#[allow(...)]` or `pub` above this attribute is still the user's.
    let leading = &source[..async_at];
    let rest = &source[async_at + "async ".len()..];

    let Some(brace_at) = rest.find('{') else {
        return compile_error(&format!(
            "#[rustlavel::{which}] needs a function with a body."
        ));
    };
    let signature = &rest[..brace_at];
    let body = &rest[brace_at..];

    let test_attribute = if which == "test" { "#[::core::prelude::v1::test]" } else { "" };

    format!(
        "{test_attribute}{leading}{signature}{{
            ::rustlavel::tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect(\"the async runtime could not be started\")
                .block_on(async {body})
        }}"
    )
    .parse()
    .unwrap_or_else(|error| {
        compile_error(&format!("#[rustlavel::{which}] produced code that will not parse: {error}"))
    })
}

fn compile_error(message: &str) -> TokenStream {
    format!("::core::compile_error!({:?});", message).parse().expect("a literal parses")
}

#[cfg(test)]
mod tests {
    // Named imports rather than a glob: this crate now exports an attribute
    // called `test`, and a glob would shadow the one that makes these run.
    use super::{pluralize, snake_case};

    #[test]
    fn derives_conventional_table_names() {
        assert_eq!(pluralize(&snake_case("User")), "users");
        assert_eq!(pluralize(&snake_case("UserProfile")), "user_profiles");
        assert_eq!(pluralize(&snake_case("Category")), "categories");
        assert_eq!(pluralize(&snake_case("Address")), "addresses");
    }
}
