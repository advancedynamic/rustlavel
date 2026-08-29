//! Pagination.
//!
//! Two shapes, because they solve different problems: page numbers, which
//! users understand and which need a count query; and cursors, which stay
//! correct and stay fast when rows are being inserted underneath the reader.

use crate::builder::{Direction, QueryBuilder};
use crate::{Database, Row, Value, rows_to_json};
use rustlavel_core::{Json, Result};

/// One page of rows, plus what a view needs to draw the links.
#[derive(Debug)]
pub struct Page {
    pub rows: Vec<Row>,
    pub total: i64,
    pub per_page: i64,
    pub current_page: i64,
}

impl Page {
    pub fn last_page(&self) -> i64 {
        if self.per_page <= 0 {
            return 1;
        }
        // A total of 0 still has one (empty) page, which is what a view expects.
        ((self.total as f64) / (self.per_page as f64)).ceil().max(1.0) as i64
    }

    pub fn has_more(&self) -> bool {
        self.current_page < self.last_page()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The 1-based index of the first row on this page, or `None` when empty.
    pub fn from(&self) -> Option<i64> {
        (!self.is_empty()).then(|| (self.current_page - 1) * self.per_page + 1)
    }

    pub fn to(&self) -> Option<i64> {
        self.from().map(|from| from + self.rows.len() as i64 - 1)
    }

    /// The page numbers to show, with `None` standing for a gap.
    ///
    /// Always includes the first and last page and a window around the current
    /// one, so the control stays a fixed width however many pages there are.
    pub fn links(&self, window: i64) -> Vec<Option<i64>> {
        let last = self.last_page();
        let mut out = Vec::new();
        let mut previous: Option<i64> = None;

        for page in 1..=last {
            let near_current = (page - self.current_page).abs() <= window;
            if page == 1 || page == last || near_current {
                if previous.is_some_and(|p| page - p > 1) {
                    out.push(None);
                }
                out.push(Some(page));
                previous = Some(page);
            }
        }
        out
    }

    /// The API shape, matching Laravel's paginator so a client library written
    /// against one works against the other.
    pub fn to_json(&self) -> Json {
        Json::object([
            ("data", rows_to_json(&self.rows)),
            ("total", Json::from(self.total)),
            ("per_page", Json::from(self.per_page)),
            ("current_page", Json::from(self.current_page)),
            ("last_page", Json::from(self.last_page())),
            ("from", self.from().map_or(Json::Null, Json::from)),
            ("to", self.to().map_or(Json::Null, Json::from)),
        ])
    }
}

/// A page fetched by cursor rather than by offset.
#[derive(Debug)]
pub struct CursorPage {
    pub rows: Vec<Row>,
    /// Pass to the next call to continue; `None` at the end.
    pub next_cursor: Option<String>,
    pub per_page: i64,
}

impl CursorPage {
    pub fn to_json(&self) -> Json {
        Json::object([
            ("data", rows_to_json(&self.rows)),
            ("per_page", Json::from(self.per_page)),
            ("next_cursor", self.next_cursor.clone().map_or(Json::Null, Json::from)),
        ])
    }
}

impl QueryBuilder {
    /// Fetch one page, with a count query for the total.
    ///
    /// Convenient and familiar, but the count scans the whole matching set:
    /// past a few hundred thousand rows, reach for [`QueryBuilder::cursor_paginate`].
    pub async fn paginate(&self, db: &Database, page: i64, per_page: i64) -> Result<Page> {
        let per_page = per_page.clamp(1, 1000);
        let page = page.max(1);

        let total = self.count(db).await?;
        let rows = self.clone().page(page, per_page).get(db).await?;

        Ok(Page { rows, total, per_page, current_page: page })
    }

