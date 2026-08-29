//! Running rules against input, and what comes back when they all pass.
//!
//! The semantics follow Laravel closely, including the two that surprise people
//! if they are not written down:
//!
//! - A field that was never sent only trips `required`. Every other rule is
//!   skipped, so `nullable|integer` on an absent field is silence, not an error.
//! - A blank string — an untouched text input, which a browser submits as `""`
//!   rather than omitting — is treated as absent for the same reason.
//!
//! On success the validator hands back a [`Validated`] containing *only* the
//! fields that had rules. That is the point of validating: what comes out is
//! the subset that was actually checked, so an unexpected extra field in the
//! body cannot ride along into a database write.

use crate::check;
use crate::errors::Errors;
use crate::input::Input;
use crate::messages::{self, Messages, SizeKind};
use crate::rule::{IntoRules, Rule, Rules};
use rustlavel_core::Json;
use rustlavel_http::Request;
use std::collections::BTreeMap;

/// A set of fields, their rules, and the values to run them against.
#[derive(Debug, Clone, Default)]
pub struct Validator {
    input: Input,
    fields: Vec<(String, Rules)>,
    messages: Messages,
    wants_json: bool,
}

impl Validator {
    /// Validate a bag of values — a JSON body, a form, or an [`Input`] built by hand.
    pub fn new(input: impl Into<Input>) -> Self {
        Validator { input: input.into(), ..Validator::default() }
    }

    /// Validate a request, remembering whether the client wants JSON back so
    /// the failure response can be negotiated later, when the request is gone.
    pub fn from_request(request: &mut Request) -> Self {
        let wants_json = request.wants_json();
        Validator { input: Input::from_request(request), wants_json, ..Validator::default() }
    }

    /// Add one field's rules, as a Laravel string or as a built rule set.
    pub fn rule(mut self, field: impl Into<String>, rules: impl IntoRules) -> Self {
        self.fields.push((field.into(), rules.into_rules()));
        self
    }

    /// Add every field at once: `.rules(&[("email", "required|email")])`.
    pub fn rules(mut self, specs: &[(&str, &str)]) -> Self {
        for (field, spec) in specs {
            self = self.rule(*field, *spec);
        }
        self
    }

    /// Override one message. The key is `"field.rule"` or just `"rule"`.
    pub fn message(mut self, key: impl Into<String>, message: impl Into<String>) -> Self {
        self.messages.set(key, message);
        self
    }

    /// Rename a field for display: `.attribute("dob", "date of birth")`.
    pub fn attribute(mut self, field: impl Into<String>, label: impl Into<String>) -> Self {
        self.messages.set_attribute(field, label);
        self
    }

    /// Replace the whole message bag, for an application that keeps its own.
    pub fn with_messages(mut self, messages: Messages) -> Self {
        self.messages = messages;
        self
    }

    /// Force the failure response to be JSON (or not), overriding what the
    /// request negotiated.
    pub fn with_json(mut self, wants_json: bool) -> Self {
        self.wants_json = wants_json;
        self
    }

    pub fn input(&self) -> &Input {
        &self.input
    }

    /// Every message the rules produce. Empty when the input is valid.
    pub fn errors(&self) -> Errors {
        let mut errors = Errors::new().with_json(self.wants_json);
        for (field, rules) in &self.fields {
            self.check(field, rules, &mut errors);
        }
        errors
    }

    pub fn passes(&self) -> bool {
        self.errors().is_empty()
    }

    pub fn fails(&self) -> bool {
        !self.passes()
    }

    /// The validated subset of the input, or every message that failed.
    pub fn validate(&self) -> Result<Validated, Errors> {
        let errors = self.errors();
        if !errors.is_empty() {
            return Err(errors);
        }
        let mut fields = BTreeMap::new();
        for (field, _) in &self.fields {
            if let Some(value) = self.input.get(field) {
                fields.insert(field.clone(), value.clone());
            }
        }
        Ok(Validated { fields })
    }

