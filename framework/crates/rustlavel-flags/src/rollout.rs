//! Percentage rollouts, and the one property that makes them usable.
//!
//! A rollout puts every scope in a bucket from 0 to 99 and turns the flag on
//! for the buckets below the percentage. The bucket comes from hashing the flag
//! name together with the scope — **never from a random number**.
//!
//! That is the whole design, and it is worth being blunt about why. A rollout
//! that rolls a die on each check gives the same user a different answer on
//! every request: the new checkout on the product page, the old one when they
//! press "buy", the new one again on the confirmation. Half a page from each
//! version is not "10% exposure", it is a broken site for everybody, and it is
//! strictly worse than shipping the feature to all users at once. Hashing
//! instead means a scope's bucket is fixed for as long as its identifier and
//! the flag's name are — across requests, across processes, across a deploy,
//! across a restart with an empty cache.
//!
//! Two consequences fall out of hashing the *flag name* as well as the scope:
//!
//! * two flags at 10% do not roll out to the same 10% of users, so a user is
//!   not permanently the guinea pig for everything;
//! * **raising a percentage only ever adds users.** Everyone in the first 10%
//!   is still in the first 25%, because their bucket did not move. Lowering it
//!   only ever removes users, and puts back exactly the ones it took.
//!
//! ```
//! use rustlavel_flags::{Scope, rollout};
//!
//! let scope = Scope::user_id(41);
//! // Whatever bucket 41 is in, it is in the same one next time.
//! assert_eq!(rollout::bucket("new-checkout", &scope), rollout::bucket("new-checkout", &scope));
//! // 0% is nobody and 100% is everybody, with no special-casing needed.
//! assert!(!rollout::in_rollout("new-checkout", &scope, 0));
//! assert!(rollout::in_rollout("new-checkout", &scope, 100));
//! ```
//!
//! # What this is not
//!
//! It is not a secret. The hash is FNV-1a with a mixing step, it is written
//! below in twenty lines, and anybody who can guess a user id can work out
//! which bucket that user is in. That is fine for a rollout and useless as a
//! defence: if a feature must not be *reachable* by someone outside the
//! rollout, the check that keeps them out belongs in authorization, not here.

use crate::scope::Scope;

/// How many buckets a rollout is divided into. One per percentage point.
const BUCKETS: u64 = 100;

/// The bucket a scope falls into for one flag: `0..100`.
///
/// Stable for a given `(flag, scope)` pair forever — that is the point. Change
/// the flag's name or the scope's identifier and the bucket changes, which is
/// why renaming a flag mid-rollout reshuffles everybody and should be treated
/// as starting the rollout again.
pub fn bucket(flag: &str, scope: &Scope) -> u8 {
    (mix(hash(flag, scope)) % BUCKETS) as u8
}

/// Whether a scope is inside a `percent` rollout of one flag.
///
/// `0` is nobody and anything at or above `100` is everybody. Boundaries are
/// worth stating exactly: a scope is in when its bucket is *below* the
/// percentage, so 1% is bucket 0 alone and 100% is buckets 0 through 99.
pub fn in_rollout(flag: &str, scope: &Scope, percent: u8) -> bool {
    (bucket(flag, scope) as u64) < (percent as u64).min(BUCKETS)
}

/// FNV-1a over the flag name, a separator, and the scope's key.
///
/// The separator matters: without it the flag `new` with scope `user:1` and the
/// flag `newuser` with scope `:1` would hash the same bytes, and two unrelated
/// rollouts would share a bucket assignment. A byte that cannot appear in
/// either half keeps them apart.
fn hash(flag: &str, scope: &Scope) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET;
    let mut eat = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(PRIME);
        }
    };

    eat(flag.as_bytes());
    eat(&[0x1f]);
    eat(scope.key().as_bytes());

    hash
}

