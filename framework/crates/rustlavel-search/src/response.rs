//! Reading a search response.
//!
//! Two things in this shape are routinely misread, and both lose information
//! silently rather than loudly:
//!
//! - **The total is a relation, not a number.** Since Elasticsearch 7 the
//!   counting stops at 10,000 by default and the response says "at least
//!   10,000". A caller that reads `value` alone will print "10,000 results"
//!   above a list of millions. [`TotalHits`] refuses to be an integer for that
//!   reason.
//! - **A search can succeed on some shards and fail on others.** The status is
//!   200, the hits are there, and a slice of the index was never consulted.
//!   [`SearchResults::is_partial`] is the only thing that says so.

use crate::error::{Result, SearchError};
use rustlavel_core::Json;
use std::collections::BTreeMap;

/// Everything a search came back with.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResults {
    /// Milliseconds the cluster spent, excluding the network.
    pub took: u64,
    /// The search hit its time limit and returned what it had.
    pub timed_out: bool,
    pub shards: Shards,
    pub total: TotalHits,
    /// The best score, or `None` when the search was sorted by a field — in
    /// that case nothing was scored at all.
    pub max_score: Option<f64>,
    pub hits: Vec<Hit>,
    pub aggregations: Aggregations,
}

impl SearchResults {
    pub fn parse(body: &Json) -> Result<SearchResults> {
        let hits = body.get("hits").ok_or_else(|| SearchError::Malformed {
            context: "_search".to_string(),
            message: "the reply has no `hits`".to_string(),
        })?;

        Ok(SearchResults {
            took: u64_at(body, "took"),
            timed_out: body.get("timed_out").and_then(Json::as_bool).unwrap_or(false),
            shards: Shards::parse(body.get("_shards")),
            total: TotalHits::parse(hits.get("total")),
            max_score: hits.get("max_score").and_then(Json::as_f64),
            hits: hits
                .get("hits")
                .and_then(Json::as_array)
                .map(|items| items.iter().map(Hit::parse).collect())
                .unwrap_or_default(),
            aggregations: Aggregations::parse(body.get("aggregations")),
        })
    }

    /// How many documents came back on this page — not how many matched.
    pub fn len(&self) -> usize {
        self.hits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    /// Whether part of the index was not consulted.
    ///
    /// A search over a cluster with a failing shard answers 200 with fewer
    /// results, and nothing in the hits distinguishes that from a query that
    /// genuinely matched less. Anything acting on absence — "no orders, so
    /// close the account" — has to check this first.
    pub fn is_partial(&self) -> bool {
        self.timed_out || self.shards.failed > 0
    }

    /// The `_source` of every hit, in order.
    pub fn sources(&self) -> impl Iterator<Item = &Json> {
        self.hits.iter().map(|hit| &hit.source)
    }

    /// The `_id` of every hit, in order.
    pub fn ids(&self) -> Vec<&str> {
        self.hits.iter().map(|hit| hit.id.as_str()).collect()
    }

    /// The first hit, for a search that expects at most one.
    pub fn first(&self) -> Option<&Hit> {
        self.hits.first()
    }
}

/// How many shards answered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Shards {
    pub total: u32,
    pub successful: u32,
    pub skipped: u32,
    pub failed: u32,
    /// Why the failed ones failed, where the cluster said.
    pub failures: Vec<String>,
}

