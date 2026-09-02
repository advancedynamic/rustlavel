//! What one user is allowed, and the rules that decide it.
//!
//! Nothing here touches a database. The store loads a [`Grants`] once, caches
//! it, and every subsequent authorization question is answered from these
//! functions — which is why they are worth reading carefully and testing
//! exhaustively. "Why can this user still do X?" is the only question an RBAC
//! system exists to answer, and it is answered here.

use std::collections::BTreeSet;

/// Does a *stored* permission name satisfy a check for `wanted`?
///
/// Wildcards live on the stored side only: a role is granted `users.*` and
/// that satisfies a check for `users.create`. Asking `can("users.*")` is not a
/// question about a wildcard, it is a question about a permission that happens
/// to be spelled with a star, and it is answered by exact match.
///
/// The matcher is deliberately not a regular expression, and not glob syntax.
/// It understands exactly two things:
///
/// * `*` on its own matches every permission.
/// * a trailing `.*` matches everything below that prefix, at any depth — so
///   `users.*` covers `users.create` and `users.avatar.delete`, but not the
///   bare `users`, which is a different permission.
///
/// A star anywhere else (`users.*.create`, `user*`) is treated as an ordinary
/// character, so it only ever matches itself. That is a limitation, not an
/// oversight: an authorization rule nobody can predict the meaning of by
/// reading it is worse than one that cannot express every case.
pub fn permission_matches(stored: &str, wanted: &str) -> bool {
    if stored == "*" || stored == wanted {
        return true;
    }

    let Some(prefix) = stored.strip_suffix(".*") else { return false };

    // The byte after the prefix has to be the separator, or `users.*` would
    // match `usersomething` through a plain `starts_with`.
    wanted.len() > prefix.len()
        && wanted.starts_with(prefix)
        && wanted.as_bytes()[prefix.len()] == b'.'
}

/// Everything the store knows about one user, resolved into a form that can
/// answer a check without another query.
///
/// `granted` is already the union of the permissions the user's roles carry and
/// the permissions granted to the user directly, because at check time those
/// two are indistinguishable: both mean "somebody said yes". `denied` is kept
/// apart, because a deny is not the absence of a grant — it outranks one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants {
    /// Role names, exactly as stored.
    pub roles: BTreeSet<String>,
    /// Permission names from roles and from direct grants, as stored — so a
    /// wildcard appears here as the wildcard, never expanded.
    pub granted: BTreeSet<String>,
    /// Permission names denied directly to this user.
    pub denied: BTreeSet<String>,
    /// Whether one of `roles` is a super role.
    ///
    /// Resolved when the grants are loaded rather than at check time, so a
    /// check never has to know how super roles are configured.
    pub is_super: bool,
}

impl Grants {
    /// Whether a direct deny covers `permission`.
    ///
    /// Denies match by the same wildcard rule as grants, so denying `users.*`
    /// shuts a user out of everything under `users.`, however they came by it.
    pub fn denies(&self, permission: &str) -> bool {
        self.denied.iter().any(|stored| permission_matches(stored, permission))
    }

    /// The whole precedence rule, in the order it is applied.
    ///
    /// 1. **An explicit direct deny beats everything** — including a super
    ///    role. A deny is the only entry an administrator writes in order to
    ///    say "no, not this one", and if a role could quietly overrule it there
    ///    would be no way to say that at all. It also means a super role is not
    ///    quite unstoppable, which is the honest trade: you get an auditable
    ///    way to fence off one action.
    /// 2. A super role passes. See [`crate::DEFAULT_SUPER_ROLE`] for why that
    ///    is dangerous.
    /// 3. A grant — from a role or made directly, the two rank equally — wins.
    ///    A direct grant therefore beats the *absence* of a role grant, which
    ///    is how one person gets one extra ability without inventing a role
    ///    for them.
    /// 4. Otherwise no. Silence is never permission.
    pub fn allows(&self, permission: &str) -> bool {
        if self.denies(permission) {
            return false;
        }
        if self.is_super {
            return true;
        }
        self.granted.iter().any(|stored| permission_matches(stored, permission))
    }

