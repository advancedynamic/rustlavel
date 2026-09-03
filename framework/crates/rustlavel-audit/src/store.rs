//! Reading and writing the trail.

use crate::entry::{Builder, Entry};
use crate::tables::TABLE;
use rustlavel_core::{Json, Result};
use rustlavel_db::{Database, Value};

/// The audit trail. Cheap to clone; every clone writes to the same table.
/// No `Debug`: the only field is a database handle, and printing a connection
/// pool in a log is how a connection string ends up in a bug report.
#[derive(Clone)]
pub struct Trail {
    db: Database,
}

/// What to narrow a listing by. Every field is optional and they combine with
/// AND, which is what the filter bar on an audit page does.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub user_id: Option<i64>,
    pub event: Option<String>,
    pub model_type: Option<String>,
    /// Inclusive, as `YYYY-MM-DD`.
    pub from: Option<String>,
    /// Inclusive: the whole of that day, not the midnight at the start of it.
    pub to: Option<String>,
    pub ip_address: Option<String>,
    /// Matched against the description, the event and the actor's name.
    pub search: Option<String>,
}

/// One page of entries, and enough to draw a pager.
#[derive(Debug, Clone)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub total: i64,
    pub page: i64,
    pub per_page: i64,
}

impl Page {
    pub fn pages(&self) -> i64 {
        if self.per_page <= 0 { 1 } else { ((self.total + self.per_page - 1) / self.per_page).max(1) }
    }

    /// The 1-based index of the first row on this page, for "showing 1 to 50".
    pub fn first(&self) -> i64 {
        if self.total == 0 { 0 } else { (self.page - 1) * self.per_page + 1 }
    }

    pub fn last(&self) -> i64 {
        ((self.page - 1) * self.per_page + self.entries.len() as i64).max(0)
    }
}

impl Trail {
    pub fn new(db: Database) -> Trail {
        Trail { db }
    }

    pub fn database(&self) -> &Database {
        &self.db
    }

    /// Start an entry. Nothing is written until [`Builder::save`].
    pub fn event(&self, event: impl Into<String>) -> Builder {
        Builder { trail: self.clone(), entry: Entry::new(event) }
    }

    /// Write one, returning its id.
    pub async fn write(&self, entry: Entry) -> Result<i64> {
        let properties = match &entry.properties {
            Json::Null => None,
            value => Some(value.to_string()),
        };
        let now = now();

        self.db
            .table(TABLE)
            .insert(
                &self.db,
                &[
                    ("user_id", entry.user_id.map_or(Value::Null, Value::from)),
                    ("user_name", optional(entry.user_name)),
                    ("event", Value::from(entry.event)),
                    ("model_type", optional(entry.model_type)),
                    ("model_id", optional(entry.model_id)),
                    ("description", optional(entry.description)),
                    ("properties", optional(properties)),
                    ("ip_address", optional(entry.ip_address)),
                    // Long agent strings are a nuisance in a table and tell
                    // nobody anything past the first line's worth.
                    ("user_agent", optional(entry.user_agent.map(|a| a.chars().take(255).collect::<String>()))),
                    ("created_at", Value::from(now.clone())),
                    ("updated_at", Value::from(now)),
                ],
            )
            .await
    }

    /// One page, newest first.
    pub async fn page(&self, filter: &Filter, page: i64, per_page: i64) -> Result<Page> {
        let page = page.max(1);
        let per_page = per_page.clamp(1, 500);

        let total = self.query(filter).count(&self.db).await?;
        let rows = self
            .query(filter)
            .latest("id")
            .limit(per_page)
            .offset((page - 1) * per_page)
            .get(&self.db)
            .await?;

        Ok(Page { entries: rows.iter().map(read).collect(), total, page, per_page })
    }

