//! The rules themselves, and the two ways to declare them.
//!
//! A rule set can be written the way it is in Laravel — `"required|email|max:255"`
//! — or built up with methods: `Rule::required().email().max(255)`. Both land on
//! the same [`Rules`] value, so nothing downstream has to care which was used.
//! The string form is what a developer already knows and what a config file or
//! a generator can emit; the builder form is what the compiler can check, so a
//! misspelled rule is a compile error instead of a runtime panic.

use rustlavel_core::{Error, Result};

/// One validation rule, with its parameters already parsed.
///
/// This is the single internal representation: the string parser and the
/// builder are two front doors onto the same enum.
#[derive(Debug, Clone, PartialEq)]
pub enum Rule {
    /// The field must be present and not blank.
    Required,
    /// An explicit `null` is allowed, and skips the remaining rules.
    Nullable,
    String,
    Integer,
    Numeric,
    Boolean,
    Email,
    Url,
    /// Minimum length for a string, value for a number, item count for an array.
    Min(f64),
    /// Maximum length for a string, value for a number, item count for an array.
    Max(f64),
    /// Inclusive range, measured the same way as [`Rule::Min`].
    Between(f64, f64),
    /// Exact length, value, or item count.
    Size(f64),
    In(Vec<String>),
    NotIn(Vec<String>),
    /// A `<field>_confirmation` field must be present and equal.
    Confirmed,
    /// Must equal another field.
    Same(String),
    /// Must differ from another field.
    Different(String),
    Alpha,
    AlphaNum,
    AlphaDash,
    StartsWith(Vec<String>),
    EndsWith(Vec<String>),
    /// A `YYYY-MM-DD` calendar date.
    Date,
    Uuid,
    Array,
}

impl Rule {
    /// The name this rule is written as in the string syntax, which is also the
    /// key used to look up its message and any per-field override.
    pub fn name(&self) -> &'static str {
        match self {
            Rule::Required => "required",
            Rule::Nullable => "nullable",
            Rule::String => "string",
            Rule::Integer => "integer",
            Rule::Numeric => "numeric",
            Rule::Boolean => "boolean",
            Rule::Email => "email",
            Rule::Url => "url",
            Rule::Min(_) => "min",
            Rule::Max(_) => "max",
            Rule::Between(_, _) => "between",
            Rule::Size(_) => "size",
            Rule::In(_) => "in",
            Rule::NotIn(_) => "not_in",
            Rule::Confirmed => "confirmed",
            Rule::Same(_) => "same",
            Rule::Different(_) => "different",
            Rule::Alpha => "alpha",
            Rule::AlphaNum => "alpha_num",
            Rule::AlphaDash => "alpha_dash",
            Rule::StartsWith(_) => "starts_with",
            Rule::EndsWith(_) => "ends_with",
            Rule::Date => "date",
            Rule::Uuid => "uuid",
            Rule::Array => "array",
        }
    }

    // --- Builder entry points. Each starts a rule set that keeps chaining. ---

    pub fn required() -> Rules {
        Rules::new().required()
    }

    pub fn nullable() -> Rules {
        Rules::new().nullable()
    }

    pub fn string() -> Rules {
        Rules::new().string()
    }

    pub fn integer() -> Rules {
        Rules::new().integer()
    }

    pub fn numeric() -> Rules {
        Rules::new().numeric()
    }

    pub fn boolean() -> Rules {
        Rules::new().boolean()
    }

    pub fn email() -> Rules {
        Rules::new().email()
    }

    pub fn url() -> Rules {
        Rules::new().url()
    }

    pub fn min(bound: impl Into<f64>) -> Rules {
        Rules::new().min(bound)
    }

    pub fn max(bound: impl Into<f64>) -> Rules {
        Rules::new().max(bound)
    }

    pub fn between(low: impl Into<f64>, high: impl Into<f64>) -> Rules {
        Rules::new().between(low, high)
    }

    pub fn size(exact: impl Into<f64>) -> Rules {
        Rules::new().size(exact)
    }

    /// The builder spelling of `in:a,b,c`; `in` is a Rust keyword.
    pub fn one_of<S: Into<String>>(values: impl IntoIterator<Item = S>) -> Rules {
        Rules::new().one_of(values)
    }

    pub fn not_in<S: Into<String>>(values: impl IntoIterator<Item = S>) -> Rules {
        Rules::new().not_in(values)
    }

    pub fn confirmed() -> Rules {
        Rules::new().confirmed()
    }

    pub fn same(other: impl Into<String>) -> Rules {
        Rules::new().same(other)
    }

    pub fn different(other: impl Into<String>) -> Rules {
        Rules::new().different(other)
    }

    pub fn alpha() -> Rules {
        Rules::new().alpha()
    }

    pub fn alpha_num() -> Rules {
        Rules::new().alpha_num()
    }

    pub fn alpha_dash() -> Rules {
        Rules::new().alpha_dash()
    }

    pub fn starts_with<S: Into<String>>(prefixes: impl IntoIterator<Item = S>) -> Rules {
        Rules::new().starts_with(prefixes)
    }

    pub fn ends_with<S: Into<String>>(suffixes: impl IntoIterator<Item = S>) -> Rules {
        Rules::new().ends_with(suffixes)
    }

    pub fn date() -> Rules {
        Rules::new().date()
    }

    pub fn uuid() -> Rules {
        Rules::new().uuid()
    }

    pub fn array() -> Rules {
        Rules::new().array()
    }
}

