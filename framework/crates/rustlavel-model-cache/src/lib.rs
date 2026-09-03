//! A second-level cache for models — Hibernate's, in Rustlavel's vocabulary.
//!
//! The first level is the query you already ran. This is the one underneath:
//! entities kept by primary key and query results kept by the SQL that
//! produced them, shared by every request in the process and, with Redis
//! behind it, by every process.
//!
//! ```ignore
//! let cache = ModelCache::new(store)
//!     .region::<User>(Region::new().ttl(Duration::from_secs(300)))
//!     .region::<Country>(Region::read_only());
//!
//! let user = cache.find::<User>(&db, 7).await?;              // by key
//! let admins = cache.get::<User>(&db, User::query()          // by query
//!     .filter("role", "admin")).await?;
//!
//! cache.update(&db, &user).await?;   // writes, and invalidates what it must
//! ```
//!
//! ## What is hard about this, and how it is handled
//!
//! Caching an entity is easy: one key, and the write that changes it knows
//! which key to drop. **Caching a query result is the hard half**, because a
//! write to one row invalidates an unknown number of cached result sets and
//! there is no way to enumerate them — a cached `WHERE role = 'admin'` is
//! invalidated by an insert that this cache never sees the shape of.
//!
//! Hibernate's answer, and the one here: a **generation counter per table**.
//! Every cached query records the generation it was computed under; every
//! write bumps it. A read whose recorded generation is not the current one is
//! a miss, whatever its key. One counter invalidates every stale result set
//! for that table at once, without knowing anything about them.
//!
//! That is deliberately blunt. A write to any row of `users` costs you every
//! cached `users` query, which is the correct trade: the alternative is
//! serving a list that is missing a row somebody just added, and a cache that
//! is occasionally wrong is worse than no cache at all.
//!
//! ## What this does not do
//!
//! There is **no transactional strategy**. Hibernate's `TRANSACTIONAL` mode
//! keeps the cache consistent with an in-flight transaction that may still
//! roll back; this one is updated when the write returns, so a rollback after
//! a cached write leaves the cache holding a value the database does not have.
//! Invalidating rather than writing through — which is what [`ModelCache`]
//! does — makes that window a stale read rather than a wrong one, and it
//! closes on the next generation bump. If you need better than that, do not
//! cache that table.
//!
//! It also caches **rows, not object graphs**. There is no relation cache and
//! no lazy-loading proxy, because this ORM has neither.

pub mod cache;
pub mod keys;
pub mod region;
pub mod stats;

pub use cache::ModelCache;
pub use region::{Region, Strategy};
pub use stats::{Counts, Stats};

/// What an application file usually needs.
pub mod prelude {
    pub use crate::cache::ModelCache;
    pub use crate::region::{Region, Strategy};
}
