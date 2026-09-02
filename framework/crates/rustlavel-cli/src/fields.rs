//! The `--fields` spec that `make:crud` reads.
//!
//! One string decides four things that have to agree: the columns in the
//! migration, the fields on the model, the inputs on the form, and the columns
//! in the table. Writing them out four times is how they drift, so they are
//! written once:
//!
//! ```text
//! --fields "title:string,body:text,published:bool,email:string?"
//! ```
//!
//! A trailing `?` makes the column nullable, the model field an `Option`, and
//! the validation rule `nullable` instead of `required`.

use crate::naming;

/// What a column holds. One variant per `Schema` builder method that is worth
/// generating; anything more exotic is a line the developer adds by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    String,
    Text,
    Integer,
    BigInteger,
    Float,
    Decimal,
    Bool,
    Date,
    Timestamp,
}

/// Every type a spec may name, with the aliases people reach for first.
///
/// The canonical spelling is the first column, and it is what an error message
/// suggests, so nobody has to guess whether it is `bool` or `boolean`.
const TYPES: &[(&str, Kind)] = &[
    ("string", Kind::String),
    ("text", Kind::Text),
    ("integer", Kind::Integer),
    ("int", Kind::Integer),
    ("bigint", Kind::BigInteger),
    ("big_integer", Kind::BigInteger),
    ("float", Kind::Float),
    ("decimal", Kind::Decimal),
    ("bool", Kind::Bool),
    ("boolean", Kind::Bool),
    ("date", Kind::Date),
    ("timestamp", Kind::Timestamp),
    ("datetime", Kind::Timestamp),
];

/// The canonical names, for the "known types" half of an error message.
fn known_types() -> String {
    let mut names: Vec<&str> = [
        Kind::String,
        Kind::Text,
        Kind::Integer,
        Kind::BigInteger,
        Kind::Float,
        Kind::Decimal,
        Kind::Bool,
        Kind::Date,
        Kind::Timestamp,
    ]
    .iter()
    .map(|kind| kind.canonical())
    .collect();
    names.sort_unstable();
    names.join(", ")
}

impl Kind {
    pub fn canonical(&self) -> &'static str {
        match self {
            Kind::String => "string",
            Kind::Text => "text",
            Kind::Integer => "integer",
            Kind::BigInteger => "bigint",
            Kind::Float => "float",
            Kind::Decimal => "decimal",
            Kind::Bool => "bool",
            Kind::Date => "date",
            Kind::Timestamp => "timestamp",
        }
    }

    /// The Rust type the model field takes.
    ///
    /// A date and a timestamp both arrive as text: the framework has no date
    /// type of its own, and inventing one here would be a type the rest of the
    /// application does not know about.
    fn rust(&self) -> &'static str {
        match self {
            Kind::String | Kind::Text | Kind::Date | Kind::Timestamp => "String",
            Kind::Integer => "i32",
            Kind::BigInteger => "i64",
            Kind::Float | Kind::Decimal => "f64",
            Kind::Bool => "bool",
        }
    }

    /// The `Schema` builder call for the column, without the trailing modifiers.
    fn column(&self, name: &str) -> String {
        match self {
            Kind::String => format!("t.string(\"{name}\")"),
            Kind::Text => format!("t.text(\"{name}\")"),
            Kind::Integer => format!("t.integer(\"{name}\")"),
            Kind::BigInteger => format!("t.big_integer(\"{name}\")"),
            Kind::Float => format!("t.float(\"{name}\")"),
            // 12 digits with 2 after the point: money, which is what a decimal
            // column is nearly always for. Widen it if it is not.
            Kind::Decimal => format!("t.decimal(\"{name}\", 12, 2)"),
            Kind::Bool => format!("t.boolean(\"{name}\")"),
            Kind::Date => format!("t.date(\"{name}\")"),
            Kind::Timestamp => format!("t.timestamp(\"{name}\")"),
        }
    }

    /// The rules after `required` or `nullable`.
    fn rules(&self) -> &'static str {
        match self {
            Kind::String => "string|max:255",
            Kind::Text => "string",
            Kind::Integer | Kind::BigInteger => "integer",
            Kind::Float | Kind::Decimal => "numeric",
            Kind::Bool => "boolean",
            Kind::Date | Kind::Timestamp => "date",
        }
    }

    /// The `type` attribute of the form input.
    fn input_type(&self) -> &'static str {
        match self {
            Kind::String => "text",
            Kind::Text => "textarea",
            Kind::Integer | Kind::BigInteger | Kind::Float | Kind::Decimal => "number",
            Kind::Bool => "checkbox",
            Kind::Date => "date",
            Kind::Timestamp => "datetime-local",
        }
    }

    /// The `step` a number input needs to accept what the column stores.
    fn step(&self) -> Option<&'static str> {
        match self {
            Kind::Float => Some("any"),
            Kind::Decimal => Some("0.01"),
            _ => None,
        }
    }
}