/// An ordered set of rules for one field.
///
/// Order is preserved because it is the order the messages come back in, and a
/// developer who wrote `required|email` expects to be told about the missing
/// field before the malformed one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rules {
    rules: Vec<Rule>,
}

impl Rules {
    pub fn new() -> Self {
        Rules::default()
    }

    /// Parse Laravel's pipe/colon syntax: `"required|between:1,10|in:a,b,c"`.
    ///
    /// Whitespace around a rule is ignored and empty segments are dropped, so a
    /// spec split across lines in a config file still parses.
    pub fn parse(spec: &str) -> Result<Rules> {
        let mut rules = Rules::new();
        for segment in spec.split('|') {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }
            let (name, parameters) = match segment.split_once(':') {
                Some((name, rest)) => (name.trim(), Some(rest)),
                None => (segment, None),
            };
            rules.rules.push(parse_rule(name, parameters)?);
        }
        Ok(rules)
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether a rule with this name is in the set. The validator asks this to
    /// find `nullable`, and to decide whether `min` counts characters or value.
    pub fn has(&self, name: &str) -> bool {
        self.rules.iter().any(|rule| rule.name() == name)
    }

    /// Append an already-built rule. The chaining methods below all go through here.
    pub fn push(mut self, rule: Rule) -> Self {
        self.rules.push(rule);
        self
    }

    pub fn required(self) -> Self {
        self.push(Rule::Required)
    }

    pub fn nullable(self) -> Self {
        self.push(Rule::Nullable)
    }

    pub fn string(self) -> Self {
        self.push(Rule::String)
    }

    pub fn integer(self) -> Self {
        self.push(Rule::Integer)
    }

    pub fn numeric(self) -> Self {
        self.push(Rule::Numeric)
    }

    pub fn boolean(self) -> Self {
        self.push(Rule::Boolean)
    }

    pub fn email(self) -> Self {
        self.push(Rule::Email)
    }

    pub fn url(self) -> Self {
        self.push(Rule::Url)
    }

    pub fn min(self, bound: impl Into<f64>) -> Self {
        self.push(Rule::Min(bound.into()))
    }

    pub fn max(self, bound: impl Into<f64>) -> Self {
        self.push(Rule::Max(bound.into()))
    }

    pub fn between(self, low: impl Into<f64>, high: impl Into<f64>) -> Self {
        self.push(Rule::Between(low.into(), high.into()))
    }

