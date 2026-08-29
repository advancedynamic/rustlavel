//! Default messages, placeholder interpolation, and per-field overrides.
//!
//! Laravel keeps its messages in `lang/en/validation.php` with `:attribute`
//! style placeholders, and lets an application override one message for one
//! field. That split matters: the defaults have to be good enough that nobody
//! writes them out again, and the override has to be reachable for the one
//! field where the default reads wrong ("The g-recaptcha-response field is
//! required" is never what a user should see).

use crate::rule::Rule;
use std::collections::BTreeMap;

/// Which of the three readings of a size rule applies to a value.
///
/// `min:3` means three characters, three items, or the number three depending
/// on what is being validated, and the message has to say the right one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeKind {
    Numeric,
    String,
    Array,
}

/// Message defaults, plus whatever the application overrode.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Messages {
    overrides: BTreeMap<String, String>,
    attributes: BTreeMap<String, String>,
}

impl Messages {
    pub fn new() -> Self {
        Messages::default()
    }

    /// Override a message.
    ///
    /// The key is `"field.rule"` for one field (`"email.required"`) or just
    /// `"rule"` to replace the default everywhere (`"required"`). Placeholders
    /// still interpolate, so an override can keep `:attribute`.
    pub fn set(&mut self, key: impl Into<String>, message: impl Into<String>) {
        self.overrides.insert(key.into(), message.into());
    }

    /// The chaining form of [`Messages::set`].
    pub fn with(mut self, key: impl Into<String>, message: impl Into<String>) -> Self {
        self.set(key, message);
        self
    }

    /// Rename a field for display: `attribute("dob", "date of birth")`.
    pub fn set_attribute(&mut self, field: impl Into<String>, label: impl Into<String>) {
        self.attributes.insert(field.into(), label.into());
    }

    /// The chaining form of [`Messages::set_attribute`].
    pub fn attribute(mut self, field: impl Into<String>, label: impl Into<String>) -> Self {
        self.set_attribute(field, label);
        self
    }

    /// How a field is named in a message.
    ///
    /// Without an override, `email_address` reads as "email address" — a form
    /// field name is a programmer's word, and the message is a user's.
    pub fn label(&self, field: &str) -> String {
        match self.attributes.get(field) {
            Some(label) => label.clone(),
            None => field.replace(['_', '-', '.'], " "),
        }
    }

    /// The template for a field/rule pair: a per-field override, then a
    /// per-rule override, then the built-in default.
    pub fn template(&self, field: &str, rule: &Rule, kind: SizeKind) -> String {
        let name = rule.name();
        self.overrides
            .get(&format!("{field}.{name}"))
            .or_else(|| self.overrides.get(name))
            .cloned()
            .unwrap_or_else(|| default_template(rule, kind).to_string())
    }
}

/// The built-in English message for a rule, adapted from Laravel's `en` set.
pub fn default_template(rule: &Rule, kind: SizeKind) -> &'static str {
    match rule {
        Rule::Required => "The :attribute field is required.",
        // `nullable` never fails; it only permits a null.
        Rule::Nullable => "The :attribute field is invalid.",
        Rule::String => "The :attribute field must be a string.",
        Rule::Integer => "The :attribute field must be an integer.",
        Rule::Numeric => "The :attribute field must be a number.",
        Rule::Boolean => "The :attribute field must be true or false.",
        Rule::Email => "The :attribute field must be a valid email address.",
        Rule::Url => "The :attribute field must be a valid URL.",
        Rule::Min(_) => match kind {
            SizeKind::Numeric => "The :attribute field must be at least :min.",
            SizeKind::String => "The :attribute field must be at least :min characters.",
            SizeKind::Array => "The :attribute field must have at least :min items.",
        },
        Rule::Max(_) => match kind {
            SizeKind::Numeric => "The :attribute field must not be greater than :max.",
            SizeKind::String => "The :attribute field must not be greater than :max characters.",
            SizeKind::Array => "The :attribute field must not have more than :max items.",
        },
        Rule::Between(_, _) => match kind {
            SizeKind::Numeric => "The :attribute field must be between :min and :max.",
            SizeKind::String => "The :attribute field must be between :min and :max characters.",
            SizeKind::Array => "The :attribute field must have between :min and :max items.",
        },
        Rule::Size(_) => match kind {
            SizeKind::Numeric => "The :attribute field must be :size.",
            SizeKind::String => "The :attribute field must be :size characters.",
            SizeKind::Array => "The :attribute field must contain :size items.",
        },
        Rule::In(_) | Rule::NotIn(_) => "The selected :attribute is invalid.",
        Rule::Confirmed => "The :attribute field confirmation does not match.",
        Rule::Same(_) => "The :attribute field must match :other.",
        Rule::Different(_) => "The :attribute field and :other must be different.",
        Rule::Alpha => "The :attribute field must only contain letters.",
        Rule::AlphaNum => "The :attribute field must only contain letters and numbers.",
        Rule::AlphaDash => {
            "The :attribute field must only contain letters, numbers, dashes, and underscores."
        }
        Rule::StartsWith(_) => "The :attribute field must start with one of the following: :values.",
        Rule::EndsWith(_) => "The :attribute field must end with one of the following: :values.",
        Rule::Date => "The :attribute field must be a valid date in the format YYYY-MM-DD.",
        Rule::Uuid => "The :attribute field must be a valid UUID.",
        Rule::Array => "The :attribute field must be an array.",
    }
}

