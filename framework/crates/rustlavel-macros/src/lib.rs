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
                "            ({:?}, ::rustlavel_db::Value::from(self.{}.clone())),",
                column_of(field),
                field.name
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let generated = format!(
        r#"
impl ::rustlavel_db::model::Model for {name} {{
    type Key = {primary_type};

    const TABLE: &'static str = {table:?};
    const PRIMARY_KEY: &'static str = {primary_column:?};
    const COLUMNS: &'static str = {column_list:?};

    fn from_row(row: &::rustlavel_db::Row) -> ::rustlavel_db::Result<Self> {{
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

    fn values(&self) -> ::std::vec::Vec<(&'static str, ::rustlavel_db::Value)> {{
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
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_conventional_table_names() {
        assert_eq!(pluralize(&snake_case("User")), "users");
        assert_eq!(pluralize(&snake_case("UserProfile")), "user_profiles");
        assert_eq!(pluralize(&snake_case("Category")), "categories");
        assert_eq!(pluralize(&snake_case("Address")), "addresses");
    }
}
