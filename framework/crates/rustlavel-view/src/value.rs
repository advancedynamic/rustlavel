//! The view's rules for reading a `Json` value: truth, order, and text.
//!
//! There is deliberately no second value type here. Templates render whatever
//! the rest of the framework already speaks — `rustlavel_core::Json` — so a
//! query result or an API payload can go straight into a view without a
//! conversion layer in between. What lives in this module is only the handful
//! of decisions a template engine has to make on top of that model.

use rustlavel_core::Json;
use std::cmp::Ordering;

/// Whether a value counts as true for `@if` and `&&`/`||`.
///
/// The rules follow PHP closely enough that a Laravel developer's instinct is
/// right: nothing, zero, empty text, and an empty collection are all falsy.
/// That is what makes `@if(items)` read naturally instead of forcing
/// `@if(items.length > 0)`.
pub fn truthy(value: &Json) -> bool {
    match value {
        Json::Null => false,
        Json::Bool(flag) => *flag,
        // NaN is falsy for the same reason it is not equal to itself: no
        // comparison involving it should quietly succeed.
        Json::Number(number) => *number != 0.0 && !number.is_nan(),
        Json::String(text) => !text.is_empty(),
        Json::Array(items) => !items.is_empty(),
        Json::Object(map) => !map.is_empty(),
    }
}

/// Ordering for `<`, `<=`, `>` and `>=`.
///
/// Only numbers and strings are ordered, and only against their own kind:
/// comparing a number to a string is a mistake in the view, and answering
/// `false` to both `a < b` and `a > b` is a far better clue than inventing a
/// coercion rule the author has to memorise.
pub fn compare(left: &Json, right: &Json) -> Option<Ordering> {
    match (left, right) {
        (Json::Number(a), Json::Number(b)) => a.partial_cmp(b),
        (Json::String(a), Json::String(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// Render a value as the text `{{ }}` would emit, before escaping.
///
/// `null` becomes the empty string. That is the single most important line in
/// this file: a missing key must leave a hole in the page, never take the page
/// down.
pub fn to_text(value: &Json) -> String {
    match value {
        Json::Null => String::new(),
        Json::Bool(flag) => flag.to_string(),
        Json::String(text) => text.clone(),
        // Ids are the most common numeric output, so integral values render
        // without the `.0` that `f64` would otherwise insist on.
        Json::Number(number) if number.fract() == 0.0 && number.abs() < 1e15 => {
            (*number as i64).to_string()
        }
        Json::Number(number) => number.to_string(),
        // Arrays and objects have no obvious text form; compact JSON is at
        // least honest about what the author asked for.
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emptiness_of_every_kind_is_falsy() {
        assert!(!truthy(&Json::Null));
        assert!(!truthy(&Json::Bool(false)));
        assert!(!truthy(&Json::Number(0.0)));
        assert!(!truthy(&Json::from("")));
        assert!(!truthy(&Json::Array(Vec::new())));
        assert!(!truthy(&Json::object(Vec::<(String, Json)>::new())));

        assert!(truthy(&Json::Bool(true)));
        assert!(truthy(&Json::Number(-1.0)));
        assert!(truthy(&Json::from("0")));
        assert!(truthy(&Json::from(vec![1])));
    }

    #[test]
    fn only_matching_kinds_are_ordered() {
        assert_eq!(compare(&Json::from(1), &Json::from(2)), Some(Ordering::Less));
        assert_eq!(compare(&Json::from("a"), &Json::from("b")), Some(Ordering::Less));
        assert_eq!(compare(&Json::from(1), &Json::from("1")), None);
    }

    #[test]
    fn null_renders_as_nothing_and_integers_keep_their_shape() {
        assert_eq!(to_text(&Json::Null), "");
        assert_eq!(to_text(&Json::from(42)), "42");
        assert_eq!(to_text(&Json::from(1.5)), "1.5");
        assert_eq!(to_text(&Json::Bool(true)), "true");
        assert_eq!(to_text(&Json::from(vec![1, 2])), "[1,2]");
    }
}