    fn check(&self, field: &str, rules: &Rules, errors: &mut Errors) {
        let required = rules.has("required");

        let Some(value) = self.input.get(field) else {
            if required {
                self.fail(field, &Rule::Required, SizeKind::String, errors);
            }
            return;
        };

        if required && is_blank(value) {
            self.fail(field, &Rule::Required, SizeKind::String, errors);
            return;
        }

        // A blank string is what a browser sends for an untouched input. Laravel
        // treats it as absent for every rule but the presence rules, so
        // `nullable|email` on an empty box is silence rather than a complaint.
        if matches!(value, Json::String(text) if text.trim().is_empty()) {
            return;
        }

        // An explicit `null` stops here only when the field opted into it;
        // otherwise it falls through so `integer` can say what is wrong.
        if value.is_null() && rules.has("nullable") {
            return;
        }

        for rule in rules.rules() {
            if matches!(rule, Rule::Required | Rule::Nullable) {
                continue;
            }
            if !self.satisfied(field, rule, value, rules) {
                self.fail(field, rule, size_kind(value, rules), errors);
            }
        }
    }

    fn satisfied(&self, field: &str, rule: &Rule, value: &Json, rules: &Rules) -> bool {
        match rule {
            Rule::Required | Rule::Nullable => true,
            Rule::String => matches!(value, Json::String(_)),
            Rule::Integer => as_number(value).is_some_and(|n| n.fract() == 0.0),
            Rule::Numeric => as_number(value).is_some(),
            Rule::Boolean => is_boolean(value),
            Rule::Email => text(value).is_some_and(|t| check::is_email(&t)),
            Rule::Url => text(value).is_some_and(|t| check::is_url(&t)),
            Rule::Alpha => text(value).is_some_and(|t| check::is_alpha(&t)),
            Rule::AlphaNum => text(value).is_some_and(|t| check::is_alpha_num(&t)),
            Rule::AlphaDash => text(value).is_some_and(|t| check::is_alpha_dash(&t)),
            Rule::Date => text(value).is_some_and(|t| check::is_date(&t)),
            Rule::Uuid => text(value).is_some_and(|t| check::is_uuid(&t)),
            Rule::Array => matches!(value, Json::Array(_)),
            Rule::Min(bound) => measure(value, rules).is_some_and(|(size, _)| size >= *bound),
            Rule::Max(bound) => measure(value, rules).is_some_and(|(size, _)| size <= *bound),
            Rule::Between(low, high) => {
                measure(value, rules).is_some_and(|(size, _)| size >= *low && size <= *high)
            }
            Rule::Size(exact) => measure(value, rules).is_some_and(|(size, _)| size == *exact),
            Rule::In(allowed) => text(value).is_some_and(|t| allowed.contains(&t)),
            Rule::NotIn(denied) => text(value).is_some_and(|t| !denied.contains(&t)),
            Rule::StartsWith(prefixes) => {
                text(value).is_some_and(|t| prefixes.iter().any(|p| t.starts_with(p)))
            }
            Rule::EndsWith(suffixes) => {
                text(value).is_some_and(|t| suffixes.iter().any(|s| t.ends_with(s)))
            }
            Rule::Confirmed => self.compares(value, &format!("{field}_confirmation"), true),
            Rule::Same(other) => self.compares(value, other, true),
            Rule::Different(other) => self.compares(value, other, false),
        }
    }

    /// Compare against another field. The other field must exist either way:
    /// "must be different from a field you did not send" is not something a
    /// user can act on, so it fails rather than silently passing.
    fn compares(&self, value: &Json, other: &str, want_equal: bool) -> bool {
        self.input.get(other).is_some_and(|found| equivalent(value, found) == want_equal)
    }