    /// Every matching entry, newest first, up to `limit`. For an export.
    pub async fn all(&self, filter: &Filter, limit: i64) -> Result<Vec<Entry>> {
        let rows =
            self.query(filter).latest("id").limit(limit.clamp(1, 100_000)).get(&self.db).await?;
        Ok(rows.iter().map(read).collect())
    }

    pub async fn find(&self, id: i64) -> Result<Option<Entry>> {
        let rows = self.db.table(TABLE).filter("id", id).limit(1).get(&self.db).await?;
        Ok(rows.first().map(read))
    }

    /// Every distinct event name that has been recorded, for a filter's
    /// dropdown. Read from the trail rather than declared, so an application
    /// that records something new does not also have to register it.
    pub async fn events(&self) -> Result<Vec<String>> {
        let rows = self.db.table(TABLE).group_by(&["event"]).select(&["event"]).get(&self.db).await?;
        let mut names: Vec<String> =
            rows.iter().filter_map(|row| row.get::<String>("event").ok()).collect();
        names.sort();
        Ok(names)
    }

    /// The same for the model types.
    pub async fn model_types(&self) -> Result<Vec<String>> {
        let rows = self
            .db
            .table(TABLE)
            .filter_not_null("model_type")
            .group_by(&["model_type"])
            .select(&["model_type"])
            .get(&self.db)
            .await?;
        let mut names: Vec<String> =
            rows.iter().filter_map(|row| row.get::<String>("model_type").ok()).filter(|n| !n.is_empty()).collect();
        names.sort();
        Ok(names)
    }

    pub async fn count(&self, filter: &Filter) -> Result<i64> {
        self.query(filter).count(&self.db).await
    }

    /// How many entries were written on or after `since`.
    pub async fn count_since(&self, since: &str) -> Result<i64> {
        self.db.table(TABLE).filter_op("created_at", ">=", since).count(&self.db).await
    }

    /// How many different accounts appear in the trail.
    pub async fn distinct_users(&self) -> Result<i64> {
        let rows = self
            .db
            .table(TABLE)
            .filter_not_null("user_id")
            .group_by(&["user_id"])
            .select(&["user_id"])
            .get(&self.db)
            .await?;
        Ok(rows.len() as i64)
    }

    /// When the last entry was written, if there is one.
    pub async fn latest_at(&self) -> Result<Option<String>> {
        let rows = self.db.table(TABLE).latest("id").limit(1).get(&self.db).await?;
        Ok(rows.first().and_then(|row| row.get::<String>("created_at").ok()))
    }

    /// Delete everything older than a date. A trail nobody prunes becomes the
    /// largest table in the database, and a retention rule is a decision the
    /// application makes rather than one this package makes for it.
    pub async fn prune_before(&self, cutoff: &str) -> Result<u64> {
        self.db.table(TABLE).filter_op("created_at", "<", cutoff).delete(&self.db).await
    }

    fn query(&self, filter: &Filter) -> rustlavel_db::QueryBuilder {
        let mut query = self.db.table(TABLE);

        if let Some(user_id) = filter.user_id {
            query = query.filter("user_id", user_id);
        }
        if let Some(event) = filter.event.as_deref().filter(|e| !e.is_empty()) {
            query = query.filter("event", event);
        }
        if let Some(kind) = filter.model_type.as_deref().filter(|m| !m.is_empty()) {
            query = query.filter("model_type", kind);
        }
        if let Some(from) = filter.from.as_deref().filter(|d| !d.is_empty()) {
            query = query.filter_op("created_at", ">=", format!("{from} 00:00:00"));
        }
        if let Some(to) = filter.to.as_deref().filter(|d| !d.is_empty()) {
            // The whole of the closing day. `<= "2026-09-03"` would compare
            // against midnight and drop everything that happened that day,
            // which is the day somebody filtering for it most wants.
            query = query.filter_op("created_at", "<=", format!("{to} 23:59:59"));
        }
        if let Some(ip) = filter.ip_address.as_deref().filter(|i| !i.is_empty()) {
            query = query.filter("ip_address", ip);
        }
        if let Some(search) = filter.search.as_deref().filter(|s| !s.is_empty()) {
            let pattern = format!("%{}%", escape_like(search));
            // Parenthesised, and that matters: without the group an OR would
            // reach outside and quietly widen every other filter on the bar.
            query = query.group_filter(|group| {
                group
                    .filter_like("description", pattern.clone())
                    .or_filter_like("event", pattern.clone())
                    .or_filter_like("user_name", pattern.clone())
            });
        }

        query
    }
}

