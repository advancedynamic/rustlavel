//! Who a flag is being checked *for*.
//!
//! A flag on its own is not a question anybody can answer. "Is the new checkout
//! on?" only has a meaning once you say for whom — this customer, that tenant,
//! or everybody at once — and the scope is that second half of the question.

use std::str::FromStr;

/// The subject of a flag check.
///
/// Three kinds, because three are what applications actually roll out to:
///
/// * a **user**, by whatever identifier they log in with;
/// * a **tenant**, when a feature is bought or enabled per customer rather than
///   per person;
/// * **nobody in particular** ([`Scope::none`]), for a switch that is on or off
///   for the whole installation.
///
/// The identifier is a `String` and not a generic parameter on purpose. A flag
/// store has to write it down, an operator has to type it into a console, and a
/// percentage rollout has to hash it — all three want text, and a type
/// parameter here would spread through every signature in the crate to buy
/// nothing.
///
/// ```
/// use rustlavel_flags::Scope;
///
/// assert_eq!(Scope::user("ada").id(), "ada");
/// assert_eq!(Scope::user_id(41).id(), "41");
/// assert_eq!(Scope::tenant("acme").key(), "tenant:acme");
/// assert_eq!(Scope::none().id(), "");
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Everybody. Built with [`Scope::none`].
    #[default]
    None,
    /// One logged-in user, by their authentication identifier.
    User(String),
    /// One tenant, team, organisation — whatever the application calls it.
    Tenant(String),
}

impl Scope {
    /// The global scope: a flag that is simply on or off, for everyone.
    ///
    /// Named `none` rather than `global` because of how it reads at the call
    /// site — `flags.active("maintenance", &Scope::none())` says "there is
    /// nobody in particular here", which is the fact the caller has.
    pub fn none() -> Self {
        Scope::None
    }

    /// One user, by the identifier they log in with.
    pub fn user(id: impl Into<String>) -> Self {
        Scope::User(id.into())
    }

    /// One user, by a numeric primary key — the common case in an application
    /// whose `users` table is keyed by `bigint`.
    pub fn user_id(id: i64) -> Self {
        Scope::User(id.to_string())
    }

    /// One tenant, by name or by id.
    pub fn tenant(name: impl Into<String>) -> Self {
        Scope::Tenant(name.into())
    }

    /// The identifier, or `""` for [`Scope::none`].
    ///
    /// The empty string is deliberate: a resolver that does not care which kind
    /// of scope it was given can read `scope.id()` unconditionally, and a
    /// global check falls through whatever test it applies rather than
    /// panicking or matching some sentinel value.
    pub fn id(&self) -> &str {
        match self {
            Scope::None => "",
            Scope::User(id) | Scope::Tenant(id) => id,
        }
    }

    /// The identifier parsed into a type: `scope.id_as::<i64>()`.
    ///
    /// `None` when the scope is global, or when the identifier is not that
    /// type — a resolver keyed on a numeric user id gets one answer for both,
    /// which is right, because it cannot serve either.
    pub fn id_as<T: FromStr>(&self) -> Option<T> {
        self.id().parse().ok()
    }

    /// `"global"`, `"user"` or `"tenant"` — for logs and for the store key.
    pub fn kind(&self) -> &'static str {
        match self {
            Scope::None => "global",
            Scope::User(_) => "user",
            Scope::Tenant(_) => "tenant",
        }
    }

    /// Whether this is the global scope.
    pub fn is_none(&self) -> bool {
        matches!(self, Scope::None)
    }

    /// The string a [`FlagStore`](crate::FlagStore) writes an override under.
    ///
    /// The kind is part of the key, so a user called `acme` and a tenant called
    /// `acme` are two different subjects and cannot inherit each other's
    /// overrides. An identifier containing a colon is left alone rather than
    /// escaped — the prefix already separates the namespaces, and the only way
    /// two keys can still collide is if one subject's identifier is literally
    /// another kind's prefix plus another subject's identifier, which is a
    /// collision between two identifiers the application itself issued.
    pub fn key(&self) -> String {
        match self {
            Scope::None => "global".to_string(),
            Scope::User(id) => format!("user:{id}"),
            Scope::Tenant(id) => format!("tenant:{id}"),
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_carries_its_identifier_and_its_kind() {
        assert_eq!(Scope::user("ada").kind(), "user");
        assert_eq!(Scope::user("ada").id(), "ada");
        assert_eq!(Scope::tenant("acme").kind(), "tenant");
        assert_eq!(Scope::none().kind(), "global");
        assert!(Scope::none().is_none());
        assert!(!Scope::user("ada").is_none());
    }

    #[test]
    fn a_numeric_identifier_survives_the_round_trip() {
        assert_eq!(Scope::user_id(41).id(), "41");
        assert_eq!(Scope::user_id(41).id_as::<i64>(), Some(41));
        assert_eq!(Scope::user("ada").id_as::<i64>(), None);
        assert_eq!(Scope::none().id_as::<i64>(), None);
    }

    #[test]
    fn the_kind_is_part_of_the_key_so_two_namespaces_cannot_collide() {
        // The bug this prevents: a tenant called `acme` inheriting the
        // overrides an operator set for a *user* called `acme`.
        assert_ne!(Scope::user("acme").key(), Scope::tenant("acme").key());
        assert_eq!(Scope::user("acme").key(), "user:acme");
        assert_eq!(Scope::none().key(), "global");
        assert_eq!(Scope::none().to_string(), "global");
    }

    #[test]
    fn the_default_scope_is_the_global_one() {
        assert_eq!(Scope::default(), Scope::none());
    }
}