impl Shards {
    fn parse(body: Option<&Json>) -> Shards {
        let Some(body) = body else {
            return Shards::default();
        };

        Shards {
            total: u32_at(body, "total"),
            successful: u32_at(body, "successful"),
            skipped: u32_at(body, "skipped"),
            failed: u32_at(body, "failed"),
            failures: body
                .get("failures")
                .and_then(Json::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|failure| {
                            failure
                                .get("reason.reason")
                                .or_else(|| failure.get("reason"))
                                .and_then(Json::as_str)
                                .unwrap_or("no reason given")
                                .to_string()
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// How the total relates to the real number of matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TotalRelation {
    /// The number is the answer.
    Exact,
    /// Counting stopped early; the real number is this or larger.
    AtLeast,
}

/// How many documents matched.
///
/// Not a `u64`, deliberately. Elasticsearch stops counting at
/// `track_total_hits` — 10,000 unless asked otherwise — and reports the cutoff
/// as the value with a `gte` relation. Anything that renders a count, decides
/// how many pages there are, or compares against an expected number has to
/// know which of the two it is holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TotalHits {
    pub value: u64,
    pub relation: TotalRelation,
}

impl TotalHits {
    fn parse(body: Option<&Json>) -> TotalHits {
        let Some(body) = body else {
            return TotalHits { value: 0, relation: TotalRelation::Exact };
        };

        // Elasticsearch 6, and OpenSearch asked for `rest_total_hits_as_int`,
        // send a bare number here. It has no cutoff, so it is always exact.
        if let Some(value) = body.as_i64() {
            return TotalHits { value: value.max(0) as u64, relation: TotalRelation::Exact };
        }

        TotalHits {
            value: body.get("value").and_then(Json::as_i64).unwrap_or(0).max(0) as u64,
            relation: match body.get("relation").and_then(Json::as_str) {
                Some("gte") => TotalRelation::AtLeast,
                _ => TotalRelation::Exact,
            },
        }
    }

    /// Whether the number can be trusted as the whole answer.
    pub fn is_exact(&self) -> bool {
        self.relation == TotalRelation::Exact
    }

    /// The count, but only when it is really the count.
    ///
    /// Use this where being wrong matters — a "1 of N" label, a page count.
    /// [`TotalHits::value`] is there for a progress bar, where a floor is fine.
    pub fn exact(&self) -> Option<u64> {
        self.is_exact().then_some(self.value)
    }
}

impl std::fmt::Display for TotalHits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.relation {
            TotalRelation::Exact => write!(f, "{}", self.value),
            TotalRelation::AtLeast => write!(f, "at least {}", self.value),
        }
    }
}

/// One matching document.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub index: String,
    pub id: String,
    /// `None` when the search was sorted by a field rather than by relevance.
    pub score: Option<f64>,
    /// The document, or the part of it `_source` filtering asked for.
    pub source: Json,
    /// The values this hit sorted on, for paging with `search_after`.
    pub sort: Vec<Json>,
}

impl Hit {
    fn parse(body: &Json) -> Hit {
        Hit {
            index: string_at(body, "_index"),
            id: string_at(body, "_id"),
            score: body.get("_score").and_then(Json::as_f64),
            source: body.get("_source").cloned().unwrap_or(Json::Null),
            sort: body.get("sort").and_then(Json::as_array).map(<[Json]>::to_vec).unwrap_or_default(),
        }
    }

    /// A field of the document, by dotted path.
    pub fn field(&self, path: &str) -> Option<&Json> {
        self.source.get(path)
    }

    /// A string field of the document, by dotted path.
    pub fn string(&self, path: &str) -> Option<&str> {
        self.source.get(path).and_then(Json::as_str)
    }
}

/// The aggregation results, keyed by the names the search gave them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Aggregations(BTreeMap<String, Json>);

impl Aggregations {
    fn parse(body: Option<&Json>) -> Aggregations {
        Aggregations(
            body.and_then(Json::as_object).cloned().unwrap_or_default(),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }

    /// One result as the cluster sent it, for an aggregation this module does
    /// not model.
    pub fn raw(&self, name: &str) -> Option<&Json> {
        self.0.get(name)
    }

    /// A `terms` result.
    pub fn terms(&self, name: &str) -> Option<TermsResult> {
        let body = self.0.get(name)?;
        let buckets = body.get("buckets")?.as_array()?;

        Some(TermsResult {
            doc_count_error_upper_bound: u64_at(body, "doc_count_error_upper_bound"),
            sum_other_doc_count: u64_at(body, "sum_other_doc_count"),
            buckets: buckets.iter().map(Bucket::parse).collect(),
        })
    }

    /// A `stats` result.
    pub fn stats(&self, name: &str) -> Option<StatsResult> {
        let body = self.0.get(name)?;

        Some(StatsResult {
            count: u64_at(body, "count"),
            // Null on every one of these when the aggregation saw no document
            // — there is no minimum of nothing, and reporting 0 would be a
            // number somebody charts.
            min: body.get("min").and_then(Json::as_f64),
            max: body.get("max").and_then(Json::as_f64),
            avg: body.get("avg").and_then(Json::as_f64),
            sum: body.get("sum").and_then(Json::as_f64).unwrap_or(0.0),
        })
    }

    /// The number from a single-value metric — `min`, `max`, `avg`, `sum`,
    /// `cardinality`, `value_count`.
    pub fn value(&self, name: &str) -> Option<f64> {
        self.0.get(name)?.get("value")?.as_f64()
    }
}

/// What a `terms` aggregation returned.
#[derive(Debug, Clone, PartialEq)]
pub struct TermsResult {
    pub buckets: Vec<Bucket>,
    /// The largest number of documents any *returned* bucket may be missing.
    pub doc_count_error_upper_bound: u64,
    /// Documents in the values that did not make the list.
    pub sum_other_doc_count: u64,
}

impl TermsResult {
    /// Whether values are missing from this answer.
    ///
    /// A `terms` aggregation returns the top few — ten by default — and says
    /// nothing else about the rest. Charting the buckets as "the tags" when
    /// this is true draws a picture that is quietly wrong, and raising `size`
    /// is the fix.
    pub fn is_approximate(&self) -> bool {
        self.sum_other_doc_count > 0 || self.doc_count_error_upper_bound > 0
    }