    fn fail(&self, field: &str, rule: &Rule, kind: SizeKind, errors: &mut Errors) {
        let template = self.messages.template(field, rule, kind);
        let mut values = vec![("attribute", self.messages.label(field))];
        match rule {
            Rule::Min(bound) => values.push(("min", messages::format_number(*bound))),
            Rule::Max(bound) => values.push(("max", messages::format_number(*bound))),
            Rule::Between(low, high) => {
                values.push(("min", messages::format_number(*low)));
                values.push(("max", messages::format_number(*high)));
            }
            Rule::Size(exact) => values.push(("size", messages::format_number(*exact))),
            Rule::In(list) | Rule::NotIn(list) | Rule::StartsWith(list) | Rule::EndsWith(list) => {
                values.push(("values", messages::format_values(list)));
            }
            Rule::Same(other) | Rule::Different(other) => {
                values.push(("other", self.messages.label(other)));
            }
            _ => {}
        }
        errors.add(field, messages::interpolate(&template, &values));
    }
}

/// The fields that had rules and passed them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Validated {
    fields: BTreeMap<String, Json>,
}

impl Validated {
    pub fn get(&self, name: &str) -> Option<&Json> {
        self.fields.get(name)
    }

    pub fn has(&self, name: &str) -> bool {
        self.fields.contains_key(name)
    }

    /// A scalar as text. A number comes back as its digits, matching what
    /// `Request::input` hands back for the same field sent through a form.
    pub fn string(&self, name: &str) -> Option<String> {
        text(self.get(name)?)
    }

    pub fn integer(&self, name: &str) -> Option<i64> {
        let number = as_number(self.get(name)?)?;
        (number.fract() == 0.0).then_some(number as i64)
    }

    pub fn number(&self, name: &str) -> Option<f64> {
        as_number(self.get(name)?)
    }