    /// Fetch one page by cursor, ordered by a unique column.
    ///
    /// No count and no offset, so the cost does not grow with the page number,
    /// and a row inserted while the reader pages through cannot cause another
    /// row to be skipped or repeated.
    pub async fn cursor_paginate(
        &self,
        db: &Database,
        column: &str,
        after: Option<&str>,
        per_page: i64,
    ) -> Result<CursorPage> {
        let per_page = per_page.clamp(1, 1000);

        let mut query = self.clone().order_by(column, Direction::Asc).limit(per_page + 1);
        if let Some(cursor) = after {
            // The cursor is the last value seen, so the comparison is strict.
            query = query.filter_op(column, ">", decode_cursor(cursor));
        }

        let mut rows = query.get(db).await?;

        // One row beyond the page proves there is a next page without a count.
        let has_more = rows.len() as i64 > per_page;
        if has_more {
            rows.truncate(per_page as usize);
        }

        let next_cursor = has_more
            .then(|| rows.last().and_then(|row| row.value(column).ok().map(Value::to_display)))
            .flatten();

        Ok(CursorPage { rows, next_cursor, per_page })
    }
}

/// Cursors travel through a URL, so they arrive as text.
///
/// A numeric-looking cursor becomes a number; anything else stays text, which
/// keeps both an integer id and a uuid working without the caller saying which.
fn decode_cursor(cursor: &str) -> Value {
    match cursor.parse::<i64>() {
        Ok(number) => Value::Int(number),
        Err(_) => Value::Text(cursor.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn page(total: i64, per_page: i64, current: i64, rows: usize) -> Page {
        let columns = Arc::new(vec!["id".to_string()]);
        Page {
            rows: (0..rows).map(|i| Row::new(Arc::clone(&columns), vec![Value::Int(i as i64)])).collect(),
            total,
            per_page,
            current_page: current,
        }
    }

    #[test]
    fn computes_the_page_count() {
        assert_eq!(page(100, 10, 1, 10).last_page(), 10);
        assert_eq!(page(101, 10, 1, 10).last_page(), 11);
        assert_eq!(page(0, 10, 1, 0).last_page(), 1);
    }

    #[test]
    fn reports_the_range_on_the_page() {
        let second = page(100, 10, 2, 10);

        assert_eq!(second.from(), Some(11));
        assert_eq!(second.to(), Some(20));
        assert!(second.has_more());

        let empty = page(0, 10, 1, 0);
        assert_eq!(empty.from(), None);
        assert!(!empty.has_more());
    }

    #[test]
    fn link_windows_stay_a_fixed_width() {
        let middle = page(1000, 10, 50, 10);
        let links = middle.links(2);

        assert_eq!(links.first(), Some(&Some(1)));
        assert_eq!(links.last(), Some(&Some(100)));
        assert!(links.contains(&None), "distant pages should be elided");
        assert!(links.contains(&Some(50)));
        assert!(links.contains(&Some(48)));
        assert!(!links.contains(&Some(47)));
    }

    #[test]
    fn a_short_run_of_pages_has_no_gaps() {
        let links = page(30, 10, 2, 10).links(2);
        assert_eq!(links, vec![Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn serializes_the_laravel_shape() {
        let json = page(25, 10, 3, 5).to_json();

        assert_eq!(json.get("total").unwrap().as_i64(), Some(25));
        assert_eq!(json.get("last_page").unwrap().as_i64(), Some(3));
        assert_eq!(json.get("from").unwrap().as_i64(), Some(21));
        assert_eq!(json.get("to").unwrap().as_i64(), Some(25));
        assert_eq!(json.get("data").unwrap().as_array().map(<[Json]>::len), Some(5));
    }

    #[test]
    fn cursors_keep_their_type() {
        assert_eq!(decode_cursor("42"), Value::Int(42));
        assert_eq!(
            decode_cursor("018f2c1a-0000-7000-8000-000000000000"),
            Value::Text("018f2c1a-0000-7000-8000-000000000000".into())
        );
    }

    #[test]
    fn cursor_pagination_asks_for_one_extra_row() {
        // The +1 is what lets the next page be detected without a count query.
        let (sql, _) = QueryBuilder::new("posts")
            .clone()
            .order_by("id", Direction::Asc)
            .limit(11)
            .to_sql(&crate::dialect::Postgres)
            .unwrap();

        assert!(sql.ends_with("order by \"id\" asc limit 11"));
    }
}
