//! A small hand-written parser for the subset of Rust a derive needs to see.
//!
//! `syn` would be one line of Cargo.toml, but it is also a large dependency in
//! every application's build graph. A derive only needs the struct name, its
//! fields, and the `#[model(...)]` attributes on them, which is a few hundred
//! lines of token walking.

use proc_macro::{Delimiter, TokenStream, TokenTree};

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    /// The type as written. Kept for future use — a nullable column is
    /// recognisable as `Option<T>`.
    #[allow(dead_code)]
    pub type_text: String,
    pub attributes: Attributes,
}

/// Everything `#[model(...)]` can say.
#[derive(Debug, Clone, Default)]
pub struct Attributes {
    pub table: Option<String>,
    pub column: Option<String>,
    pub primary_key: bool,
    pub skip: bool,
    /// The path the generated code reaches the database package through.
    pub krate: Option<String>,
    /// Managed by the database; excluded from inserts and updates.
    pub generated: bool,
}

#[derive(Debug)]
pub struct Struct {
    pub name: String,
    pub fields: Vec<Field>,
    pub attributes: Attributes,
}

/// Parse `#[derive(Model)] #[model(table = "users")] struct User { ... }`.
pub fn parse_struct(input: TokenStream) -> Result<Struct, String> {
    let tokens: Vec<TokenTree> = input.into_iter().collect();
    let mut index = 0;

    let mut container_attributes = Attributes::default();

    // Outer attributes come first; only ours are interesting.
    while index < tokens.len() {
        match &tokens[index] {
            TokenTree::Punct(punct) if punct.as_char() == '#' => {
                let Some(TokenTree::Group(group)) = tokens.get(index + 1) else {
                    return Err("malformed attribute".into());
                };
                merge_attributes(&mut container_attributes, group.stream())?;
                index += 2;
            }
            TokenTree::Ident(ident) if ident.to_string() == "struct" => break,
            // Visibility and anything else before `struct`.
            _ => index += 1,
        }
    }

    // `struct`
    match tokens.get(index) {
        Some(TokenTree::Ident(ident)) if ident.to_string() == "struct" => index += 1,
        _ => return Err("#[derive(Model)] can only be used on a struct".into()),
    }

    let name = match tokens.get(index) {
        Some(TokenTree::Ident(ident)) => ident.to_string(),
        _ => return Err("expected a struct name".into()),
    };
    index += 1;

    if matches!(tokens.get(index), Some(TokenTree::Punct(p)) if p.as_char() == '<') {
        return Err("#[derive(Model)] does not support generic structs".into());
    }

    let body = match tokens.get(index) {
        Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Brace => group,
        _ => return Err("#[derive(Model)] needs a struct with named fields".into()),
    };

    Ok(Struct { name, fields: parse_fields(body.stream())?, attributes: container_attributes })
}

fn parse_fields(stream: TokenStream) -> Result<Vec<Field>, String> {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    let mut fields = Vec::new();
    let mut index = 0;

    while index < tokens.len() {
        let mut attributes = Attributes::default();

        // Field attributes.
        while matches!(tokens.get(index), Some(TokenTree::Punct(p)) if p.as_char() == '#') {
            let Some(TokenTree::Group(group)) = tokens.get(index + 1) else {
                return Err("malformed field attribute".into());
            };
            merge_attributes(&mut attributes, group.stream())?;
            index += 2;
        }

        // Visibility: `pub`, or `pub(crate)`.
        if matches!(tokens.get(index), Some(TokenTree::Ident(i)) if i.to_string() == "pub") {
            index += 1;
            if matches!(tokens.get(index), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis)
            {
                index += 1;
            }
        }

        let Some(TokenTree::Ident(name)) = tokens.get(index) else {
            // A trailing comma leaves nothing to read.
            break;
        };
        let name = name.to_string();
        index += 1;

        match tokens.get(index) {
            Some(TokenTree::Punct(p)) if p.as_char() == ':' => index += 1,
            _ => return Err(format!("field `{name}` has no type")),
        }

        // The type runs to the next top-level comma.
        let mut depth = 0i32;
        let mut type_text = String::new();
        while index < tokens.len() {
            match &tokens[index] {
                TokenTree::Punct(p) if p.as_char() == '<' => {
                    depth += 1;
                    type_text.push('<');
                }
                TokenTree::Punct(p) if p.as_char() == '>' => {
                    depth -= 1;
                    type_text.push('>');
                }
                TokenTree::Punct(p) if p.as_char() == ',' && depth <= 0 => {
                    index += 1;
                    break;
                }
                other => type_text.push_str(&other.to_string()),
            }
            index += 1;
        }

        fields.push(Field { name, type_text: type_text.trim().to_string(), attributes });
    }

    Ok(fields)
}

/// Read `model(table = "users", primary_key)` out of one attribute group.
fn merge_attributes(into: &mut Attributes, stream: TokenStream) -> Result<(), String> {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();

    let Some(TokenTree::Ident(name)) = tokens.first() else { return Ok(()) };
    if name.to_string() != "model" {
        // Not ours — `#[derive(...)]`, `#[doc]`, anything else.
        return Ok(());
    }

    let Some(TokenTree::Group(group)) = tokens.get(1) else {
        return Err("`#[model]` needs parentheses: `#[model(table = \"users\")]`".into());
    };

    let inner: Vec<TokenTree> = group.stream().into_iter().collect();
    let mut index = 0;

    while index < inner.len() {
        let TokenTree::Ident(key) = &inner[index] else {
            index += 1;
            continue;
        };
        let key = key.to_string();
        index += 1;

        let value = if matches!(inner.get(index), Some(TokenTree::Punct(p)) if p.as_char() == '=') {
            index += 1;
            match inner.get(index) {
                Some(TokenTree::Literal(literal)) => {
                    index += 1;
                    Some(literal.to_string().trim_matches('"').to_string())
                }
                _ => return Err(format!("`{key}` needs a string value")),
            }
        } else {
            None
        };

        match (key.as_str(), value) {
            ("table", Some(value)) => into.table = Some(value),
            ("crate", Some(value)) => into.krate = Some(value),
            ("column", Some(value)) => into.column = Some(value),
            ("primary_key", _) => into.primary_key = true,
            ("skip", _) => into.skip = true,
            ("generated", _) => into.generated = true,
            (other, _) => {
                return Err(format!(
                    "unknown `#[model]` option `{other}`. Known options: table, column, \
                     primary_key, skip, generated, crate"
                ));
            }
        }

        if matches!(inner.get(index), Some(TokenTree::Punct(p)) if p.as_char() == ',') {
            index += 1;
        }
    }

    Ok(())
}