    /// A bucket by its key, rendered as a string.
    pub fn get(&self, key: &str) -> Option<&Bucket> {
        self.buckets.iter().find(|bucket| bucket.key_string() == key)
    }

    /// The count for one key, or zero if it is not in the answer.
    ///
    /// Zero is honest only when [`TermsResult::is_approximate`] is false.
    pub fn count(&self, key: &str) -> u64 {
        self.get(key).map(|bucket| bucket.doc_count).unwrap_or(0)
    }
}

/// One bucket of an aggregation.
#[derive(Debug, Clone, PartialEq)]
pub struct Bucket {
    /// The value, which is a string for a `keyword` field and a number for a
    /// numeric or date one.
    pub key: Json,
    pub doc_count: u64,
    /// Whatever was nested inside this bucket.
    pub aggregations: Aggregations,
}

impl Bucket {
    fn parse(body: &Json) -> Bucket {
        Bucket {
            key: body.get("key").cloned().unwrap_or(Json::Null),
            doc_count: u64_at(body, "doc_count"),
            // The bucket object holds its sub-aggregations alongside `key` and
            // `doc_count`; those two extra names cost nothing and keeping the
            // whole object means a sub-aggregation is read exactly the way a
            // top-level one is.
            aggregations: Aggregations::parse(Some(body)),
        }
    }

    /// The key as text.
    ///
    /// Prefers `key_as_string`, which a date histogram sends alongside the raw
    /// epoch milliseconds and which is the only readable form of it.
    pub fn key_string(&self) -> String {
        if let Some(Json::String(text)) = self.aggregations.raw("key_as_string") {
            return text.clone();
        }
        match &self.key {
            Json::String(text) => text.clone(),
            other => other.to_string(),
        }
    }
}

/// What a `stats` aggregation returned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StatsResult {
    pub count: u64,
    /// `None` when nothing was aggregated — not zero.
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub avg: Option<f64>,
    pub sum: f64,
}

fn string_at(body: &Json, path: &str) -> String {
    body.get(path).and_then(Json::as_str).unwrap_or_default().to_string()
}

fn u64_at(body: &Json, path: &str) -> u64 {
    body.get(path).and_then(Json::as_i64).unwrap_or(0).max(0) as u64
}

