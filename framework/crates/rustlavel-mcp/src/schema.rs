//! The input schema a tool declares, and the validator that guards it.
//!
//! Two jobs, one definition. An agent needs a JSON Schema to know how to call
//! a tool, and the server needs to check what actually arrives — a tool's
//! arguments come from a language model, which is to say from the open
//! internet. Deriving both from the same `Schema` means the description an
//! agent reads can never drift from the rule the server enforces.
//!
//! Nobody writes schema JSON by hand:
//!
//! ```ignore
//! Schema::object()
//!     .string("city", "The city to look up")
//!     .field(Field::integer("days").describe("How many days ahead").optional())
//! ```

use rustlavel_core::Json;

/// The kinds of value a field may hold.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    String,
    /// Any JSON number.
    Number,
    /// A number with no fractional part — checked, not just declared.
    Integer,
    Boolean,
    Array(Box<FieldType>),
    /// A nested object with its own fields.
    Object(Box<Schema>),
    /// Anything at all, for a tool that genuinely takes free-form JSON.
    Any,
}

impl FieldType {
    /// The JSON Schema `type` keyword for this field.
    fn keyword(&self) -> Option<&'static str> {
        match self {
            FieldType::String => Some("string"),
            FieldType::Number => Some("number"),
            FieldType::Integer => Some("integer"),
            FieldType::Boolean => Some("boolean"),
            FieldType::Array(_) => Some("array"),
            FieldType::Object(_) => Some("object"),
            FieldType::Any => None,
        }
    }

    /// What a value of the wrong type should be called in an error message.
    fn describe(value: &Json) -> &'static str {
        match value {
            Json::Null => "null",
            Json::Bool(_) => "a boolean",
            Json::Number(_) => "a number",
            Json::String(_) => "a string",
            Json::Array(_) => "an array",
            Json::Object(_) => "an object",
        }
    }

    fn to_json(&self) -> Json {
        match self {
            FieldType::Array(item) => {
                Json::object([("type", Json::from("array")), ("items", item.to_json())])
            }
            FieldType::Object(schema) => schema.to_json(),
            FieldType::Any => Json::Object(Default::default()),
            other => Json::object([("type", Json::from(other.keyword().unwrap_or("string")))]),
        }
    }

    /// Check one value, appending a human sentence per problem found.
    fn check(&self, path: &str, value: &Json, errors: &mut Vec<String>) {
        let mismatch = |errors: &mut Vec<String>, expected: &str| {
            errors.push(format!(
                "`{path}` must be {expected}, found {}",
                FieldType::describe(value)
            ));
        };

        match self {
            FieldType::Any => {}
            FieldType::String if !matches!(value, Json::String(_)) => mismatch(errors, "a string"),
            FieldType::Boolean if !matches!(value, Json::Bool(_)) => mismatch(errors, "a boolean"),
            FieldType::Number if !matches!(value, Json::Number(_)) => mismatch(errors, "a number"),
            FieldType::Integer => match value {
                Json::Number(n) if n.fract() == 0.0 => {}
                Json::Number(_) => errors
                    .push(format!("`{path}` must be an integer, found a fractional number")),
                _ => mismatch(errors, "an integer"),
            },
            FieldType::Array(item) => match value {
                Json::Array(values) => {
                    for (index, entry) in values.iter().enumerate() {
                        item.check(&format!("{path}[{index}]"), entry, errors);
                    }
                }
                _ => mismatch(errors, "an array"),
            },
            FieldType::Object(schema) => match value {
                Json::Object(_) => schema.check(path, value, errors),
                _ => mismatch(errors, "an object"),
            },
            _ => {}
        }
    }
}

/// One property of an object schema.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub description: Option<String>,
    pub kind: FieldType,
    pub required: bool,
    /// When set, the value must be one of these — a closed set an agent can see.
    pub allowed: Vec<String>,
}

impl Field {
    pub fn new(name: impl Into<String>, kind: FieldType) -> Self {
        Field {
            name: name.into(),
            description: None,
            kind,
            // Required by default: the common case, and the safe one.
            required: true,
            allowed: Vec::new(),
        }
    }

    pub fn string(name: impl Into<String>) -> Self {
        Field::new(name, FieldType::String)
    }

    pub fn number(name: impl Into<String>) -> Self {
        Field::new(name, FieldType::Number)
    }

    pub fn integer(name: impl Into<String>) -> Self {
        Field::new(name, FieldType::Integer)
    }