/// Avalanche the hash before taking it modulo 100.
///
/// FNV-1a's low bits are badly mixed — the lowest bit of the digest is just the
/// XOR of the lowest bits of the input, because multiplying by an odd prime
/// cannot change it. Taking `% 100` reads exactly those bits, so without this
/// step consecutive numeric user ids land in a visible pattern rather than
/// spread out, and a rollout aimed at 10% could hit a great deal more or less.
/// This is the finaliser from SplitMix64, which exists to do precisely this.
fn mix(mut hash: u64) -> u64 {
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^= hash >> 31;
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_always_lands_in_the_same_bucket() {
        for id in 0..500 {
            let scope = Scope::user_id(id);
            assert_eq!(bucket("new-checkout", &scope), bucket("new-checkout", &scope));
        }
    }

    #[test]
    fn two_flags_do_not_roll_out_to_the_same_people() {
        // If the flag name were not hashed in, these two would be identical and
        // the same unlucky 10% would test every feature the team ever ships.
        let differ = (0..200)
            .map(Scope::user_id)
            .filter(|scope| bucket("alpha", scope) != bucket("beta", scope))
            .count();

        assert!(differ > 180, "only {differ} of 200 scopes differed between two flags");
    }

    #[test]
    fn the_buckets_are_spread_evenly_over_ten_thousand_scopes() {
        let mut counts = [0usize; 100];
        for id in 0..10_000 {
            counts[bucket("new-checkout", &Scope::user_id(id)) as usize] += 1;
        }

        // 100 buckets over 10,000 scopes is 100 each on average. A fair hash
        // will not be exact; one with the low-bit problem the mixing step
        // exists to fix produces empty buckets and buckets of 200.
        let low = *counts.iter().min().expect("a hundred buckets");
        let high = *counts.iter().max().expect("a hundred buckets");
        assert!(low >= 60, "the emptiest bucket held {low} of an expected 100");
        assert!(high <= 150, "the fullest bucket held {high} of an expected 100");
    }

    #[test]
    fn a_percentage_is_roughly_the_proportion_it_says() {
        let inside =
            (0..10_000).filter(|id| in_rollout("new-checkout", &Scope::user_id(*id), 25)).count();

        assert!((2_300..=2_700).contains(&inside), "25% of 10,000 scopes came out as {inside}");
    }

    #[test]
    fn raising_the_percentage_only_ever_adds_people() {
        // The property that makes a rollout safe to widen: nobody who had the
        // feature at 10% loses it at 25%, so no user watches it disappear.
        for id in 0..2_000 {
            let scope = Scope::user_id(id);
            if in_rollout("new-checkout", &scope, 10) {
                assert!(in_rollout("new-checkout", &scope, 25), "user {id} lost the feature");
            }
        }
    }

    #[test]
    fn zero_is_nobody_and_a_hundred_is_everybody() {
        for id in 0..1_000 {
            let scope = Scope::user_id(id);
            assert!(!in_rollout("new-checkout", &scope, 0));
            assert!(in_rollout("new-checkout", &scope, 100));
            // Above 100 is still everybody rather than nobody, which is what a
            // wrapping or truncating implementation would give.
            assert!(in_rollout("new-checkout", &scope, 255));
        }
    }

    #[test]
    fn one_percent_is_the_first_bucket_alone() {
        for id in 0..1_000 {
            let scope = Scope::user_id(id);
            assert_eq!(in_rollout("new-checkout", &scope, 1), bucket("new-checkout", &scope) == 0);
        }
    }

    #[test]
    fn the_separator_keeps_a_flag_name_from_running_into_a_scope() {
        // `new` + `user:1` and `newuser` + `:1` are the same bytes end to end.
        // With the separator they are not the same hash.
        assert_ne!(hash("new", &Scope::user("1")), hash("newuser", &Scope::tenant("1")));
    }

    #[test]
    fn a_tenant_and_a_user_with_one_name_are_rolled_out_separately() {
        // Not a coincidence to be relied on for a single name — two different
        // keys can share a bucket — so this asks it of a hundred names at once,
        // where agreement everywhere would mean the kind never reached the hash.
        let differ = (0..100)
            .map(|n| format!("acme-{n}"))
            .filter(|name| {
                bucket("new-checkout", &Scope::user(name))
                    != bucket("new-checkout", &Scope::tenant(name))
            })
            .count();

        assert!(differ > 80, "only {differ} of 100 names differed by scope kind");
    }
}
