//! The values a validator runs against.
//!
//! A JSON body, a urlencoded form, and a query string all arrive as the same
//! thing here: a map of field name to [`Json`]. Form and query values are always
//! strings, which is why rules like `integer` accept `"18"` as readily as `18`
//! — whether a form validates should not depend on how the browser encoded it.

use rustlavel_core::Json;
use rustlavel_http::Request;
use std::collections::BTreeMap;

/// The named values under validation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Input {
    fields: BTreeMap<String, Json>,
}

impl Input {
    pub fn new() -> Self {
        Input::default()
    }

    /// Take the top level of a JSON object.
    ///
    /// A body that is not an object — an array, a bare string — has no named
    /// fields, so it validates as empty and `required` rules report what is
    /// missing. That is a better failure than a parse error the client cannot act on.
    pub fn from_json(value: &Json) -> Self {
        match value {
            Json::Object(map) => Input { fields: map.clone() },
            _ => Input::new(),
        }
    }

    /// Build from name/value pairs — a form body or a query string.
    ///
    /// A repeated key becomes an array, because that is how a browser sends a
    /// group of checkboxes, and it is what makes `array` and a counting `min`
    /// work on one.
    pub fn from_pairs<K: AsRef<str>, V: AsRef<str>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        let mut input = Input::new();
        for (key, value) in pairs {
            input.push(key.as_ref(), value.as_ref());
        }
        input
    }

    /// Everything a request carries, in `Request::input`'s precedence order:
    /// the query string first, then a form body, then a JSON body on top.
    pub fn from_request(request: &mut Request) -> Self {
        let query: Vec<(String, String)> = request.query_pairs().to_vec();
        let mut input = Input::from_pairs(query);

        let form: Vec<(String, String)> = request.form().to_vec();
        input.merge(Input::from_pairs(form));

        if let Some(body) = request.json() {
            let body = Input::from_json(body);
            input.merge(body);
        }
        input
    }

    /// Overlay another set of values, replacing any field they share.
    pub fn merge(&mut self, other: Input) {
        self.fields.extend(other.fields);
    }

    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<Json>) {
        self.fields.insert(name.into(), value.into());
    }

    /// The chaining form of [`Input::insert`], for building an input in a test
    /// or from values the application already holds.
    pub fn with(mut self, name: impl Into<String>, value: impl Into<Json>) -> Self {
        self.insert(name, value);
        self
    }

    pub fn get(&self, name: &str) -> Option<&Json> {
        self.fields.get(name)
    }

    /// Whether the field was sent at all. A field sent as `null` is present.
    pub fn has(&self, name: &str) -> bool {
        self.fields.contains_key(name)
    }

    pub fn fields(&self) -> &BTreeMap<String, Json> {
        &self.fields
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Add a string value, promoting a repeated key to an array.
    fn push(&mut self, name: &str, value: &str) {
        match self.fields.get_mut(name) {
            Some(Json::Array(items)) => items.push(Json::from(value)),
            Some(existing) => {
                let first = std::mem::replace(existing, Json::Null);
                *existing = Json::Array(vec![first, Json::from(value)]);
            }
            None => {
                self.fields.insert(name.to_string(), Json::from(value));
            }
        }
    }
}

impl From<Json> for Input {
    fn from(value: Json) -> Self {
        Input::from_json(&value)
    }
}

impl From<&Json> for Input {
    fn from(value: &Json) -> Self {
        Input::from_json(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_http::Method;

    #[test]
    fn takes_the_top_level_of_a_json_object() {
        let body = Json::parse(r#"{"email":"ada@example.com","age":36,"tags":["a"]}"#).unwrap();
        let input = Input::from_json(&body);

        assert_eq!(input.get("email").unwrap().as_str(), Some("ada@example.com"));
        assert_eq!(input.get("age").unwrap().as_i64(), Some(36));
        assert_eq!(input.get("tags").unwrap().as_array().unwrap().len(), 1);
        assert_eq!(input.len(), 3);
    }

    #[test]
    fn a_json_body_that_is_not_an_object_validates_as_empty() {
        assert!(Input::from_json(&Json::parse("[1,2]").unwrap()).is_empty());
        assert!(Input::from_json(&Json::Null).is_empty());
    }

    #[test]
    fn a_repeated_pair_key_becomes_an_array() {
        let input = Input::from_pairs([("tag", "a"), ("tag", "b"), ("tag", "c"), ("name", "ada")]);

        assert_eq!(input.get("tag").unwrap().as_array().unwrap().len(), 3);
        assert_eq!(input.get("name").unwrap().as_str(), Some("ada"));
    }

    #[test]
    fn a_request_body_wins_over_the_query_string() {
        let mut request = Request::new(Method::Post, "/users?name=from-query&page=2")
            .with_json(Json::object([("name", "from-body".into())]));
        let input = Input::from_request(&mut request);

        assert_eq!(input.get("name").unwrap().as_str(), Some("from-body"));
        assert_eq!(input.get("page").unwrap().as_str(), Some("2"));
    }

    #[test]
    fn a_form_body_is_read_alongside_the_query_string() {
        let mut request = Request::new(Method::Post, "/login?next=/home")
            .with_form(&[("email", "ada@example.com"), ("password", "s e c")]);
        let input = Input::from_request(&mut request);

        assert_eq!(input.get("email").unwrap().as_str(), Some("ada@example.com"));
        assert_eq!(input.get("password").unwrap().as_str(), Some("s e c"));
        assert_eq!(input.get("next").unwrap().as_str(), Some("/home"));
    }

    #[test]
    fn merging_replaces_shared_fields_and_keeps_the_rest() {
        let mut input = Input::new().with("a", 1).with("b", 2);
        input.merge(Input::new().with("b", 20).with("c", 30));

        assert_eq!(input.get("a").unwrap().as_i64(), Some(1));
        assert_eq!(input.get("b").unwrap().as_i64(), Some(20));
        assert_eq!(input.get("c").unwrap().as_i64(), Some(30));
    }

    #[test]
    fn a_null_field_is_present_even_though_it_is_empty() {
        let input = Input::new().with("nickname", Json::Null);

        assert!(input.has("nickname"));
        assert!(!input.has("missing"));
        assert!(input.get("nickname").unwrap().is_null());
    }
}