fn optional(value: Option<String>) -> Value {
    value.filter(|v| !v.is_empty()).map_or(Value::Null, Value::from)
}

/// `%` and `_` are wildcards in LIKE, so a search for `50%` must not become a
/// search for "50 followed by anything".
fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn read(row: &rustlavel_db::Row) -> Entry {
    Entry {
        id: row.get("id").unwrap_or_default(),
        user_id: row.get::<i64>("user_id").ok(),
        user_name: row.get::<String>("user_name").ok().filter(|n| !n.is_empty()),
        event: row.get("event").unwrap_or_default(),
        model_type: row.get::<String>("model_type").ok().filter(|n| !n.is_empty()),
        model_id: row.get::<String>("model_id").ok().filter(|n| !n.is_empty()),
        description: row.get::<String>("description").ok().filter(|n| !n.is_empty()),
        properties: row
            .get::<String>("properties")
            .ok()
            .filter(|p| !p.is_empty())
            .and_then(|p| Json::parse(&p).ok())
            .unwrap_or(Json::Null),
        ip_address: row.get::<String>("ip_address").ok().filter(|n| !n.is_empty()),
        user_agent: row.get::<String>("user_agent").ok().filter(|n| !n.is_empty()),
        created_at: row.get("created_at").unwrap_or_default(),
    }
}

/// `YYYY-MM-DD HH:MM:SS` in UTC.
pub fn now() -> String {
    format_utc(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64),
    )
}

/// Howard Hinnant's civil calendar, the same one the HTTP date code uses.
pub fn format_utc(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_reports_its_own_bounds() {
        let page = Page { entries: vec![Entry::new("a"); 50], total: 305, page: 1, per_page: 50 };

        assert_eq!(page.pages(), 7);
        assert_eq!(page.first(), 1);
        assert_eq!(page.last(), 50);

        let page = Page { entries: vec![Entry::new("a"); 5], total: 305, page: 7, per_page: 50 };
        assert_eq!(page.first(), 301);
        assert_eq!(page.last(), 305);

        // An empty trail says "0 of 0" rather than "1 of 0".
        let page = Page { entries: vec![], total: 0, page: 1, per_page: 50 };
        assert_eq!(page.first(), 0);
        assert_eq!(page.last(), 0);
        assert_eq!(page.pages(), 1);
    }

    #[test]
    fn a_search_cannot_smuggle_wildcards_in() {
        assert_eq!(escape_like("50%"), "50\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("100\\%"), "100\\\\\\%");
    }

    #[test]
    fn an_entry_without_a_description_still_says_something() {
        let mut entry = Entry::new("users.deleted");
        assert_eq!(entry.summary(), "Somebody performed users.deleted");

        entry.user_name = Some("Ada Lovelace".into());
        entry.model_type = Some("User".into());
        entry.model_id = Some("7".into());
        assert_eq!(entry.summary(), "Ada Lovelace performed users.deleted on User #7");

        entry.description = Some("Ada Lovelace deleted Alan Turing".into());
        assert_eq!(entry.summary(), "Ada Lovelace deleted Alan Turing");
    }

    #[test]
    fn the_epoch_formats_the_way_the_rest_of_the_framework_stores_a_time() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00");
        assert_eq!(format_utc(1_788_400_000), "2026-09-03 01:46:40");
    }
}