/// One column, as named in `--fields`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub kind: Kind,
    pub nullable: bool,
}

impl Field {
    /// `title` → `Title`, for a label and a table heading.
    pub fn label(&self) -> String {
        let words = self.name.replace('_', " ");
        let mut chars = words.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        }
    }

    /// The struct field: `pub title: String,`.
    pub fn declaration(&self) -> String {
        let ty = if self.nullable {
            format!("Option<{}>", self.kind.rust())
        } else {
            self.kind.rust().to_string()
        };
        format!("    pub {}: {ty},", self.name)
    }

    /// The migration line: `t.string("title").nullable();`.
    pub fn migration_line(&self) -> String {
        let mut line = self.kind.column(&self.name);
        if self.nullable {
            line.push_str(".nullable()");
        } else if self.kind == Kind::Bool {
            // A boolean column with no default is three-valued in practice:
            // true, false, and the row somebody inserted before the column
            // existed. A default keeps it to two.
            line.push_str(".default_bool(false)");
        }
        format!("                {line};")
    }

    /// The rule string for this column.
    pub fn rule(&self) -> String {
        // A checkbox that is not ticked is not sent at all, so `required` on a
        // boolean would make "no" impossible to express.
        let leading = if self.nullable || self.kind == Kind::Bool { "nullable" } else { "required" };
        format!("{leading}|{}", self.kind.rules())
    }

    /// Reading this field out of the validated subset.
    pub fn read_validated(&self) -> String {
        let name = &self.name;
        match (self.kind, self.nullable) {
            (Kind::Bool, _) => format!("valid.boolean(\"{name}\").unwrap_or(false)"),
            (Kind::Integer, false) => format!("valid.integer(\"{name}\").unwrap_or_default() as i32"),
            (Kind::Integer, true) => format!("valid.integer(\"{name}\").map(|value| value as i32)"),
            (Kind::BigInteger, false) => format!("valid.integer(\"{name}\").unwrap_or_default()"),
            (Kind::BigInteger, true) => format!("valid.integer(\"{name}\")"),
            (Kind::Float | Kind::Decimal, false) => {
                format!("valid.number(\"{name}\").unwrap_or_default()")
            }
            (Kind::Float | Kind::Decimal, true) => format!("valid.number(\"{name}\")"),
            (_, false) => format!("valid.string(\"{name}\").unwrap_or_default()"),
            (_, true) => format!("valid.string(\"{name}\")"),
        }
    }

    /// Turning the stored value back into what the form input shows.
    pub fn to_form_value(&self, record: &str) -> String {
        let name = &self.name;
        match (self.kind, self.nullable) {
            // A checkbox posts "1" or nothing, so that is what it reads back.
            (Kind::Bool, false) => {
                format!("if {record}.{name} {{ \"1\".to_string() }} else {{ String::new() }}")
            }
            (Kind::Bool, true) => format!(
                "if {record}.{name}.unwrap_or(false) {{ \"1\".to_string() }} else {{ String::new() }}"
            ),
            (Kind::String | Kind::Text | Kind::Date | Kind::Timestamp, false) => {
                format!("{record}.{name}.clone()")
            }
            (Kind::String | Kind::Text | Kind::Date | Kind::Timestamp, true) => {
                format!("{record}.{name}.clone().unwrap_or_default()")
            }
            (_, false) => format!("{record}.{name}.to_string()"),
            (_, true) => {
                format!("{record}.{name}.map(|value| value.to_string()).unwrap_or_default()")
            }
        }
    }

    /// The input element for this column, already indented for the form.
    pub fn input_html(&self) -> String {
        let name = &self.name;
        let label = self.label();
        let required = if self.nullable || self.kind == Kind::Bool { "" } else { " required" };

        let control = match self.kind {
            Kind::Text => format!(
                "<textarea id=\"{name}\" name=\"{name}\" rows=\"6\" class=\"field-input\"{required}>{{{{ value_{name} }}}}</textarea>"
            ),
            Kind::Bool => format!(
                "<input id=\"{name}\" name=\"{name}\" type=\"checkbox\" value=\"1\" class=\"field-check\"@if(checked_{name}) checked@endif>"
            ),
            kind => {
                let step = kind.step().map(|s| format!(" step=\"{s}\"")).unwrap_or_default();
                format!(
                    "<input id=\"{name}\" name=\"{name}\" type=\"{}\"{step} value=\"{{{{ value_{name} }}}}\" class=\"field-input\"{required}>",
                    kind.input_type()
                )
            }
        };

        // The checkbox reads better with the label after it; everything else
        // reads better with the label above.
        let body = if self.kind == Kind::Bool {
            format!("        <label class=\"field-inline\">{control} <span>{label}</span></label>")
        } else {
            format!(
                "        <label class=\"field-label\" for=\"{name}\">{label}</label>\n        {control}"
            )
        };

        format!(
            "      <div class=\"field\">\n{body}\n        @if(error_{name})<p class=\"field-error\">{{{{ error_{name} }}}}</p>@endif\n      </div>"
        )
    }
}

