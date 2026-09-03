//! What the cache has been doing.
//!
//! Kept because the first question anybody asks of a cache is "is it working",
//! and the honest answer needs a hit rate rather than an impression. Counted
//! per model, since one table doing badly is invisible in a total.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// One region's tally.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub entity_hits: u64,
    pub entity_misses: u64,
    pub query_hits: u64,
    pub query_misses: u64,
    /// Result sets not stored because they were over `max_rows`.
    pub too_large: u64,
    /// Result sets not stored because a column cannot round-trip through JSON
    /// — a binary column. Worth watching: a table that never caches is a
    /// region that is doing nothing.
    pub unsupported: u64,
    /// Generation bumps: how often a write threw this table's queries away.
    pub invalidations: u64,
}

impl Counts {
    /// Hits as a fraction of lookups, entities and queries together.
    ///
    /// `None` when nothing has been looked up: a cache nobody has asked
    /// anything of has no hit rate, and reporting 0% would read as a problem.
    pub fn hit_rate(&self) -> Option<f64> {
        let hits = self.entity_hits + self.query_hits;
        let total = hits + self.entity_misses + self.query_misses;
        (total > 0).then(|| hits as f64 / total as f64)
    }
}

#[derive(Default)]
struct Tally {
    entity_hits: AtomicU64,
    entity_misses: AtomicU64,
    query_hits: AtomicU64,
    query_misses: AtomicU64,
    too_large: AtomicU64,
    unsupported: AtomicU64,
    invalidations: AtomicU64,
}

impl Tally {
    fn read(&self) -> Counts {
        Counts {
            entity_hits: self.entity_hits.load(Ordering::Relaxed),
            entity_misses: self.entity_misses.load(Ordering::Relaxed),
            query_hits: self.query_hits.load(Ordering::Relaxed),
            query_misses: self.query_misses.load(Ordering::Relaxed),
            too_large: self.too_large.load(Ordering::Relaxed),
            unsupported: self.unsupported.load(Ordering::Relaxed),
            invalidations: self.invalidations.load(Ordering::Relaxed),
        }
    }
}

/// The counters, by model name.
#[derive(Default)]
pub struct Stats {
    tables: Mutex<BTreeMap<&'static str, Tally>>,
}

/// Which counter a call touched.
#[derive(Debug, Clone, Copy)]
pub enum Event {
    EntityHit,
    EntityMiss,
    QueryHit,
    QueryMiss,
    TooLarge,
    Unsupported,
    Invalidated,
}

impl Stats {
    pub fn record(&self, table: &'static str, event: Event) {
        // `Relaxed` throughout: these are counters nobody makes a decision on,
        // and ordering them against each other would cost more than the
        // numbers are worth.
        let mut tables = self.tables.lock().unwrap_or_else(|e| e.into_inner());
        let tally = tables.entry(table).or_default();
        let counter = match event {
            Event::EntityHit => &tally.entity_hits,
            Event::EntityMiss => &tally.entity_misses,
            Event::QueryHit => &tally.query_hits,
            Event::QueryMiss => &tally.query_misses,
            Event::TooLarge => &tally.too_large,
            Event::Unsupported => &tally.unsupported,
            Event::Invalidated => &tally.invalidations,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Every region's tally, by model name.
    pub fn all(&self) -> BTreeMap<&'static str, Counts> {
        let tables = self.tables.lock().unwrap_or_else(|e| e.into_inner());
        tables.iter().map(|(name, tally)| (*name, tally.read())).collect()
    }

    pub fn for_table(&self, table: &str) -> Counts {
        let tables = self.tables.lock().unwrap_or_else(|e| e.into_inner());
        tables.get(table).map(Tally::read).unwrap_or_default()
    }

    /// The totals across every region.
    pub fn total(&self) -> Counts {
        self.all().values().fold(Counts::default(), |mut sum, counts| {
            sum.entity_hits += counts.entity_hits;
            sum.entity_misses += counts.entity_misses;
            sum.query_hits += counts.query_hits;
            sum.query_misses += counts.query_misses;
            sum.too_large += counts.too_large;
            sum.unsupported += counts.unsupported;
            sum.invalidations += counts.invalidations;
            sum
        })
    }

    pub fn reset(&self) {
        self.tables.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

impl std::fmt::Debug for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stats").field("tables", &self.all()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cache_nobody_has_asked_anything_of_has_no_hit_rate() {
        assert_eq!(Counts::default().hit_rate(), None);

        let counts = Counts { entity_hits: 3, entity_misses: 1, ..Default::default() };
        assert_eq!(counts.hit_rate(), Some(0.75));
    }

    #[test]
    fn counters_are_kept_per_table_and_summed_on_request() {
        let stats = Stats::default();
        stats.record("users", Event::EntityHit);
        stats.record("users", Event::EntityHit);
        stats.record("users", Event::QueryMiss);
        stats.record("posts", Event::EntityMiss);

        assert_eq!(stats.for_table("users").entity_hits, 2);
        assert_eq!(stats.for_table("posts").entity_misses, 1);
        assert_eq!(stats.for_table("nothing"), Counts::default());

        let total = stats.total();
        assert_eq!(total.entity_hits, 2);
        assert_eq!(total.entity_misses, 1);
        assert_eq!(total.query_misses, 1);

        stats.reset();
        assert_eq!(stats.total(), Counts::default());
    }
}