/// Replace `:name` placeholders. Unknown placeholders are left alone so a typo
/// in an override is visible in the output instead of silently vanishing.
pub fn interpolate(template: &str, values: &[(&str, String)]) -> String {
    let mut out = template.to_string();
    for (name, value) in values {
        out = out.replace(&format!(":{name}"), value);
    }
    out
}

/// Render a bound the way a person writes it: `3`, not `3.0`.
pub fn format_number(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Join rule parameters for the `:values` placeholder.
pub fn format_values(values: &[String]) -> String {
    values.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_name_reads_as_words_by_default() {
        let messages = Messages::new();
        assert_eq!(messages.label("email"), "email");
        assert_eq!(messages.label("email_address"), "email address");
        assert_eq!(messages.label("billing.postal-code"), "billing postal code");
    }

    #[test]
    fn an_attribute_override_replaces_the_derived_label() {
        let messages = Messages::new().attribute("dob", "date of birth");
        assert_eq!(messages.label("dob"), "date of birth");
        assert_eq!(messages.label("other"), "other");
    }

    #[test]
    fn a_field_override_beats_a_rule_override_which_beats_the_default() {
        let messages = Messages::new()
            .with("required", "We need :attribute.")
            .with("email.required", "An email address is required.");

        assert_eq!(
            messages.template("email", &Rule::Required, SizeKind::String),
            "An email address is required."
        );
        assert_eq!(messages.template("name", &Rule::Required, SizeKind::String), "We need :attribute.");
        assert_eq!(
            messages.template("name", &Rule::Email, SizeKind::String),
            "The :attribute field must be a valid email address."
        );
    }

    #[test]
    fn a_size_rule_picks_its_wording_from_the_kind_of_value() {
        assert_eq!(
            default_template(&Rule::Min(3.0), SizeKind::String),
            "The :attribute field must be at least :min characters."
        );
        assert_eq!(
            default_template(&Rule::Min(3.0), SizeKind::Numeric),
            "The :attribute field must be at least :min."
        );
        assert_eq!(
            default_template(&Rule::Min(3.0), SizeKind::Array),
            "The :attribute field must have at least :min items."
        );
    }

    #[test]
    fn placeholders_are_interpolated_and_unknown_ones_are_left_visible() {
        let rendered = interpolate(
            "The :attribute field must be between :min and :max. :nope",
            &[("attribute", "age".into()), ("min", "1".into()), ("max", "10".into())],
        );
        assert_eq!(rendered, "The age field must be between 1 and 10. :nope");
    }

    #[test]
    fn bounds_render_without_a_decimal_point() {
        assert_eq!(format_number(255.0), "255");
        assert_eq!(format_number(1.5), "1.5");
    }

    #[test]
    fn list_parameters_render_comma_separated() {
        assert_eq!(format_values(&["a".to_string(), "b".to_string()]), "a, b");
    }
}