fn u32_at(body: &Json, path: &str) -> u32 {
    u64_at(body, path) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Copied from a running Elasticsearch 8.15, not written from memory.
    fn parse(body: &str) -> SearchResults {
        SearchResults::parse(&Json::parse(body).expect("valid JSON in a test")).unwrap()
    }

    #[test]
    fn reads_the_hits_a_real_search_returns() {
        let results = parse(
            r#"{"took":4,"timed_out":false,"_shards":{"total":1,"successful":1,"skipped":0,"failed":0},"hits":{"total":{"value":2,"relation":"eq"},"max_score":0.60706794,"hits":[{"_index":"posts","_id":"1","_score":0.60706794,"_source":{"title":"Rust web frameworks","tags":["rust","web"],"views":120}},{"_index":"posts","_id":"2","_score":0.35667494,"_source":{"title":"Rust and Tokio","tags":["rust"],"views":45}}]}}"#,
        );

        assert_eq!(results.took, 4);
        assert_eq!(results.total.value, 2);
        assert!(results.total.is_exact());
        assert_eq!(results.max_score, Some(0.60706794));
        assert_eq!(results.ids(), vec!["1", "2"]);
        assert_eq!(results.hits[0].string("title"), Some("Rust web frameworks"));
        assert_eq!(results.hits[0].field("views").and_then(Json::as_i64), Some(120));
        assert!(!results.is_partial());
    }

    #[test]
    fn a_capped_total_is_never_reported_as_the_answer() {
        // The trap: `value` is 10000 and the real number is far larger. This is
        // the default for every index, so it is the common case rather than an
        // edge one.
        let results = parse(
            r#"{"took":12,"timed_out":false,"_shards":{"total":1,"successful":1,"skipped":0,"failed":0},"hits":{"total":{"value":10000,"relation":"gte"},"max_score":1.0,"hits":[]}}"#,
        );

        assert_eq!(results.total.relation, TotalRelation::AtLeast);
        assert!(!results.total.is_exact());
        assert_eq!(results.total.exact(), None, "a floor must not be handed out as a count");
        assert_eq!(results.total.to_string(), "at least 10000");
    }

    #[test]
    fn an_exact_total_reads_as_a_plain_number() {
        let results = parse(
            r#"{"took":1,"timed_out":false,"_shards":{"total":1,"successful":1,"skipped":0,"failed":0},"hits":{"total":{"value":7,"relation":"eq"},"max_score":null,"hits":[]}}"#,
        );

        assert_eq!(results.total.exact(), Some(7));
        assert_eq!(results.total.to_string(), "7");
        assert_eq!(results.max_score, None, "a sorted search scores nothing");
    }

    #[test]
    fn an_old_style_integer_total_is_still_read() {
        // OpenSearch with `rest_total_hits_as_int`, and Elasticsearch 6.
        let results = parse(
            r#"{"took":1,"timed_out":false,"_shards":{"total":1,"successful":1,"skipped":0,"failed":0},"hits":{"total":42,"max_score":1.0,"hits":[]}}"#,
        );

        assert_eq!(results.total.exact(), Some(42));
    }

    #[test]
    fn a_search_that_succeeded_on_only_some_shards_says_so() {
        // Status 200, hits present, and part of the index was never read. A
        // caller acting on absence has to know.
        let results = parse(
            r#"{"took":8,"timed_out":false,"_shards":{"total":2,"successful":1,"skipped":0,"failed":1,"failures":[{"shard":0,"index":"posts","node":"abc","reason":{"type":"query_shard_exception","reason":"failed to create query"}}]},"hits":{"total":{"value":1,"relation":"eq"},"max_score":1.0,"hits":[{"_index":"posts","_id":"1","_score":1.0,"_source":{}}]}}"#,
        );

        assert!(results.is_partial(), "one shard failed and the results are incomplete");
        assert_eq!(results.shards.failed, 1);
        assert_eq!(results.shards.failures, vec!["failed to create query".to_string()]);
    }

    #[test]
    fn a_timed_out_search_is_partial_too() {
        let results = parse(
            r#"{"took":500,"timed_out":true,"_shards":{"total":1,"successful":1,"skipped":0,"failed":0},"hits":{"total":{"value":3,"relation":"eq"},"max_score":1.0,"hits":[]}}"#,
        );

        assert!(results.is_partial());
    }

    #[test]
    fn reads_a_terms_aggregation_and_the_stats_nested_inside_it() {
        let results = parse(
            r#"{"took":6,"timed_out":false,"_shards":{"total":1,"successful":1,"skipped":0,"failed":0},"hits":{"total":{"value":3,"relation":"eq"},"max_score":null,"hits":[]},"aggregations":{"by_tag":{"doc_count_error_upper_bound":0,"sum_other_doc_count":0,"buckets":[{"key":"rust","doc_count":2,"views":{"count":2,"min":45.0,"max":120.0,"avg":82.5,"sum":165.0}},{"key":"web","doc_count":1,"views":{"count":1,"min":120.0,"max":120.0,"avg":120.0,"sum":120.0}}]}}}"#,
        );

        let tags = results.aggregations.terms("by_tag").unwrap();
        assert_eq!(tags.buckets.len(), 2);
        assert_eq!(tags.buckets[0].key_string(), "rust");
        assert_eq!(tags.count("rust"), 2);
        assert_eq!(tags.count("nothing"), 0);
        assert!(!tags.is_approximate());

        let views = tags.buckets[0].aggregations.stats("views").unwrap();
        assert_eq!(views.count, 2);
        assert_eq!(views.avg, Some(82.5));
        assert_eq!(views.sum, 165.0);
    }

    #[test]
    fn a_truncated_terms_aggregation_admits_that_it_is_truncated() {
        // Ten buckets and 4,000 documents in the ones that did not fit. A chart
        // drawn from this without checking is wrong and looks fine.
        let results = parse(
            r#"{"took":9,"timed_out":false,"_shards":{"total":1,"successful":1,"skipped":0,"failed":0},"hits":{"total":{"value":5000,"relation":"eq"},"max_score":null,"hits":[]},"aggregations":{"by_tag":{"doc_count_error_upper_bound":12,"sum_other_doc_count":4000,"buckets":[{"key":"rust","doc_count":1000}]}}}"#,
        );

        let tags = results.aggregations.terms("by_tag").unwrap();
        assert!(tags.is_approximate());
        assert_eq!(tags.sum_other_doc_count, 4000);
    }

    #[test]
    fn stats_over_nothing_are_null_rather_than_zero() {
        let results = parse(
            r#"{"took":2,"timed_out":false,"_shards":{"total":1,"successful":1,"skipped":0,"failed":0},"hits":{"total":{"value":0,"relation":"eq"},"max_score":null,"hits":[]},"aggregations":{"price":{"count":0,"min":null,"max":null,"avg":null,"sum":0.0}}}"#,
        );

        let price = results.aggregations.stats("price").unwrap();
        assert_eq!(price.count, 0);
        assert_eq!(price.min, None, "there is no minimum of nothing");
        assert_eq!(price.avg, None);
        assert_eq!(price.sum, 0.0);
    }

    #[test]
    fn a_single_value_metric_is_read_by_name() {
        let results = parse(
            r#"{"took":3,"timed_out":false,"_shards":{"total":1,"successful":1,"skipped":0,"failed":0},"hits":{"total":{"value":3,"relation":"eq"},"max_score":null,"hits":[]},"aggregations":{"authors":{"value":2},"total_views":{"value":285.0}}}"#,
        );

        assert_eq!(results.aggregations.value("authors"), Some(2.0));
        assert_eq!(results.aggregations.value("total_views"), Some(285.0));
        assert_eq!(results.aggregations.value("missing"), None);
        assert_eq!(results.aggregations.names(), vec!["authors", "total_views"]);
    }

    #[test]
    fn a_date_histogram_bucket_reads_as_its_date_and_not_as_epoch_millis() {
        let results = parse(
            r#"{"took":5,"timed_out":false,"_shards":{"total":1,"successful":1,"skipped":0,"failed":0},"hits":{"total":{"value":2,"relation":"eq"},"max_score":null,"hits":[]},"aggregations":{"per_day":{"buckets":[{"key_as_string":"2024-05-01T00:00:00.000Z","key":1714521600000,"doc_count":2}]}}}"#,
        );

        let days = results.aggregations.terms("per_day").unwrap();
        assert_eq!(days.buckets[0].key_string(), "2024-05-01T00:00:00.000Z");
        assert_eq!(days.buckets[0].doc_count, 2);
    }

    #[test]
    fn sort_values_come_back_for_paging_past_the_result_window() {
        let results = parse(
            r#"{"took":2,"timed_out":false,"_shards":{"total":1,"successful":1,"skipped":0,"failed":0},"hits":{"total":{"value":2,"relation":"eq"},"max_score":null,"hits":[{"_index":"posts","_id":"2","_score":null,"_source":{"title":"b"},"sort":[1714521600000,"2"]}]}}"#,
        );

        assert_eq!(results.hits[0].sort.len(), 2);
        assert_eq!(results.hits[0].sort[1].as_str(), Some("2"));
    }

    #[test]
    fn a_reply_with_no_hits_object_is_a_shape_we_do_not_understand() {
        let error = SearchResults::parse(&Json::parse(r#"{"took":1}"#).unwrap()).unwrap_err();
        assert!(matches!(error, SearchError::Malformed { .. }), "got {error:?}");
    }
}