    pub fn size(self, exact: impl Into<f64>) -> Self {
        self.push(Rule::Size(exact.into()))
    }

    /// The builder spelling of `in:a,b,c`; `in` is a Rust keyword.
    pub fn one_of<S: Into<String>>(self, values: impl IntoIterator<Item = S>) -> Self {
        self.push(Rule::In(collect(values)))
    }

    pub fn not_in<S: Into<String>>(self, values: impl IntoIterator<Item = S>) -> Self {
        self.push(Rule::NotIn(collect(values)))
    }

    pub fn confirmed(self) -> Self {
        self.push(Rule::Confirmed)
    }

    pub fn same(self, other: impl Into<String>) -> Self {
        self.push(Rule::Same(other.into()))
    }

    pub fn different(self, other: impl Into<String>) -> Self {
        self.push(Rule::Different(other.into()))
    }

    pub fn alpha(self) -> Self {
        self.push(Rule::Alpha)
    }

    pub fn alpha_num(self) -> Self {
        self.push(Rule::AlphaNum)
    }

    pub fn alpha_dash(self) -> Self {
        self.push(Rule::AlphaDash)
    }

    pub fn starts_with<S: Into<String>>(self, prefixes: impl IntoIterator<Item = S>) -> Self {
        self.push(Rule::StartsWith(collect(prefixes)))
    }

    pub fn ends_with<S: Into<String>>(self, suffixes: impl IntoIterator<Item = S>) -> Self {
        self.push(Rule::EndsWith(collect(suffixes)))
    }

    pub fn date(self) -> Self {
        self.push(Rule::Date)
    }

    pub fn uuid(self) -> Self {
        self.push(Rule::Uuid)
    }

    pub fn array(self) -> Self {
        self.push(Rule::Array)
    }
}

impl std::str::FromStr for Rules {
    type Err = Error;

    fn from_str(spec: &str) -> Result<Rules> {
        Rules::parse(spec)
    }
}

impl FromIterator<Rule> for Rules {
    fn from_iter<I: IntoIterator<Item = Rule>>(rules: I) -> Self {
        Rules { rules: rules.into_iter().collect() }
    }
}

/// Anything that can name a rule set when registering a field.
///
/// This is what lets `validator.rule("email", "required|email")` and
/// `validator.rule("email", Rule::required().email())` sit side by side.
pub trait IntoRules {
    fn into_rules(self) -> Rules;
}

impl IntoRules for Rules {
    fn into_rules(self) -> Rules {
        self
    }
}

impl IntoRules for Rule {
    fn into_rules(self) -> Rules {
        Rules::new().push(self)
    }
}

/// A malformed spec is a bug in the source, not bad user input, so it panics
/// with the parser's message rather than turning into a validation error a user
/// would see. Use [`Rules::parse`] directly when the spec comes from data.
impl IntoRules for &str {
    fn into_rules(self) -> Rules {
        Rules::parse(self).unwrap_or_else(|error| panic!("invalid validation rules `{self}`: {error}"))
    }
}

impl IntoRules for String {
    fn into_rules(self) -> Rules {
        self.as_str().into_rules()
    }
}

fn collect<S: Into<String>>(values: impl IntoIterator<Item = S>) -> Vec<String> {
    values.into_iter().map(Into::into).collect()
}

