//! What a cached thing is stored under.
//!
//! Three shapes, all prefixed so a `flush` of the application's own cache does
//! not have to know about this one:
//!
//! - `rl:mc:e:{table}:{key}` — one entity.
//! - `rl:mc:q:{table}:{fingerprint}` — one query's result set.
//! - `rl:mc:g:{table}` — the table's generation counter.

use rustlavel_core::Json;
use rustlavel_db::Value;
use std::hash::{Hash, Hasher};

pub const PREFIX: &str = "rl:mc";

pub fn entity(table: &str, key: &str) -> String {
    format!("{PREFIX}:e:{table}:{key}")
}

pub fn query(table: &str, fingerprint: u64) -> String {
    format!("{PREFIX}:q:{table}:{fingerprint:016x}")
}

pub fn generation(table: &str) -> String {
    format!("{PREFIX}:g:{table}")
}

/// A 64-bit fingerprint of a statement and its bindings.
///
/// **A collision here would serve one query's rows as another's**, which is
/// the worst thing a cache can do, so the fingerprint is not trusted on its
/// own: the statement it was taken from is stored alongside the rows and
/// compared on the way out — see [`crate::cache::Entry`]. A collision is then
/// a miss rather than a wrong answer, which turns a correctness problem into a
/// performance one and costs a string comparison per hit.
///
/// That is also why this is not a cryptographic hash. There is nothing to
/// forge — the input is SQL this process generated — and the verification
/// above makes the collision rate a matter of cost rather than of safety.
pub fn fingerprint(sql: &str, bindings: &[Value]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    sql.hash(&mut hasher);
    for value in bindings {
        // Through `Json` because `Value` is not `Hash` — a float is in there —
        // and its text form is exactly what distinguishes two bindings.
        Json::from(value.clone()).to_string().hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_shapes_do_not_collide_with_each_other() {
        assert_eq!(entity("users", "7"), "rl:mc:e:users:7");
        assert_eq!(generation("users"), "rl:mc:g:users");
        assert!(query("users", 1).starts_with("rl:mc:q:users:"));

        // A table called `e:users` must not be able to spell another table's
        // entity key. The prefixes are distinct segments, so it cannot.
        assert_ne!(entity("users", "7"), query("users", 7));
    }

    #[test]
    fn the_bindings_are_part_of_the_fingerprint() {
        let sql = "select * from users where role = ?";

        let admin = fingerprint(sql, &[Value::from("admin")]);
        let support = fingerprint(sql, &[Value::from("support")]);

        assert_ne!(admin, support, "two different filters shared a cache key");
        assert_eq!(admin, fingerprint(sql, &[Value::from("admin")]), "not stable");

        // And so is the statement, for the same bindings.
        assert_ne!(admin, fingerprint("select 1", &[Value::from("admin")]));
    }

    /// `7` the number and `"7"` the string are different bindings, and a
    /// fingerprint that flattened both to `7` would serve one for the other.
    #[test]
    fn a_number_and_its_text_are_different_bindings() {
        let sql = "select * from users where id = ?";

        assert_ne!(
            fingerprint(sql, &[Value::from(7i64)]),
            fingerprint(sql, &[Value::from("7")]),
        );
    }

    #[test]
    fn the_order_of_the_bindings_matters() {
        let sql = "select * from users where a = ? and b = ?";

        assert_ne!(
            fingerprint(sql, &[Value::from("x"), Value::from("y")]),
            fingerprint(sql, &[Value::from("y"), Value::from("x")]),
        );
    }
}
