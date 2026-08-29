//! Scopes: what a token is allowed to do.

use std::collections::BTreeSet;

/// A set of scopes, kept sorted so one set always renders identically.
///
/// OAuth writes scopes as a space-delimited string, which is easy to get wrong
/// by hand — an empty scope, a double space, or a duplicate all produce a
/// string that looks fine and compares unequal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scopes {
    values: BTreeSet<String>,
}

impl Scopes {
    pub fn new() -> Self {
        Scopes::default()
    }

    /// Parse the space-delimited form OAuth sends on the wire.
    pub fn parse(raw: &str) -> Scopes {
        Scopes {
            values: raw
                .split_whitespace()
                .filter(|scope| !scope.is_empty())
                .map(str::to_string)
                .collect(),
        }
    }

    pub fn of<I, S>(scopes: I) -> Scopes
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Scopes { values: scopes.into_iter().map(Into::into).collect() }
    }

    pub fn add(&mut self, scope: impl Into<String>) {
        self.values.insert(scope.into());
    }

    pub fn with(mut self, scope: impl Into<String>) -> Self {
        self.add(scope);
        self
    }

    pub fn contains(&self, scope: &str) -> bool {
        self.values.contains(scope)
    }

    /// Whether this set covers everything `required` asks for.
    ///
    /// The question an authorisation check actually asks: not "are these equal"
    /// but "is what I was granted enough".
    pub fn covers(&self, required: &Scopes) -> bool {
        required.values.is_subset(&self.values)
    }

    /// The scopes in this set that are also in `allowed`.
    ///
    /// A client asking for more than it is registered for gets the intersection
    /// rather than a refusal, which is what most providers do — but the caller
    /// decides, because silently granting less than was asked for is worth
    /// knowing about.
    pub fn intersect(&self, allowed: &Scopes) -> Scopes {
        Scopes { values: self.values.intersection(&allowed.values).cloned().collect() }
    }

    /// Anything in this set that `allowed` does not contain.
    pub fn beyond(&self, allowed: &Scopes) -> Scopes {
        Scopes { values: self.values.difference(&allowed.values).cloned().collect() }
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.values.iter().map(String::as_str)
    }
}

impl std::fmt::Display for Scopes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let joined: Vec<&str> = self.values.iter().map(String::as_str).collect();
        f.write_str(&joined.join(" "))
    }
}

impl From<&str> for Scopes {
    fn from(raw: &str) -> Scopes {
        Scopes::parse(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_wire_form_forgivingly() {
        let scopes = Scopes::parse("  read   write  read ");

        assert_eq!(scopes.len(), 2, "duplicates and blanks are dropped");
        assert!(scopes.contains("read"));
        assert!(scopes.contains("write"));
    }

    #[test]
    fn renders_in_a_stable_order() {
        // Two sets built differently must produce the same string, or a
        // signature over them would differ for no reason.
        assert_eq!(Scopes::of(["write", "read"]).to_string(), "read write");
        assert_eq!(Scopes::parse("write read").to_string(), "read write");
    }

    #[test]
    fn an_empty_set_is_an_empty_string() {
        assert_eq!(Scopes::new().to_string(), "");
        assert!(Scopes::parse("   ").is_empty());
    }

    #[test]
    fn coverage_asks_whether_what_was_granted_is_enough() {
        let granted = Scopes::of(["read", "write", "delete"]);

        assert!(granted.covers(&Scopes::of(["read"])));
        assert!(granted.covers(&Scopes::of(["read", "write"])));
        assert!(granted.covers(&Scopes::new()), "asking for nothing is always covered");
        assert!(!granted.covers(&Scopes::of(["admin"])));
    }

    #[test]
    fn a_request_can_be_narrowed_to_what_a_client_may_have() {
        let asked = Scopes::of(["read", "write", "admin"]);
        let allowed = Scopes::of(["read", "write"]);

        assert_eq!(asked.intersect(&allowed), Scopes::of(["read", "write"]));
        assert_eq!(asked.beyond(&allowed), Scopes::of(["admin"]));
    }

    #[test]
    fn round_trips_through_the_wire_form() {
        let original = Scopes::of(["openid", "profile", "email"]);
        assert_eq!(Scopes::parse(&original.to_string()), original);
    }
}