fn parse_rule(name: &str, parameters: Option<&str>) -> Result<Rule> {
    match name {
        "required" => Ok(Rule::Required),
        "nullable" => Ok(Rule::Nullable),
        "string" => Ok(Rule::String),
        "integer" => Ok(Rule::Integer),
        "numeric" => Ok(Rule::Numeric),
        "boolean" => Ok(Rule::Boolean),
        "email" => Ok(Rule::Email),
        "url" => Ok(Rule::Url),
        "confirmed" => Ok(Rule::Confirmed),
        "alpha" => Ok(Rule::Alpha),
        "alpha_num" => Ok(Rule::AlphaNum),
        "alpha_dash" => Ok(Rule::AlphaDash),
        "date" => Ok(Rule::Date),
        "uuid" => Ok(Rule::Uuid),
        "array" => Ok(Rule::Array),
        "min" => Ok(Rule::Min(number(name, parameters, "min:3")?)),
        "max" => Ok(Rule::Max(number(name, parameters, "max:255")?)),
        "size" => Ok(Rule::Size(number(name, parameters, "size:6")?)),
        "between" => {
            let values = list(name, parameters, "between:1,10")?;
            if values.len() != 2 {
                return Err(Error::msg(
                    "validation rule `between` takes exactly two numbers, like `between:1,10`",
                ));
            }
            Ok(Rule::Between(parse_number(name, &values[0])?, parse_number(name, &values[1])?))
        }
        "in" => Ok(Rule::In(list(name, parameters, "in:draft,published")?)),
        "not_in" => Ok(Rule::NotIn(list(name, parameters, "not_in:admin,root")?)),
        "same" => Ok(Rule::Same(single(name, parameters, "same:password")?)),
        "different" => Ok(Rule::Different(single(name, parameters, "different:username")?)),
        "starts_with" => Ok(Rule::StartsWith(list(name, parameters, "starts_with:https")?)),
        "ends_with" => Ok(Rule::EndsWith(list(name, parameters, "ends_with:.com")?)),
        other => Err(Error::msg(match suggest(other) {
            Some(guess) => format!("unknown validation rule `{other}` — did you mean `{guess}`?"),
            None => format!("unknown validation rule `{other}`"),
        })),
    }
}

/// The comma-separated parameters of a rule, e.g. `a,b,c` in `in:a,b,c`.
fn list(name: &str, parameters: Option<&str>, example: &str) -> Result<Vec<String>> {
    let raw = parameters.ok_or_else(|| missing(name, example))?;
    let values: Vec<String> = raw.split(',').map(|value| value.trim().to_string()).collect();
    if values.iter().all(String::is_empty) {
        return Err(missing(name, example));
    }
    Ok(values)
}

fn single(name: &str, parameters: Option<&str>, example: &str) -> Result<String> {
    let value = parameters.map(str::trim).unwrap_or_default();
    if value.is_empty() {
        return Err(missing(name, example));
    }
    Ok(value.to_string())
}

fn number(name: &str, parameters: Option<&str>, example: &str) -> Result<f64> {
    parse_number(name, &single(name, parameters, example)?)
}

fn parse_number(name: &str, value: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .map_err(|_| Error::msg(format!("validation rule `{name}` expects a number, got `{value}`")))
}

fn missing(name: &str, example: &str) -> Error {
    Error::msg(format!("validation rule `{name}` needs a parameter, like `{example}`"))
}

/// The closest known rule name, for the "did you mean" hint.
///
/// Only near misses are offered — suggesting `size` for `nonsense` would be
/// worse than saying nothing.
fn suggest(unknown: &str) -> Option<&'static str> {
    const NAMES: [&str; 25] = [
        "required", "nullable", "string", "integer", "numeric", "boolean", "email", "url", "min",
        "max", "between", "size", "in", "not_in", "confirmed", "same", "different", "alpha",
        "alpha_num", "alpha_dash", "starts_with", "ends_with", "date", "uuid", "array",
    ];
    let limit = if unknown.len() <= 4 { 1 } else { 2 };
    NAMES
        .iter()
        .map(|name| (distance(unknown, name), *name))
        .filter(|(distance, _)| *distance <= limit)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, name)| name)
}