    pub fn boolean(name: impl Into<String>) -> Self {
        Field::new(name, FieldType::Boolean)
    }

    pub fn array_of(name: impl Into<String>, item: FieldType) -> Self {
        Field::new(name, FieldType::Array(Box::new(item)))
    }

    pub fn object(name: impl Into<String>, schema: Schema) -> Self {
        Field::new(name, FieldType::Object(Box::new(schema)))
    }

    pub fn any(name: impl Into<String>) -> Self {
        Field::new(name, FieldType::Any)
    }

    /// The sentence the calling model reads to decide what to put here.
    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn optional(mut self) -> Self {
        self.required = false;
        self
    }

    /// Restrict to a fixed set of string values.
    pub fn one_of<I, S>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed = values.into_iter().map(Into::into).collect();
        self
    }

    fn to_json(&self) -> Json {
        let mut value = self.kind.to_json();
        if let Json::Object(map) = &mut value {
            if let Some(description) = &self.description {
                map.insert("description".into(), Json::from(description.clone()));
            }
            if !self.allowed.is_empty() {
                let allowed: Vec<Json> = self.allowed.iter().cloned().map(Json::from).collect();
                map.insert("enum".into(), Json::Array(allowed));
            }
        }
        value
    }
}

/// An object schema: what a tool accepts.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Schema {
    fields: Vec<Field>,
}

impl Schema {
    pub fn object() -> Self {
        Schema::default()
    }

    /// Add a fully-specified field.
    pub fn field(mut self, field: Field) -> Self {
        self.fields.push(field);
        self
    }

    /// Shorthand for the common case: a required, described property.
    pub fn string(self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.field(Field::string(name).describe(description))
    }

    pub fn number(self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.field(Field::number(name).describe(description))
    }

    pub fn integer(self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.field(Field::integer(name).describe(description))
    }

    pub fn boolean(self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.field(Field::boolean(name).describe(description))
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The JSON Schema an agent sees as a tool's `inputSchema`.
    pub fn to_json(&self) -> Json {
        let properties: Vec<(String, Json)> =
            self.fields.iter().map(|f| (f.name.clone(), f.to_json())).collect();
        let required: Vec<Json> = self
            .fields
            .iter()
            .filter(|f| f.required)
            .map(|f| Json::from(f.name.clone()))
            .collect();

        let mut pairs = vec![
            ("type".to_string(), Json::from("object")),
            ("properties".to_string(), Json::object(properties)),
        ];
        // An empty `required` array is noise; the specification allows omitting it.
        if !required.is_empty() {
            pairs.push(("required".to_string(), Json::Array(required)));
        }
        Json::object(pairs)
    }

    /// Check arguments against the schema.
    ///
    /// Returns every problem at once rather than the first, because the caller
    /// is a language model that will otherwise fix one field per round trip.
    pub fn validate(&self, arguments: &Json) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        // A tool called with no arguments at all is the same as `{}`; whether
        // that is acceptable is then decided by the required fields.
        let arguments = if arguments.is_null() {
            Json::Object(Default::default())
        } else {
            arguments.clone()
        };

        if arguments.as_object().is_none() {
            return Err(vec![format!(
                "arguments must be an object, found {}",
                FieldType::describe(&arguments)
            )]);
        }

        self.check("arguments", &arguments, &mut errors);
        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }

    fn check(&self, path: &str, value: &Json, errors: &mut Vec<String>) {
        let Some(map) = value.as_object() else { return };

        for field in &self.fields {
            let child = if path == "arguments" {
                field.name.clone()
            } else {
                format!("{path}.{}", field.name)
            };

            match map.get(&field.name) {
                None | Some(Json::Null) if field.required => {
                    errors.push(format!("`{child}` is required"));
                }
                None | Some(Json::Null) => {}
                Some(found) => {
                    field.kind.check(&child, found, errors);
                    if !field.allowed.is_empty() {
                        let text = found.as_str().unwrap_or_default();
                        if !field.allowed.iter().any(|allowed| allowed == text) {
                            errors.push(format!(
                                "`{child}` must be one of {}",
                                field.allowed.join(", ")
                            ));
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weather_schema() -> Schema {
        Schema::object()
            .string("city", "The city to look up")
            .field(Field::integer("days").describe("How many days ahead").optional())
    }

    #[test]
    fn a_schema_renders_the_json_an_agent_reads() {
        let schema = weather_schema();

        assert_eq!(
            schema.to_json().to_string(),
            r#"{"properties":{"city":{"description":"The city to look up","type":"string"},"days":{"description":"How many days ahead","type":"integer"}},"required":["city"],"type":"object"}"#
        );
    }

    #[test]
    fn a_schema_with_no_required_fields_omits_the_required_array() {
        let schema = Schema::object().field(Field::string("note").optional());
        assert!(schema.to_json().get("required").is_none());
    }

    #[test]
    fn valid_arguments_pass() {
        let arguments =
            Json::object([("city", Json::from("Jakarta")), ("days", Json::from(3))]);
        assert!(weather_schema().validate(&arguments).is_ok());
    }

    #[test]
    fn an_optional_field_may_be_left_out() {
        let arguments = Json::object([("city", Json::from("Jakarta"))]);
        assert!(weather_schema().validate(&arguments).is_ok());
    }

    #[test]
    fn a_missing_required_field_is_reported() {
        let errors = weather_schema().validate(&Json::object([("days", Json::from(1))])).unwrap_err();
        assert_eq!(errors, ["`city` is required"]);
    }

    #[test]
    fn a_wrong_type_is_rejected_with_the_type_it_saw() {
        let arguments = Json::object([("city", Json::from(42))]);
        let errors = weather_schema().validate(&arguments).unwrap_err();

        assert_eq!(errors, ["`city` must be a string, found a number"]);
    }

    #[test]
    fn a_fractional_number_is_not_an_integer() {
        let arguments =
            Json::object([("city", Json::from("Bandung")), ("days", Json::from(1.5))]);
        let errors = weather_schema().validate(&arguments).unwrap_err();

        assert_eq!(errors, ["`days` must be an integer, found a fractional number"]);
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let arguments = Json::object([("days", Json::from("soon"))]);
        let errors = weather_schema().validate(&arguments).unwrap_err();

        assert_eq!(errors.len(), 2);
        assert!(errors.iter().any(|e| e.contains("`city` is required")));
        assert!(errors.iter().any(|e| e.contains("`days` must be an integer")));
    }

    #[test]
    fn arguments_that_are_not_an_object_are_rejected() {
        let errors = weather_schema().validate(&Json::from("just a string")).unwrap_err();
        assert_eq!(errors, ["arguments must be an object, found a string"]);
    }

    #[test]
    fn absent_arguments_are_treated_as_an_empty_object() {
        assert!(Schema::object().validate(&Json::Null).is_ok());
        assert!(weather_schema().validate(&Json::Null).is_err());
    }

    #[test]
    fn arrays_check_every_element() {
        let schema = Schema::object().field(Field::array_of("tags", FieldType::String));
        let good = Json::object([("tags", Json::Array(vec![Json::from("a")]))]);
        let bad = Json::object([("tags", Json::Array(vec![Json::from("a"), Json::from(2)]))]);

        assert!(schema.validate(&good).is_ok());
        assert_eq!(schema.validate(&bad).unwrap_err(), ["`tags[1]` must be a string, found a number"]);
    }

    #[test]
    fn nested_objects_are_validated_through() {
        let schema = Schema::object()
            .field(Field::object("filter", Schema::object().string("status", "Which status")));
        let bad = Json::object([(
            "filter",
            Json::object([("status", Json::from(true))]),
        )]);

        assert_eq!(
            schema.validate(&bad).unwrap_err(),
            ["`filter.status` must be a string, found a boolean"]
        );
    }

    #[test]
    fn an_enum_restricts_the_accepted_values() {
        let schema = Schema::object().field(Field::string("unit").one_of(["celsius", "fahrenheit"]));

        assert!(schema.validate(&Json::object([("unit", Json::from("celsius"))])).is_ok());
        assert_eq!(
            schema.validate(&Json::object([("unit", Json::from("kelvin"))])).unwrap_err(),
            ["`unit` must be one of celsius, fahrenheit"]
        );
        assert_eq!(
            schema.to_json().get("properties.unit.enum").unwrap().to_string(),
            r#"["celsius","fahrenheit"]"#
        );
    }

    #[test]
    fn an_any_field_accepts_whatever_arrives() {
        let schema = Schema::object().field(Field::any("payload"));

        assert!(schema.validate(&Json::object([("payload", Json::from(1))])).is_ok());
        assert!(schema.validate(&Json::object([("payload", Json::from("x"))])).is_ok());
    }
}