    /// Whether the user holds `role`, by exact name.
    ///
    /// No wildcards. Role names are a small, curated set that an administrator
    /// types in full; matching them loosely would only invent surprises.
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.contains(role)
    }

    /// The permissions that survive the denies, sorted.
    ///
    /// These are stored names, so a wildcard is listed as the wildcard. This is
    /// not the set of every action the user can perform — with a wildcard, or
    /// with a super role, that set is not enumerable at all — it is the set of
    /// rules that apply to them, which is what an admin screen should show.
    pub fn effective(&self) -> Vec<String> {
        self.granted
            .iter()
            .filter(|stored| !self.denies(stored))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set<const N: usize>(names: [&str; N]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn an_exact_name_matches_itself_and_nothing_near_it() {
        assert!(permission_matches("users.create", "users.create"));
        assert!(!permission_matches("users.create", "users.createx"));
        assert!(!permission_matches("users.create", "users.delete"));
        assert!(!permission_matches("users.create", "users"));
        assert!(!permission_matches("users.create", ""));
    }

    #[test]
    fn a_bare_star_matches_anything() {
        for wanted in ["users.create", "a", "", "*", "a.b.c.d"] {
            assert!(permission_matches("*", wanted), "`*` should cover {wanted:?}");
        }
    }

    #[test]
    fn a_trailing_star_matches_every_segment_below_it() {
        assert!(permission_matches("users.*", "users.create"));
        assert!(permission_matches("users.*", "users.avatar.delete"));

        // The prefix itself is a different permission, and the star does not
        // reach sideways into a name that merely starts with the same letters.
        assert!(!permission_matches("users.*", "users"));
        assert!(!permission_matches("users.*", "usersomething"));
        assert!(!permission_matches("users.*", "user.create"));
        assert!(!permission_matches("users.*", "posts.create"));
    }

    #[test]
    fn a_star_anywhere_else_is_just_a_character() {
        assert!(!permission_matches("users.*.create", "users.avatar.create"));
        assert!(permission_matches("users.*.create", "users.*.create"));
        assert!(!permission_matches("user*", "users"));
    }

    #[test]
    fn the_wildcard_only_works_on_the_stored_side() {
        // Storing `users.create` does not answer a question about `users.*`:
        // the check is not a pattern, it is the name of one thing.
        assert!(!permission_matches("users.create", "users.*"));
        assert!(!permission_matches("users.create", "*"));
    }

    #[test]
    fn a_grant_allows_and_silence_refuses() {
        let grants = Grants { granted: set(["users.create"]), ..Grants::default() };

        assert!(grants.allows("users.create"));
        assert!(!grants.allows("users.delete"));
        assert!(!Grants::default().allows("users.create"));
    }

    #[test]
    fn a_direct_deny_beats_a_grant() {
        let grants = Grants {
            granted: set(["users.create", "users.delete"]),
            denied: set(["users.delete"]),
            ..Grants::default()
        };

        assert!(grants.allows("users.create"));
        assert!(!grants.allows("users.delete"), "the deny has to win");
    }

    #[test]
    fn a_direct_deny_beats_a_wildcard_grant() {
        let grants = Grants {
            granted: set(["users.*"]),
            denied: set(["users.delete"]),
            ..Grants::default()
        };

        assert!(grants.allows("users.create"));
        assert!(!grants.allows("users.delete"));
    }

    #[test]
    fn a_wildcard_deny_shuts_a_whole_prefix() {
        let grants =
            Grants { granted: set(["*"]), denied: set(["billing.*"]), ..Grants::default() };

        assert!(grants.allows("users.create"));
        assert!(!grants.allows("billing.refund"));
        // The bare prefix is outside the wildcard, as everywhere else.
        assert!(grants.allows("billing"));
    }

    #[test]
    fn a_deny_beats_even_a_super_role() {
        let grants =
            Grants { is_super: true, denied: set(["billing.refund"]), ..Grants::default() };

        assert!(grants.allows("anything.at.all"));
        assert!(
            !grants.allows("billing.refund"),
            "an explicit deny is the one thing a super role does not overrule"
        );
    }

    #[test]
    fn a_super_role_needs_no_permissions_attached() {
        let grants = Grants { is_super: true, ..Grants::default() };

        assert!(grants.allows("users.create"));
        assert!(grants.allows("something.nobody.has.defined.yet"));
        assert!(grants.granted.is_empty(), "which is exactly why it cannot be audited");
    }

    #[test]
    fn roles_are_matched_by_exact_name() {
        let grants = Grants { roles: set(["admin", "billing.manager"]), ..Grants::default() };

        assert!(grants.has_role("admin"));
        assert!(grants.has_role("billing.manager"));
        assert!(!grants.has_role("Admin"));
        assert!(!grants.has_role("billing.*"), "role names are not patterns");
        assert!(!grants.has_role("editor"));
    }

    #[test]
    fn effective_lists_stored_names_with_the_denied_ones_removed() {
        let grants = Grants {
            granted: set(["users.create", "users.delete", "posts.*"]),
            denied: set(["users.delete", "posts.publish"]),
            ..Grants::default()
        };

        // `posts.*` stays: it is not itself denied, even though one permission
        // underneath it is. A list of rules, not an expansion of them.
        assert_eq!(grants.effective(), vec!["posts.*", "users.create"]);
    }

    #[test]
    fn precedence_holds_in_every_combination() {
        // Every combination of (deny, super, grant) against one permission, so
        // the table above is not merely described but checked.
        for &deny in &[false, true] {
            for &is_super in &[false, true] {
                for &grant in &[false, true] {
                    let grants = Grants {
                        granted: if grant { set(["users.create"]) } else { BTreeSet::new() },
                        denied: if deny { set(["users.create"]) } else { BTreeSet::new() },
                        is_super,
                        ..Grants::default()
                    };

                    let expected = !deny && (is_super || grant);
                    assert_eq!(
                        grants.allows("users.create"),
                        expected,
                        "deny={deny} super={is_super} grant={grant}"
                    );
                }
            }
        }
    }
}