/// The columns the generator adds itself, and which a spec must not repeat.
const RESERVED: &[&str] = &["id", "created_at", "updated_at"];

/// Parse a `--fields` value.
///
/// Every failure names the token that caused it, because a spec is typed on a
/// command line where there is nothing to click on and no line number to read.
pub fn parse(spec: &str) -> Result<Vec<Field>, String> {
    if spec.trim().is_empty() {
        return Err("--fields is empty. Write it as \"title:string,body:text\", \
                    or leave --fields off for an id/timestamps skeleton."
            .to_string());
    }

    let mut fields: Vec<Field> = Vec::new();

    for part in spec.split(',') {
        let token = part.trim();
        if token.is_empty() {
            return Err(format!(
                "`{spec}` has an empty field — two commas together, or one at the end."
            ));
        }

        let Some((raw_name, raw_type)) = token.split_once(':') else {
            return Err(format!(
                "`{token}` is not a field. Write it as `name:type`, for instance `title:string`."
            ));
        };

        let name = raw_name.trim();
        if name.is_empty() {
            return Err(format!("`{token}` has no column name before the `:`."));
        }
        // A dash is accepted and normalised, because `published-at` is what a
        // shell history hands back; a space or a quote is not, because that is
        // a spec with a missing comma rather than a name.
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            || name.starts_with(|c: char| c.is_ascii_digit())
        {
            return Err(format!(
                "`{name}` is not a column name. Use letters, digits and underscores, \
                 starting with a letter — `published_at`, not `{name}`."
            ));
        }
        let name = naming::snake(name);

        if RESERVED.contains(&name.as_str()) {
            return Err(format!(
                "`{name}` is added by the generator — `id` is the primary key and \
                 `created_at`/`updated_at` come from `t.timestamps()`. Remove `{token}` \
                 from --fields."
            ));
        }
        if fields.iter().any(|existing| existing.name == name) {
            return Err(format!("`{name}` appears twice in --fields."));
        }

        let raw_type = raw_type.trim();
        if raw_type.is_empty() {
            return Err(format!(
                "`{token}` has no type after the `:`. Known types: {}.",
                known_types()
            ));
        }

        let (type_name, nullable) = match raw_type.strip_suffix('?') {
            Some(stripped) => (stripped.trim(), true),
            None => (raw_type, false),
        };
        if type_name.is_empty() {
            return Err(format!(
                "`{token}` is nullable but names no type. Write it as `{name}:string?`."
            ));
        }

        let Some((_, kind)) = TYPES.iter().find(|(alias, _)| *alias == type_name) else {
            return Err(format!(
                "`{type_name}` is not a column type, in `{token}`. Known types: {}. \
                 Add `?` for a nullable column, as in `email:string?`.",
                known_types()
            ));
        };

        fields.push(Field { name, kind: *kind, nullable });
    }

    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(spec: &str) -> Field {
        parse(spec).expect("spec should parse").pop().expect("one field")
    }

    #[test]
    fn parses_every_type_it_documents() {
        let expected = [
            ("string", Kind::String),
            ("text", Kind::Text),
            ("integer", Kind::Integer),
            ("int", Kind::Integer),
            ("bigint", Kind::BigInteger),
            ("big_integer", Kind::BigInteger),
            ("float", Kind::Float),
            ("decimal", Kind::Decimal),
            ("bool", Kind::Bool),
            ("boolean", Kind::Bool),
            ("date", Kind::Date),
            ("timestamp", Kind::Timestamp),
            ("datetime", Kind::Timestamp),
        ];

        for (spelling, kind) in expected {
            let field = one(&format!("value:{spelling}"));
            assert_eq!(field.kind, kind, "`{spelling}` should parse as {kind:?}");
            assert!(!field.nullable, "`{spelling}` is not nullable without a `?`");
        }
    }

    #[test]
    fn the_question_mark_makes_a_column_nullable() {
        for spelling in ["string", "int", "bool", "decimal", "timestamp"] {
            let field = one(&format!("value:{spelling}?"));
            assert!(field.nullable, "`{spelling}?` should be nullable");
        }

        let field = one("email:string?");
        assert_eq!(field.declaration(), "    pub email: Option<String>,");
        assert!(field.migration_line().contains(".nullable()"));
        assert!(field.rule().starts_with("nullable|"));
    }

    #[test]
    fn parses_a_whole_spec_in_order() {
        let fields = parse("title:string, body:text ,published:bool").unwrap();

        assert_eq!(
            fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            ["title", "body", "published"]
        );
        assert_eq!(fields[2].kind, Kind::Bool);
    }

    #[test]
    fn names_are_normalised_to_snake_case() {
        assert_eq!(one("publishedAt:timestamp").name, "published_at");
        assert_eq!(one("published-at:timestamp").name, "published_at");
    }

    #[test]
    fn an_empty_spec_says_what_to_write_instead() {
        let message = parse("   ").unwrap_err();
        assert!(message.contains("--fields is empty"), "{message}");
        assert!(message.contains("title:string"), "{message}");
    }

    #[test]
    fn a_field_with_no_colon_is_rejected_by_name() {
        let message = parse("title,body:text").unwrap_err();
        assert!(message.contains("`title`"), "the offending token: {message}");
        assert!(message.contains("name:type"), "{message}");
    }

    #[test]
    fn an_unknown_type_is_rejected_by_name_and_lists_the_known_ones() {
        let message = parse("title:strng").unwrap_err();
        assert!(message.contains("`strng`"), "the offending type: {message}");
        assert!(message.contains("`title:strng`"), "the offending token: {message}");
        assert!(message.contains("string"), "{message}");
        assert!(message.contains("timestamp"), "{message}");
        assert!(message.contains("email:string?"), "{message}");
    }

    #[test]
    fn a_missing_name_or_type_is_rejected() {
        let no_name = parse(":string").unwrap_err();
        assert!(no_name.contains("`:string`"), "{no_name}");
        assert!(no_name.contains("no column name"), "{no_name}");

        let no_type = parse("title:").unwrap_err();
        assert!(no_type.contains("`title:`"), "{no_type}");
        assert!(no_type.contains("no type"), "{no_type}");

        let only_nullable = parse("title:?").unwrap_err();
        assert!(only_nullable.contains("names no type"), "{only_nullable}");
    }

    #[test]
    fn an_empty_element_is_rejected() {
        let trailing = parse("title:string,").unwrap_err();
        assert!(trailing.contains("empty field"), "{trailing}");

        let doubled = parse("title:string,,body:text").unwrap_err();
        assert!(doubled.contains("empty field"), "{doubled}");
    }

    #[test]
    fn a_column_name_that_is_not_a_name_is_rejected() {
        let message = parse("2legit:string").unwrap_err();
        assert!(message.contains("`2legit`"), "{message}");

        let punctuation = parse("first name:string").unwrap_err();
        assert!(punctuation.contains("first name"), "{punctuation}");
    }

    #[test]
    fn the_generated_columns_may_not_be_repeated() {
        for reserved in ["id", "created_at", "updated_at"] {
            let message = parse(&format!("{reserved}:bigint")).unwrap_err();
            assert!(message.contains(reserved), "{message}");
            assert!(message.contains("added by the generator"), "{message}");
        }
    }

    #[test]
    fn a_repeated_column_is_rejected() {
        let message = parse("title:string,title:text").unwrap_err();
        assert!(message.contains("`title` appears twice"), "{message}");
    }

    #[test]
    fn rules_follow_the_type_and_the_nullability() {
        assert_eq!(one("title:string").rule(), "required|string|max:255");
        assert_eq!(one("body:text").rule(), "required|string");
        assert_eq!(one("count:integer").rule(), "required|integer");
        assert_eq!(one("price:decimal").rule(), "required|numeric");
        assert_eq!(one("at:date").rule(), "required|date");
        // A checkbox that is not ticked is not sent, so it can never be required.
        assert_eq!(one("published:bool").rule(), "nullable|boolean");
        assert_eq!(one("email:string?").rule(), "nullable|string|max:255");
    }

    #[test]
    fn a_boolean_column_gets_a_default_so_it_is_never_null() {
        assert!(one("published:bool").migration_line().contains("default_bool(false)"));
        assert!(!one("published:bool?").migration_line().contains("default_bool"));
    }

    #[test]
    fn labels_read_like_english() {
        assert_eq!(one("published_at:timestamp").label(), "Published at");
        assert_eq!(one("title:string").label(), "Title");
    }

    #[test]
    fn every_input_names_its_own_field_and_error() {
        for spec in ["title:string", "body:text", "published:bool", "price:decimal"] {
            let field = one(spec);
            let html = field.input_html();
            assert!(html.contains(&format!("error_{}", field.name)), "{html}");
            assert!(html.contains(&format!("name=\"{}\"", field.name)), "{html}");
        }

        assert!(one("body:text").input_html().contains("<textarea"));
        assert!(one("published:bool").input_html().contains("type=\"checkbox\""));
        assert!(one("price:decimal").input_html().contains("step=\"0.01\""));
        assert!(one("weight:float").input_html().contains("step=\"any\""));
    }
}