    /// A boolean, accepting the `"1"`/`"0"` and `"true"`/`"false"` a form sends.
    pub fn boolean(&self, name: &str) -> Option<bool> {
        match self.get(name)? {
            Json::Bool(value) => Some(*value),
            Json::Number(number) => Some(*number != 0.0),
            Json::String(text) => match text.as_str() {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn array(&self, name: &str) -> Option<&[Json]> {
        self.get(name)?.as_array()
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

    /// The validated data as a JSON object, ready to echo back or store.
    pub fn into_json(self) -> Json {
        Json::Object(self.fields)
    }
}

impl From<Validated> for Json {
    fn from(validated: Validated) -> Self {
        validated.into_json()
    }
}

/// What `required` considers missing: null, a blank string, an empty array.
fn is_blank(value: &Json) -> bool {
    match value {
        Json::Null => true,
        Json::String(text) => text.trim().is_empty(),
        Json::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// A scalar rendered as text, so rules written for strings still work on the
/// numbers and booleans a JSON body carries.
fn text(value: &Json) -> Option<String> {
    match value {
        Json::String(found) => Some(found.clone()),
        Json::Number(_) | Json::Bool(_) => Some(value.to_string()),
        Json::Null | Json::Array(_) | Json::Object(_) => None,
    }
}

/// A number, whether it came from a JSON body or as the text of a form field.
///
/// Non-finite values are refused: Rust parses `"inf"` and `"NaN"` happily, and
/// neither is a number a user meant to type.
fn as_number(value: &Json) -> Option<f64> {
    match value {
        Json::Number(number) => Some(*number).filter(|n| n.is_finite()),
        Json::String(found) => found.trim().parse::<f64>().ok().filter(|n| n.is_finite()),
        _ => None,
    }
}

fn is_boolean(value: &Json) -> bool {
    match value {
        Json::Bool(_) => true,
        Json::Number(number) => *number == 0.0 || *number == 1.0,
        Json::String(found) => matches!(found.as_str(), "0" | "1" | "true" | "false"),
        _ => false,
    }
}

/// Equality for `confirmed`, `same` and `different`.
///
/// Compared as text first, so `18` from a JSON body and `"18"` from the form
/// that re-submitted it are the same value.
fn equivalent(left: &Json, right: &Json) -> bool {
    match (text(left), text(right)) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}

/// How big a value is, and which reading of "big" applied.
///
/// An array counts items. A field carrying `integer` or `numeric` is compared
/// by value, which is what makes `age|integer|min:18` accept the string `"18"`
/// a form sends. Anything else is measured in characters.
fn measure(value: &Json, rules: &Rules) -> Option<(f64, SizeKind)> {
    if let Json::Array(items) = value {
        return Some((items.len() as f64, SizeKind::Array));
    }
    if rules.has("integer") || rules.has("numeric") {
        return as_number(value).map(|number| (number, SizeKind::Numeric));
    }
    match value {
        Json::Number(number) => Some((*number, SizeKind::Numeric)),
        Json::String(found) => Some((found.chars().count() as f64, SizeKind::String)),
        _ => None,
    }
}

/// The reading a message should use, even for a value too odd to measure.
fn size_kind(value: &Json, rules: &Rules) -> SizeKind {
    if let Some((_, kind)) = measure(value, rules) {
        return kind;
    }
    if rules.has("integer") || rules.has("numeric") { SizeKind::Numeric } else { SizeKind::String }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_http::Method;

    /// Run one field's rules over one value and report the first message.
    fn message_for(value: Json, spec: &str) -> Option<String> {
        Validator::new(Input::new().with("field", value))
            .rule("field", spec)
            .errors()
            .first("field")
            .map(str::to_string)
    }

    fn passes(value: Json, spec: &str) -> bool {
        message_for(value, spec).is_none()
    }

    #[test]
    fn required_accepts_a_value_and_rejects_every_shape_of_emptiness() {
        assert!(passes(Json::from("ada"), "required"));
        assert!(passes(Json::from(0), "required"), "zero is a value");
        assert!(passes(Json::from(false), "required"), "false is a value");

        assert!(!passes(Json::Null, "required"));
        assert!(!passes(Json::from(""), "required"));
        assert!(!passes(Json::from("   "), "required"));
        assert!(!passes(Json::Array(vec![]), "required"));

        let missing = Validator::new(Input::new()).rule("field", "required").errors();
        assert_eq!(missing.first("field"), Some("The field field is required."));
    }

    #[test]
    fn an_absent_field_without_required_trips_nothing() {
        let errors = Validator::new(Input::new()).rule("age", "integer|min:18").errors();
        assert!(errors.is_empty());
    }

    #[test]
    fn nullable_lets_an_explicit_null_through_where_a_bare_rule_would_not() {
        assert!(passes(Json::Null, "nullable|integer"));
        assert!(!passes(Json::Null, "integer"));
    }

    #[test]
    fn nullable_and_required_together_still_demand_a_value() {
        assert!(!passes(Json::Null, "required|nullable|string"));
    }

    #[test]
    fn a_blank_string_is_treated_as_absent_by_every_rule_but_required() {
        assert!(passes(Json::from(""), "email|min:5"));
        assert!(!passes(Json::from(""), "required|email"));
    }

    #[test]
    fn string_accepts_text_and_rejects_other_json_types() {
        assert!(passes(Json::from("ada"), "string"));
        assert!(!passes(Json::from(7), "string"));
        assert!(!passes(Json::from(true), "string"));
        assert_eq!(
            message_for(Json::from(7), "string").unwrap(),
            "The field field must be a string."
        );
    }

    #[test]
    fn integer_accepts_a_whole_number_however_it_was_encoded() {
        assert!(passes(Json::from(18), "integer"));
        assert!(passes(Json::from("18"), "integer"), "a form sends numbers as text");
        assert!(passes(Json::from(-3), "integer"));

        assert!(!passes(Json::from(1.5), "integer"));
        assert!(!passes(Json::from("1.5"), "integer"));
        assert!(!passes(Json::from("eighteen"), "integer"));
        assert_eq!(
            message_for(Json::from("x"), "integer").unwrap(),
            "The field field must be an integer."
        );
    }

    #[test]
    fn numeric_accepts_fractions_but_not_words_or_infinities() {
        assert!(passes(Json::from(1.5), "numeric"));
        assert!(passes(Json::from("-2.75"), "numeric"));

        assert!(!passes(Json::from("abc"), "numeric"));
        assert!(!passes(Json::from("inf"), "numeric"), "`inf` parses in Rust but is not a number a user typed");
        assert!(!passes(Json::from(true), "numeric"));
    }

    #[test]
    fn boolean_accepts_the_forms_a_checkbox_arrives_in() {
        for value in [Json::from(true), Json::from(false), Json::from(1), Json::from(0)] {
            assert!(passes(value.clone(), "boolean"), "{value} should be boolean");
        }
        for value in ["1", "0", "true", "false"] {
            assert!(passes(Json::from(value), "boolean"), "{value} should be boolean");
        }

        assert!(!passes(Json::from("yes"), "boolean"));
        assert!(!passes(Json::from(2), "boolean"));
        assert_eq!(
            message_for(Json::from("yes"), "boolean").unwrap(),
            "The field field must be true or false."
        );
    }

    #[test]
    fn email_and_url_delegate_to_the_format_checks() {
        assert!(passes(Json::from("ada@example.com"), "email"));
        assert!(!passes(Json::from("ada@example"), "email"));
        assert_eq!(
            message_for(Json::from("nope"), "email").unwrap(),
            "The field field must be a valid email address."
        );

        assert!(passes(Json::from("https://example.com"), "url"));
        assert!(!passes(Json::from("example.com"), "url"));
        assert_eq!(
            message_for(Json::from("example.com"), "url").unwrap(),
            "The field field must be a valid URL."
        );
    }

    #[test]
    fn min_counts_characters_for_a_string() {
        assert!(passes(Json::from("abc"), "min:3"));
        assert!(!passes(Json::from("ab"), "min:3"));
        assert_eq!(
            message_for(Json::from("ab"), "min:3").unwrap(),
            "The field field must be at least 3 characters."
        );
    }

    #[test]
    fn min_compares_values_when_the_field_is_a_number() {
        assert!(passes(Json::from(18), "integer|min:18"));
        assert!(passes(Json::from("18"), "integer|min:18"), "a form value is still a number");
        assert!(!passes(Json::from(17), "integer|min:18"));
        assert_eq!(
            message_for(Json::from(17), "integer|min:18").unwrap(),
            "The field field must be at least 18."
        );
    }

    #[test]
    fn min_counts_items_for_an_array() {
        assert!(passes(Json::Array(vec![Json::from("a"), Json::from("b")]), "array|min:2"));
        assert_eq!(
            message_for(Json::Array(vec![Json::from("a")]), "array|min:2").unwrap(),
            "The field field must have at least 2 items."
        );
    }

    #[test]
    fn max_mirrors_min_across_the_same_three_readings() {
        assert!(passes(Json::from("abc"), "max:3"));
        assert!(!passes(Json::from("abcd"), "max:3"));
        assert_eq!(
            message_for(Json::from("abcd"), "max:3").unwrap(),
            "The field field must not be greater than 3 characters."
        );

        assert!(passes(Json::from(3), "integer|max:3"));
        assert_eq!(
            message_for(Json::from(4), "integer|max:3").unwrap(),
            "The field field must not be greater than 3."
        );

        assert_eq!(
            message_for(Json::Array(vec![Json::from(1), Json::from(2)]), "array|max:1").unwrap(),
            "The field field must not have more than 1 items."
        );
    }

    #[test]
    fn between_is_inclusive_on_both_ends() {
        assert!(passes(Json::from(1), "integer|between:1,10"));
        assert!(passes(Json::from(10), "integer|between:1,10"));
        assert!(!passes(Json::from(11), "integer|between:1,10"));
        assert_eq!(
            message_for(Json::from(0), "integer|between:1,10").unwrap(),
            "The field field must be between 1 and 10."
        );
        assert_eq!(
            message_for(Json::from("ab"), "between:3,5").unwrap(),
            "The field field must be between 3 and 5 characters."
        );
    }

    #[test]
    fn size_demands_an_exact_length_value_or_count() {
        assert!(passes(Json::from("abcdef"), "size:6"));
        assert!(!passes(Json::from("abcde"), "size:6"));
        assert_eq!(
            message_for(Json::from("abcde"), "size:6").unwrap(),
            "The field field must be 6 characters."
        );
        assert!(passes(Json::from(6), "integer|size:6"));
        assert!(passes(Json::Array(vec![Json::from(1)]), "array|size:1"));
    }

    #[test]
    fn in_and_not_in_match_against_the_listed_values() {
        assert!(passes(Json::from("draft"), "in:draft,published"));
        assert!(!passes(Json::from("deleted"), "in:draft,published"));
        assert_eq!(
            message_for(Json::from("deleted"), "in:draft,published").unwrap(),
            "The selected field is invalid."
        );

        assert!(passes(Json::from("ada"), "not_in:admin,root"));
        assert!(!passes(Json::from("root"), "not_in:admin,root"));
    }

    #[test]
    fn alpha_families_reject_the_characters_they_exclude() {
        assert!(passes(Json::from("Ada"), "alpha"));
        assert!(!passes(Json::from("Ada2"), "alpha"));
        assert_eq!(
            message_for(Json::from("Ada2"), "alpha").unwrap(),
            "The field field must only contain letters."
        );

        assert!(passes(Json::from("Ada2"), "alpha_num"));
        assert!(!passes(Json::from("Ada-2"), "alpha_num"));

        assert!(passes(Json::from("ada-2_x"), "alpha_dash"));
        assert!(!passes(Json::from("ada 2"), "alpha_dash"));
    }

    #[test]
    fn starts_with_and_ends_with_accept_any_of_their_options() {
        assert!(passes(Json::from("https://x.dev"), "starts_with:http,https"));
        assert!(!passes(Json::from("ftp://x.dev"), "starts_with:http,https"));
        assert_eq!(
            message_for(Json::from("ftp://x.dev"), "starts_with:http,https").unwrap(),
            "The field field must start with one of the following: http, https."
        );

        assert!(passes(Json::from("a@b.dev"), "ends_with:.com,.dev"));
        assert!(!passes(Json::from("a@b.net"), "ends_with:.com,.dev"));
        assert_eq!(
            message_for(Json::from("a@b.net"), "ends_with:.com,.dev").unwrap(),
            "The field field must end with one of the following: .com, .dev."
        );
    }

    #[test]
    fn date_uuid_and_array_report_their_own_shapes() {
        assert!(passes(Json::from("2024-02-29"), "date"));
        assert!(!passes(Json::from("2023-02-29"), "date"));
        assert_eq!(
            message_for(Json::from("nope"), "date").unwrap(),
            "The field field must be a valid date in the format YYYY-MM-DD."
        );

        assert!(passes(Json::from("9f8b2c1a-4d3e-4f5a-8b7c-1d2e3f4a5b6c"), "uuid"));
        assert!(!passes(Json::from("not-a-uuid"), "uuid"));
        assert_eq!(
            message_for(Json::from("x"), "uuid").unwrap(),
            "The field field must be a valid UUID."
        );

        assert!(passes(Json::Array(vec![Json::from(1)]), "array"));
        assert!(!passes(Json::from("a,b"), "array"));
        assert_eq!(
            message_for(Json::from("a"), "array").unwrap(),
            "The field field must be an array."
        );
    }

    #[test]
    fn confirmed_looks_for_the_matching_confirmation_field() {
        let input = Input::new().with("password", "secret").with("password_confirmation", "secret");
        assert!(Validator::new(input).rule("password", "confirmed").passes());

        let wrong = Input::new().with("password", "secret").with("password_confirmation", "typo");
        let errors = Validator::new(wrong).rule("password", "confirmed").errors();
        assert_eq!(errors.first("password"), Some("The password field confirmation does not match."));

        // A confirmation that was never sent fails just as a mismatch does.
        let absent = Input::new().with("password", "secret");
        assert!(Validator::new(absent).rule("password", "confirmed").fails());
    }

    #[test]
    fn same_and_different_compare_against_another_field() {
        let input = Input::new().with("password", "secret").with("repeat", "secret");
        assert!(Validator::new(input.clone()).rule("repeat", "same:password").passes());

        let errors = Validator::new(input).rule("repeat", "different:password").errors();
        assert_eq!(errors.first("repeat"), Some("The repeat field and password must be different."));

        let differing = Input::new().with("username", "ada").with("password", "secret");
        assert!(Validator::new(differing.clone()).rule("password", "different:username").passes());

        let errors = Validator::new(differing).rule("password", "same:username").errors();
        assert_eq!(errors.first("password"), Some("The password field must match username."));
    }

    #[test]
    fn same_compares_a_json_number_with_the_text_a_form_would_resend() {
        let input = Input::new().with("total", 42).with("confirm_total", "42");
        assert!(Validator::new(input).rule("confirm_total", "same:total").passes());
    }

    #[test]
    fn every_failing_rule_on_a_field_is_reported() {
        let errors = Validator::new(Input::new().with("email", "nope"))
            .rule("email", "email|min:20")
            .errors();

        assert_eq!(errors.get("email").len(), 2);
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn a_custom_message_replaces_the_default_for_one_field_and_rule() {
        let errors = Validator::new(Input::new())
            .rule("email", "required")
            .rule("name", "required")
            .message("email.required", "We cannot reach you without an email.")
            .errors();

        assert_eq!(errors.first("email"), Some("We cannot reach you without an email."));
        assert_eq!(errors.first("name"), Some("The name field is required."));
    }

    #[test]
    fn a_custom_message_keeps_its_placeholders() {
        let errors = Validator::new(Input::new().with("age", 12))
            .rule("age", "integer|min:18")
            .message("min", "You must be :min or older to sign up (:attribute).")
            .errors();

        assert_eq!(errors.first("age"), Some("You must be 18 or older to sign up (age)."));
    }

    #[test]
    fn an_attribute_override_reaches_the_rendered_message() {
        let errors = Validator::new(Input::new())
            .rule("dob", "required")
            .attribute("dob", "date of birth")
            .errors();

        assert_eq!(errors.first("dob"), Some("The date of birth field is required."));
    }

    #[test]
    fn a_snake_case_field_reads_as_words_without_any_configuration() {
        let errors = Validator::new(Input::new()).rule("email_address", "required").errors();
        assert_eq!(errors.first("email_address"), Some("The email address field is required."));
    }

    #[test]
    fn validated_data_is_the_checked_subset_and_nothing_else() {
        let input = Input::new()
            .with("email", "ada@example.com")
            .with("age", 36)
            .with("is_admin", true);

        let data = Validator::new(input)
            .rules(&[("email", "required|email"), ("age", "integer")])
            .validate()
            .unwrap();

        assert_eq!(data.len(), 2);
        assert!(!data.has("is_admin"), "a field with no rules must not ride along");
        assert_eq!(data.string("email").as_deref(), Some("ada@example.com"));
        assert_eq!(data.integer("age"), Some(36));
    }

    #[test]
    fn validated_accessors_read_the_forms_a_wire_value_arrives_in() {
        let input = Input::new()
            .with("age", "36")
            .with("rate", "1.5")
            .with("active", "true")
            .with("tags", Json::Array(vec![Json::from("a")]));

        let data = Validator::new(input)
            .rules(&[("age", "integer"), ("rate", "numeric"), ("active", "boolean"), ("tags", "array")])
            .validate()
            .unwrap();

        assert_eq!(data.integer("age"), Some(36));
        assert_eq!(data.number("rate"), Some(1.5));
        assert_eq!(data.boolean("active"), Some(true));
        assert_eq!(data.array("tags").unwrap().len(), 1);
        assert_eq!(data.string("age").as_deref(), Some("36"));
        assert_eq!(data.integer("missing"), None);
    }

    #[test]
    fn validated_data_converts_to_a_json_object() {
        let data = Validator::new(Input::new().with("name", "ada"))
            .rule("name", "required|string")
            .validate()
            .unwrap();

        assert_eq!(Json::from(data).to_string(), r#"{"name":"ada"}"#);
    }

    #[test]
    fn a_nullable_field_that_was_sent_as_null_is_still_returned() {
        let data = Validator::new(Input::new().with("nickname", Json::Null))
            .rule("nickname", "nullable|string")
            .validate()
            .unwrap();

        assert!(data.has("nickname"));
        assert!(data.get("nickname").unwrap().is_null());
        assert_eq!(data.string("nickname"), None);
    }

    #[test]
    fn the_builder_and_the_string_syntax_validate_identically() {
        let input = Input::new().with("email", "nope").with("age", 12);
        let specs = Validator::new(input.clone())
            .rules(&[("email", "required|email"), ("age", "integer|min:18")])
            .errors();
        let built = Validator::new(input)
            .rule("email", Rule::required().email())
            .rule("age", Rule::integer().min(18))
            .errors();

        assert_eq!(specs, built);
    }

    #[test]
    fn validating_a_request_reads_a_json_body() {
        let mut request = Request::new(Method::Post, "/users")
            .with_json(Json::object([("email", "ada@example.com".into()), ("age", 36.into())]));

        let data = Validator::from_request(&mut request)
            .rules(&[("email", "required|email"), ("age", "required|integer|min:18")])
            .validate()
            .unwrap();

        assert_eq!(data.string("email").as_deref(), Some("ada@example.com"));
        assert_eq!(data.integer("age"), Some(36));
    }

    #[test]
    fn validating_a_request_reads_a_form_body() {
        let mut request = Request::new(Method::Post, "/register").with_form(&[
            ("email", "ada@example.com"),
            ("password", "secret123"),
            ("password_confirmation", "secret123"),
        ]);

        let data = Validator::from_request(&mut request)
            .rules(&[("email", "required|email"), ("password", "required|min:8|confirmed")])
            .validate()
            .unwrap();

        assert_eq!(data.len(), 2);
        assert_eq!(data.string("password").as_deref(), Some("secret123"));
    }

    #[test]
    fn a_failing_form_request_reports_every_field() {
        let mut request = Request::new(Method::Post, "/register")
            .with_form(&[("email", "nope"), ("password", "short"), ("password_confirmation", "other")]);

        let errors = Validator::from_request(&mut request)
            .rules(&[("email", "required|email"), ("password", "required|min:8|confirmed")])
            .validate()
            .unwrap_err();

        assert_eq!(errors.first("email"), Some("The email field must be a valid email address."));
        assert_eq!(errors.get("password").len(), 2);
        assert!(!errors.wants_json(), "a form post is a browser, not an API client");
    }

    #[test]
    fn a_json_request_is_remembered_as_wanting_a_json_failure() {
        let mut request =
            Request::new(Method::Post, "/api/users").with_json(Json::object([("email", "no".into())]));

        let errors = Validator::from_request(&mut request).rule("email", "email").validate().unwrap_err();
        assert!(errors.wants_json());
    }

    #[test]
    fn query_string_values_validate_like_any_other_input() {
        let mut request = Request::new(Method::Get, "/search?page=2&per_page=500");

        let errors = Validator::from_request(&mut request)
            .rules(&[("page", "integer|min:1"), ("per_page", "integer|max:100")])
            .validate()
            .unwrap_err();

        assert!(!errors.has("page"));
        assert_eq!(errors.first("per_page"), Some("The per page field must not be greater than 100."));
    }
}