/// Levenshtein distance, the two-row variant — enough for word-sized inputs.
fn distance(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0usize; right.len() + 1];

    for (i, l) in left.iter().enumerate() {
        current[0] = i + 1;
        for (j, r) in right.iter().enumerate() {
            let substitution = previous[j] + usize::from(l != r);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_laravel_string_syntax() {
        let rules = Rules::parse("required|email|max:255").unwrap();
        assert_eq!(rules.rules(), [Rule::Required, Rule::Email, Rule::Max(255.0)]);
    }

    #[test]
    fn parses_a_list_parameter() {
        let rules = Rules::parse("in:draft,published,archived").unwrap();
        assert_eq!(
            rules.rules(),
            [Rule::In(vec!["draft".into(), "published".into(), "archived".into()])]
        );

        let rules = Rules::parse("not_in:admin,root").unwrap();
        assert_eq!(rules.rules(), [Rule::NotIn(vec!["admin".into(), "root".into()])]);
    }

    #[test]
    fn parses_a_two_number_parameter() {
        let rules = Rules::parse("between:1,10").unwrap();
        assert_eq!(rules.rules(), [Rule::Between(1.0, 10.0)]);
    }

    #[test]
    fn ignores_whitespace_and_empty_segments() {
        let rules = Rules::parse(" required | between:1, 10 || string ").unwrap();
        assert_eq!(rules.rules(), [Rule::Required, Rule::Between(1.0, 10.0), Rule::String]);
    }

    #[test]
    fn an_empty_spec_is_an_empty_rule_set() {
        assert!(Rules::parse("").unwrap().is_empty());
        assert_eq!(Rules::parse("").unwrap().len(), 0);
    }

    #[test]
    fn the_string_syntax_and_the_builder_agree() {
        assert_eq!(
            Rules::parse("required|email|max:255").unwrap(),
            Rule::required().email().max(255)
        );
        assert_eq!(
            Rules::parse("nullable|in:a,b").unwrap(),
            Rule::nullable().one_of(["a", "b"])
        );
        assert_eq!(
            Rules::parse("starts_with:https|ends_with:.com,.dev").unwrap(),
            Rule::starts_with(["https"]).ends_with([".com", ".dev"])
        );
        assert_eq!(Rules::parse("same:password").unwrap(), Rule::same("password"));
    }

    #[test]
    fn an_unknown_rule_is_reported_with_a_suggestion() {
        let error = Rules::parse("requried").unwrap_err().to_string();
        assert!(error.contains("unknown validation rule `requried`"), "{error}");
        assert!(error.contains("did you mean `required`"), "{error}");
    }

    #[test]
    fn a_rule_nothing_resembles_is_reported_without_a_guess() {
        let error = Rules::parse("teleport").unwrap_err().to_string();
        assert_eq!(error, "unknown validation rule `teleport`");
    }

    #[test]
    fn a_rule_missing_its_parameter_shows_an_example() {
        let error = Rules::parse("min").unwrap_err().to_string();
        assert_eq!(error, "validation rule `min` needs a parameter, like `min:3`");

        let error = Rules::parse("in:").unwrap_err().to_string();
        assert!(error.contains("in:draft,published"), "{error}");
    }

    #[test]
    fn a_non_numeric_bound_is_rejected() {
        let error = Rules::parse("max:many").unwrap_err().to_string();
        assert_eq!(error, "validation rule `max` expects a number, got `many`");
    }

    #[test]
    fn between_insists_on_exactly_two_bounds() {
        assert!(Rules::parse("between:1").unwrap_err().to_string().contains("exactly two"));
        assert!(Rules::parse("between:1,2,3").unwrap_err().to_string().contains("exactly two"));
    }

    #[test]
    fn rule_names_round_trip_through_has() {
        let rules = Rule::nullable().integer().min(18);
        assert!(rules.has("nullable"));
        assert!(rules.has("min"));
        assert!(!rules.has("required"));
    }

    #[test]
    fn a_spec_parses_through_the_from_str_and_into_rules_paths() {
        let parsed: Rules = "required|string".parse().unwrap();
        assert_eq!(parsed, "required|string".into_rules());
        assert_eq!(Rule::Required.into_rules(), Rule::required());
    }

    #[test]
    #[should_panic(expected = "invalid validation rules `nope`")]
    fn a_bad_spec_passed_as_a_string_panics_with_the_parser_message() {
        let _ = "nope".into_rules();
    }
}
