//! Per-model settings.

use std::time::Duration;

/// How a region is kept in step with the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Entities and queries are dropped when the table is written to.
    ///
    /// The default, and the only one that is safe without knowing the
    /// application: a write bumps the table's generation and every cached
    /// query for it becomes a miss.
    ReadWrite,
    /// The table is never written to, so nothing is ever invalidated.
    ///
    /// For reference data — countries, currencies, a list of statuses. It
    /// skips the generation read entirely, which is one round trip less per
    /// query, and it is **a promise the caller makes**: a write to a read-only
    /// region is served stale until the entry expires. [`Region::ttl`] is what
    /// bounds that, so a read-only region with no TTL is refused.
    ReadOnly,
}

/// The settings for one model's cache.
#[derive(Debug, Clone)]
pub struct Region {
    pub(crate) strategy: Strategy,
    pub(crate) ttl: Option<Duration>,
    pub(crate) entities: bool,
    pub(crate) queries: bool,
    /// The most rows a single cached query may hold.
    ///
    /// A cache is for the queries a page runs over and over, not for a report
    /// that returns fifty thousand rows — putting one of those in Redis
    /// serialises the whole result set on every miss and pushes everything
    /// else out. Over the limit, the query runs and its result is not stored.
    pub(crate) max_rows: usize,
}

impl Region {
    pub fn new() -> Region {
        Region::default()
    }

    /// A region for data that is never written to. Needs a TTL — see
    /// [`Strategy::ReadOnly`].
    pub fn read_only() -> Region {
        Region { strategy: Strategy::ReadOnly, ttl: Some(Duration::from_secs(3600)), ..Region::default() }
    }

    pub fn ttl(mut self, ttl: Duration) -> Region {
        self.ttl = Some(ttl);
        self
    }

    /// Keep entries until something invalidates them.
    ///
    /// Refused for a read-only region, where nothing ever does.
    pub fn forever(mut self) -> Region {
        self.ttl = None;
        self
    }

    /// Cache entities by key, but not query results.
    pub fn entities_only(mut self) -> Region {
        self.queries = false;
        self
    }

    /// Cache query results, but not entities.
    pub fn queries_only(mut self) -> Region {
        self.entities = false;
        self
    }

    pub fn max_rows(mut self, rows: usize) -> Region {
        self.max_rows = rows;
        self
    }

    pub fn strategy(&self) -> Strategy {
        self.strategy
    }

    /// Whether the settings contradict each other.
    ///
    /// Checked when the region is registered rather than when it is first
    /// read, so a mistake is a startup failure and not a stale row six weeks
    /// from now.
    pub(crate) fn check(&self, model: &str) -> rustlavel_core::Result<()> {
        if self.strategy == Strategy::ReadOnly && self.ttl.is_none() {
            return Err(rustlavel_core::Error::msg(format!(
                "the `{model}` region is read-only with no expiry, so nothing would ever \
                 remove an entry from it — not a write, not a sweep. Give it a `ttl(...)`, \
                 or use the default read-write strategy."
            )));
        }
        if !self.entities && !self.queries {
            return Err(rustlavel_core::Error::msg(format!(
                "the `{model}` region caches neither entities nor queries, so registering \
                 it does nothing. Drop the region, or turn one of them back on."
            )));
        }
        Ok(())
    }
}

impl Default for Region {
    fn default() -> Region {
        Region {
            strategy: Strategy::ReadWrite,
            // Five minutes. Long enough to be worth having, short enough that
            // a mistake somewhere else in the application ages out on its own
            // rather than needing somebody to notice.
            ttl: Some(Duration::from_secs(300)),
            entities: true,
            queries: true,
            max_rows: 500,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_only_region_with_no_expiry_is_refused() {
        let region = Region::read_only().forever();
        let error = region.check("Country").unwrap_err().to_string();

        assert!(error.contains("nothing would ever remove an entry"), "{error}");
        assert!(error.contains("Country"), "the message does not say which region: {error}");

        // With a TTL it is fine, and so is a read-write region that never expires.
        assert!(Region::read_only().check("Country").is_ok());
        assert!(Region::new().forever().check("User").is_ok());
    }

    #[test]
    fn a_region_that_caches_nothing_is_refused() {
        let region = Region::new().entities_only().queries_only();
        let error = region.check("User").unwrap_err().to_string();

        assert!(error.contains("neither entities nor queries"), "{error}");
    }

    #[test]
    fn the_defaults_are_the_conservative_ones() {
        let region = Region::default();

        assert_eq!(region.strategy, Strategy::ReadWrite);
        assert_eq!(region.ttl, Some(Duration::from_secs(300)));
        assert!(region.entities && region.queries);
    }
}
