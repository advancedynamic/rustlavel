//! Building a query without writing the DSL by hand.
//!
//! Elasticsearch's query language is JSON, deeply nested, and unforgiving
//! about where a key sits — `size` one level too deep is not an error, it is
//! silently ignored. So the whole point of this module is that the nesting is
//! written once, here, and a caller never has to remember whether `filter`
//! belongs inside `bool` or beside it.
//!
//! ```ignore
//! let search = Search::new()
//!     .query(
//!         Query::bool()
//!             .must(Query::matching("title", "rust web framework").and())
//!             .filter(Query::term("status", "published"))
//!             .filter(Query::range("published_at").gte("2024-01-01"))
//!             .must_not(Query::term("archived", true)),
//!     )
//!     .sort_desc("published_at")
//!     .size(20)
//!     .source(["title", "author"])
//!     .aggregate("by_tag", Aggregation::terms("tags").size(10))
//!     .track_total_hits(true);
//! ```
//!
//! [`Query::raw`] and [`Aggregation::raw`] are the way out for the clauses this
//! module does not name. Reaching for them should not be the normal case, but
//! the DSL is far larger than any wrapper, and a builder with no escape hatch
//! is one somebody abandons the first time they need a `span_near`.

use rustlavel_core::Json;
use std::collections::BTreeMap;

/// One query clause.
///
/// Holds the JSON it will send, which is what makes [`Query::raw`] free and
/// keeps this from becoming a second, worse copy of the DSL's type system.
#[derive(Debug, Clone, PartialEq)]
pub struct Query(Json);

impl Query {
    /// Every document. What Elasticsearch assumes when no query is given.
    pub fn match_all() -> Query {
        Query(clause("match_all", empty()))
    }

    /// No document. Useful for an aggregation-only request, where the hits are
    /// paid for and thrown away.
    pub fn match_none() -> Query {
        Query(clause("match_none", empty()))
    }

    /// Full-text search on one analysed field.
    ///
    /// Returns a builder rather than a [`Query`] so the options that apply
    /// only to `match` — above all [`Match::and`] — are reachable. It converts
    /// wherever a `Query` is wanted.
    pub fn matching(field: impl Into<String>, text: impl Into<String>) -> Match {
        Match {
            kind: "match",
            field: field.into(),
            text: text.into(),
            options: BTreeMap::new(),
        }
    }

    /// The words in order and adjacent, rather than merely all present.
    pub fn match_phrase(field: impl Into<String>, text: impl Into<String>) -> Match {
        Match {
            kind: "match_phrase",
            field: field.into(),
            text: text.into(),
            options: BTreeMap::new(),
        }
    }

    /// Full-text search across several fields at once.
    ///
    /// A field may carry a weight — `"title^3"` — which is the ordinary way to
    /// say a match in the title counts for more than one in the body.
    pub fn multi_match<S: Into<String>>(
        text: impl Into<String>,
        fields: impl IntoIterator<Item = S>,
    ) -> Query {
        let fields: Vec<Json> = fields.into_iter().map(|f| Json::String(f.into())).collect();
        Query(clause(
            "multi_match",
            Json::object([("query", Json::String(text.into())), ("fields", Json::Array(fields))]),
        ))
    }

    /// An exact value, not analysed.
    ///
    /// `term` compares against what is *stored*, so a `text` field almost never
    /// matches one: `"Hello World"` is indexed as `hello` and `world`, and a
    /// term query for `Hello World` finds nothing while reporting no error at
    /// all. Use this on `keyword`, numeric, boolean and date fields, and
    /// [`Query::matching`] on `text`.
    pub fn term(field: impl Into<String>, value: impl Into<Json>) -> Query {
        Query(clause(
            "term",
            Json::object([(field.into(), Json::object([("value", value.into())]))]),
        ))
    }

    /// Any one of several exact values — an `IN` list.
    pub fn terms<V: Into<Json>>(
        field: impl Into<String>,
        values: impl IntoIterator<Item = V>,
    ) -> Query {
        let values: Vec<Json> = values.into_iter().map(Into::into).collect();
        Query(clause("terms", Json::object([(field.into(), Json::Array(values))])))
    }

    /// Documents where the field has any value at all.
    ///
    /// The negation is [`BoolQuery::must_not`] around this, which is also the
    /// only way to ask for "field is null": Elasticsearch does not index a
    /// null, so there is nothing to match on directly.
    pub fn exists(field: impl Into<String>) -> Query {
        Query(clause("exists", Json::object([("field", Json::String(field.into()))])))
    }

    /// A value starting with this prefix, against the stored term.
    pub fn prefix(field: impl Into<String>, value: impl Into<String>) -> Query {
        Query(clause(
            "prefix",
            Json::object([(field.into(), Json::object([("value", Json::String(value.into()))]))]),
        ))
    }

    /// A stored term matching a pattern, where `*` is any run and `?` is one
    /// character. Expensive on a large index, and a leading `*` especially so.
    pub fn wildcard(field: impl Into<String>, pattern: impl Into<String>) -> Query {
        Query(clause(
            "wildcard",
            Json::object([(field.into(), Json::object([("value", Json::String(pattern.into()))]))]),
        ))
    }

    /// Documents by `_id`.
    pub fn ids<S: Into<String>>(ids: impl IntoIterator<Item = S>) -> Query {
        let values: Vec<Json> = ids.into_iter().map(|id| Json::String(id.into())).collect();
        Query(clause("ids", Json::object([("values", Json::Array(values))])))
    }

    /// A bounded field: `Query::range("age").gte(18).lt(65)`.
    pub fn range(field: impl Into<String>) -> Range {
        Range { field: field.into(), bounds: BTreeMap::new() }
    }

    /// Combine clauses. See [`BoolQuery`] for what each slot means.
    pub fn bool() -> BoolQuery {
        BoolQuery::default()
    }

    /// Every clause must match, and each contributes to the score.
    pub fn all_of<Q: Into<Query>>(clauses: impl IntoIterator<Item = Q>) -> BoolQuery {
        let mut combined = BoolQuery::default();
        for clause in clauses {
            combined = combined.must(clause);
        }
        combined
    }

    /// At least one clause must match.
    ///
    /// Sets `minimum_should_match` to 1, which is the difference between "any
    /// of these" and "prefer these": a bare `should` beside a `must` is only a
    /// scoring hint, and a `should` list that nobody constrained happily
    /// matches documents satisfying none of it.
    pub fn any_of<Q: Into<Query>>(clauses: impl IntoIterator<Item = Q>) -> BoolQuery {
        let mut combined = BoolQuery::default();
        for clause in clauses {
            combined = combined.should(clause);
        }
        combined.minimum_should_match(1)
    }

    /// A clause this module does not name, written out as JSON.
    pub fn raw(json: Json) -> Query {
        Query(json)
    }

    pub fn json(&self) -> &Json {
        &self.0
    }

    pub fn into_json(self) -> Json {
        self.0
    }
}

/// A `match` or `match_phrase` being assembled.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    kind: &'static str,
    field: String,
    text: String,
    options: BTreeMap<String, Json>,
}

impl Match {
    /// Require every term, rather than any.
    ///
    /// The default operator is `or`, which surprises nearly everyone the first
    /// time: searching for `quick brown fox` returns documents containing only
    /// `fox`, ranked lower but returned all the same. When a search box is
    /// meant to narrow results as the user types, this is what makes it.
    pub fn and(self) -> Match {
        self.option("operator", Json::String("and".into()))
    }

    /// Tolerate typos, letting Elasticsearch choose the edit distance from the
    /// length of each term.
    pub fn fuzzy(self) -> Match {
        self.fuzziness("AUTO")
    }

    /// A specific edit distance: `"1"`, `"2"`, or `"AUTO"`.
    pub fn fuzziness(self, fuzziness: impl Into<String>) -> Match {
        self.option("fuzziness", Json::String(fuzziness.into()))
    }

    /// Weight this clause relative to the others in a `bool`.
    pub fn boost(self, boost: f64) -> Match {
        self.option("boost", Json::Number(boost))
    }

    /// How many terms have to match: a count, or a percentage like `"75%"`.
    pub fn minimum_should_match(self, value: impl Into<Json>) -> Match {
        self.option("minimum_should_match", value.into())
    }

    /// Any other option the clause takes.
    pub fn option(mut self, name: impl Into<String>, value: Json) -> Match {
        self.options.insert(name.into(), value);
        self
    }
}

impl From<Match> for Query {
    fn from(value: Match) -> Query {
        // The expanded form — `{"match": {field: {"query": text}}}` — is used
        // even with no options, because the short form has no room for one and
        // switching between the two on the fly is how a builder grows a bug.
        let mut body = BTreeMap::new();
        body.insert("query".to_string(), Json::String(value.text));
        body.extend(value.options);

        Query(clause(value.kind, Json::object([(value.field, Json::Object(body))])))
    }
}

/// A `range` being assembled.
#[derive(Debug, Clone, PartialEq)]
pub struct Range {
    field: String,
    bounds: BTreeMap<String, Json>,
}

impl Range {
    /// At or above.
    pub fn gte(self, value: impl Into<Json>) -> Range {
        self.bound("gte", value)
    }

    /// Strictly above.
    pub fn gt(self, value: impl Into<Json>) -> Range {
        self.bound("gt", value)
    }

    /// At or below.
    pub fn lte(self, value: impl Into<Json>) -> Range {
        self.bound("lte", value)
    }

    /// Strictly below.
    pub fn lt(self, value: impl Into<Json>) -> Range {
        self.bound("lt", value)
    }

    /// How to read the bounds, when they are dates in a shape the field's own
    /// mapping does not use.
    pub fn format(self, format: impl Into<String>) -> Range {
        self.bound("format", Json::String(format.into()))
    }

    /// The zone to interpret a date bound in.
    ///
    /// Without it a bare `2024-01-01` is UTC, which quietly shifts a "today"
    /// filter by up to a day for everyone not on UTC.
    pub fn time_zone(self, zone: impl Into<String>) -> Range {
        self.bound("time_zone", Json::String(zone.into()))
    }

    fn bound(mut self, name: &str, value: impl Into<Json>) -> Range {
        self.bounds.insert(name.to_string(), value.into());
        self
    }
}

impl From<Range> for Query {
    fn from(value: Range) -> Query {
        Query(clause("range", Json::object([(value.field, Json::Object(value.bounds))])))
    }
}

/// Clauses combined.
///
/// The four slots differ in two ways that are easy to confuse:
///
/// - `must` and `should` affect the score; `filter` and `must_not` do not, and
///   are cached, so anything that is a yes-or-no test belongs in `filter`.
/// - `must`, `filter` and `must_not` are requirements. `should` is one only
///   when nothing else in the query is — otherwise it merely raises the score
///   of documents that match it. [`BoolQuery::minimum_should_match`] is what
///   turns it back into a requirement, and [`Query::any_of`] does that for you.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoolQuery {
    must: Vec<Query>,
    should: Vec<Query>,
    must_not: Vec<Query>,
    filter: Vec<Query>,
    minimum_should_match: Option<Json>,
}

impl BoolQuery {
    /// Required, and contributes to the score.
    pub fn must(mut self, query: impl Into<Query>) -> BoolQuery {
        self.must.push(query.into());
        self
    }

    /// Optional, and raises the score of documents that match — unless
    /// [`BoolQuery::minimum_should_match`] makes it a requirement.
    pub fn should(mut self, query: impl Into<Query>) -> BoolQuery {
        self.should.push(query.into());
        self
    }

    /// Must not match. Does not affect the score.
    pub fn must_not(mut self, query: impl Into<Query>) -> BoolQuery {
        self.must_not.push(query.into());
        self
    }

    /// Required, but contributes nothing to the score and is cached.
    ///
    /// The right slot for a status, an owner, a date window — anything where
    /// the answer is yes or no rather than "how well".
    pub fn filter(mut self, query: impl Into<Query>) -> BoolQuery {
        self.filter.push(query.into());
        self
    }

    /// How many of the `should` clauses have to match: a count, a negative
    /// count, or a percentage like `"50%"`.
    pub fn minimum_should_match(mut self, value: impl Into<Json>) -> BoolQuery {
        self.minimum_should_match = Some(value.into());
        self
    }
}

impl From<BoolQuery> for Query {
    fn from(value: BoolQuery) -> Query {
        let mut body = BTreeMap::new();

        // Empty slots are left out rather than sent as `[]`. The cluster
        // accepts both, but an empty `must_not` in a logged query body reads
        // as a bug somebody has to rule out.
        for (name, clauses) in [
            ("must", value.must),
            ("should", value.should),
            ("must_not", value.must_not),
            ("filter", value.filter),
        ] {
            if !clauses.is_empty() {
                body.insert(
                    name.to_string(),
                    Json::Array(clauses.into_iter().map(Query::into_json).collect()),
                );
            }
        }

        if let Some(minimum) = value.minimum_should_match {
            body.insert("minimum_should_match".to_string(), minimum);
        }

        Query(clause("bool", Json::Object(body)))
    }
}

/// Which way a sort runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Asc,
    Desc,
}

impl Order {
    fn as_str(self) -> &'static str {
        match self {
            Order::Asc => "asc",
            Order::Desc => "desc",
        }
    }
}

/// One aggregation.
///
/// Sub-aggregations nest with [`Aggregation::aggregate`], which is how a
/// "average price per tag" is expressed: a `terms` on the tag with a `stats`
/// inside it.
#[derive(Debug, Clone, PartialEq)]
pub struct Aggregation {
    kind: String,
    options: BTreeMap<String, Json>,
    subs: BTreeMap<String, Aggregation>,
}

impl Aggregation {
    /// A bucket per distinct value.
    ///
    /// Only works on a field that is not analysed — `keyword`, a number, a
    /// date. On a `text` field the cluster answers 400 and tells you to enable
    /// fielddata, which is almost never the right answer; adding a `keyword`
    /// sub-field to the mapping is. See [`crate::Field::text`].
    pub fn terms(field: impl Into<String>) -> Aggregation {
        Aggregation::on("terms", field)
    }

    /// Count, min, max, average and sum of a numeric field, in one pass.
    pub fn stats(field: impl Into<String>) -> Aggregation {
        Aggregation::on("stats", field)
    }

    pub fn min(field: impl Into<String>) -> Aggregation {
        Aggregation::on("min", field)
    }

    pub fn max(field: impl Into<String>) -> Aggregation {
        Aggregation::on("max", field)
    }

    pub fn avg(field: impl Into<String>) -> Aggregation {
        Aggregation::on("avg", field)
    }

    pub fn sum(field: impl Into<String>) -> Aggregation {
        Aggregation::on("sum", field)
    }

    /// How many distinct values — approximate, and deliberately so: an exact
    /// count would mean holding every value in memory on one node.
    pub fn cardinality(field: impl Into<String>) -> Aggregation {
        Aggregation::on("cardinality", field)
    }

    /// How many documents have the field at all.
    pub fn value_count(field: impl Into<String>) -> Aggregation {
        Aggregation::on("value_count", field)
    }

    /// Buckets over time: `date_histogram("created_at", "day")`.
    ///
    /// Sent as `calendar_interval`, so a month is a real month and a day
    /// survives a daylight-saving change. `fixed_interval` — exact multiples of
    /// a duration — is available through [`Aggregation::option`].
    pub fn date_histogram(field: impl Into<String>, interval: impl Into<String>) -> Aggregation {
        Aggregation::on("date_histogram", field)
            .option("calendar_interval", Json::String(interval.into()))
    }

    /// An aggregation this module does not name.
    pub fn raw(kind: impl Into<String>, body: Json) -> Aggregation {
        let options = match body {
            Json::Object(map) => map,
            other => BTreeMap::from([("value".to_string(), other)]),
        };
        Aggregation { kind: kind.into(), options, subs: BTreeMap::new() }
    }

    /// How many buckets to return.
    ///
    /// Applies to the bucket aggregations. The default is 10, which is the
    /// quietest way to lose data in this API: the eleventh tag simply is not
    /// in the answer, and nothing in the response says a number is missing
    /// except [`crate::TermsResult::is_approximate`].
    pub fn size(self, size: u32) -> Aggregation {
        self.option("size", Json::from(size))
    }

    /// Ignore buckets with fewer than this many documents.
    pub fn min_doc_count(self, count: u64) -> Aggregation {
        self.option("min_doc_count", Json::from(count))
    }

    /// Treat documents without the field as having this value, instead of
    /// leaving them out entirely.
    pub fn missing(self, value: impl Into<Json>) -> Aggregation {
        self.option("missing", value.into())
    }

    /// Any other option the aggregation takes.
    pub fn option(mut self, name: impl Into<String>, value: Json) -> Aggregation {
        self.options.insert(name.into(), value);
        self
    }

    /// Nest another aggregation inside each bucket of this one.
    pub fn aggregate(mut self, name: impl Into<String>, sub: impl Into<Aggregation>) -> Aggregation {
        self.subs.insert(name.into(), sub.into());
        self
    }

    /// The JSON this aggregation sends.
    pub fn body(&self) -> Json {
        let mut body = BTreeMap::new();
        body.insert(self.kind.clone(), Json::Object(self.options.clone()));

        if !self.subs.is_empty() {
            let subs =
                self.subs.iter().map(|(name, agg)| (name.clone(), agg.body())).collect();
            body.insert("aggs".to_string(), Json::Object(subs));
        }

        Json::Object(body)
    }

    fn on(kind: &str, field: impl Into<String>) -> Aggregation {
        Aggregation {
            kind: kind.to_string(),
            options: BTreeMap::from([("field".to_string(), Json::String(field.into()))]),
            subs: BTreeMap::new(),
        }
    }
}

/// Which parts of `_source` to return.
#[derive(Debug, Clone, Default, PartialEq)]
enum Source {
    #[default]
    All,
    None,
    Some {
        includes: Vec<String>,
        excludes: Vec<String>,
    },
}

/// A whole search request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Search {
    query: Option<Query>,
    from: Option<u64>,
    size: Option<u64>,
    sort: Vec<Json>,
    source: Source,
    aggregations: BTreeMap<String, Aggregation>,
    track_total_hits: Option<Json>,
    min_score: Option<f64>,
}

impl Search {
    /// Every document, ten at a time — what the cluster does with an empty body.
    pub fn new() -> Search {
        Search::default()
    }

    /// Start from a query, for the common case where nothing else is set.
    pub fn query_only(query: impl Into<Query>) -> Search {
        Search::new().query(query)
    }

    pub fn query(mut self, query: impl Into<Query>) -> Search {
        self.query = Some(query.into());
        self
    }

    /// How many hits to skip.
    ///
    /// `from + size` may not exceed 10,000 without raising
    /// `index.max_result_window`, and paging deep with it gets slower on every
    /// page because every shard has to sort everything up to the offset.
    /// `search_after` is the answer past a few pages; this is right for the
    /// first few.
    pub fn from(mut self, from: u64) -> Search {
        self.from = Some(from);
        self
    }

    /// How many hits to return. Ten if not set.
    ///
    /// Zero is legitimate and useful: an aggregation-only request pays for no
    /// hits at all.
    pub fn size(mut self, size: u64) -> Search {
        self.size = Some(size);
        self
    }

    pub fn sort_by(mut self, field: impl Into<String>, order: Order) -> Search {
        self.sort.push(Json::object([(
            field.into(),
            Json::object([("order", Json::String(order.as_str().into()))]),
        )]));
        self
    }

    pub fn sort_asc(self, field: impl Into<String>) -> Search {
        self.sort_by(field, Order::Asc)
    }

    pub fn sort_desc(self, field: impl Into<String>) -> Search {
        self.sort_by(field, Order::Desc)
    }

    /// Sort by relevance, best first — the default when nothing is sorted.
    ///
    /// Worth naming because adding any other sort silently *replaces* scoring
    /// rather than tie-breaking on it, so a sorted search is no longer ranked
    /// unless `_score` is put back in the list explicitly.
    pub fn sort_by_score(self) -> Search {
        self.sort_by("_score", Order::Desc)
    }

    /// Return only these fields of the document.
    pub fn source<S: Into<String>>(mut self, fields: impl IntoIterator<Item = S>) -> Search {
        let includes: Vec<String> = fields.into_iter().map(Into::into).collect();
        self.source = match self.source {
            Source::Some { excludes, .. } => Source::Some { includes, excludes },
            _ => Source::Some { includes, excludes: Vec::new() },
        };
        self
    }

    /// Return everything except these fields.
    pub fn source_excluding<S: Into<String>>(
        mut self,
        fields: impl IntoIterator<Item = S>,
    ) -> Search {
        let excludes: Vec<String> = fields.into_iter().map(Into::into).collect();
        self.source = match self.source {
            Source::Some { includes, .. } => Source::Some { includes, excludes },
            _ => Source::Some { includes: Vec::new(), excludes },
        };
        self
    }

    /// Return the ids and scores but no document bodies.
    pub fn without_source(mut self) -> Search {
        self.source = Source::None;
        self
    }

    pub fn aggregate(mut self, name: impl Into<String>, aggregation: impl Into<Aggregation>) -> Search {
        self.aggregations.insert(name.into(), aggregation.into());
        self
    }

    /// Count every match rather than stopping at 10,000.
    ///
    /// This is the one setting behind [`crate::TotalHits`] being a relation and
    /// not a number. Since Elasticsearch 7 the total stops being counted once
    /// it passes `index.max_result_window`, and the response then says "at
    /// least 10000" — which a caller that reads only `value` will render as
    /// "10000 results" on a page listing millions. Turning this on makes the
    /// number exact and makes the query slower; that trade is the caller's to
    /// make, so it is never made here.
    pub fn track_total_hits(mut self, exact: bool) -> Search {
        self.track_total_hits = Some(Json::Bool(exact));
        self
    }

    /// Count exactly up to a limit, then stop.
    ///
    /// The middle ground: enough to say "more than 1,000" honestly without
    /// paying to count a million.
    pub fn track_total_hits_up_to(mut self, limit: u64) -> Search {
        self.track_total_hits = Some(Json::from(limit));
        self
    }

    /// Drop hits scoring below this.
    pub fn min_score(mut self, score: f64) -> Search {
        self.min_score = Some(score);
        self
    }

    /// The JSON body this search sends.
    pub fn body(&self) -> Json {
        let mut body = BTreeMap::new();

        body.insert("query".to_string(), self.query_json());

        if let Some(from) = self.from {
            body.insert("from".to_string(), Json::from(from));
        }
        if let Some(size) = self.size {
            body.insert("size".to_string(), Json::from(size));
        }
        if !self.sort.is_empty() {
            body.insert("sort".to_string(), Json::Array(self.sort.clone()));
        }
        match &self.source {
            Source::All => {}
            Source::None => {
                body.insert("_source".to_string(), Json::Bool(false));
            }
            Source::Some { includes, excludes } if excludes.is_empty() => {
                body.insert("_source".to_string(), strings(includes));
            }
            Source::Some { includes, excludes } => {
                body.insert(
                    "_source".to_string(),
                    Json::object([
                        ("includes", strings(includes)),
                        ("excludes", strings(excludes)),
                    ]),
                );
            }
        }
        if !self.aggregations.is_empty() {
            let aggs = self
                .aggregations
                .iter()
                .map(|(name, agg)| (name.clone(), agg.body()))
                .collect();
            body.insert("aggs".to_string(), Json::Object(aggs));
        }
        if let Some(track) = &self.track_total_hits {
            body.insert("track_total_hits".to_string(), track.clone());
        }
        if let Some(score) = self.min_score {
            body.insert("min_score".to_string(), Json::Number(score));
        }

        Json::Object(body)
    }

    /// The body `_count` accepts, which is the query and nothing else.
    ///
    /// `_count` answers 400 for `size`, `from`, `sort` and `aggs`, so sending
    /// the search body would turn a perfectly good search into a failed count.
    pub fn count_body(&self) -> Json {
        Json::object([("query", self.query_json())])
    }

    fn query_json(&self) -> Json {
        self.query.as_ref().map(|q| q.0.clone()).unwrap_or_else(|| clause("match_all", empty()))
    }
}

fn clause(name: &str, body: Json) -> Json {
    Json::object([(name, body)])
}

fn empty() -> Json {
    Json::Object(BTreeMap::new())
}

fn strings(values: &[String]) -> Json {
    Json::Array(values.iter().map(|v| Json::String(v.clone())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_of(search: &Search) -> String {
        search.body().to_string()
    }

    #[test]
    fn an_empty_search_is_match_all_rather_than_an_empty_body() {
        assert_eq!(body_of(&Search::new()), r#"{"query":{"match_all":{}}}"#);
    }

    #[test]
    fn a_match_always_uses_the_expanded_form_so_options_have_somewhere_to_go() {
        let plain: Query = Query::matching("title", "rust").into();
        assert_eq!(plain.json().to_string(), r#"{"match":{"title":{"query":"rust"}}}"#);

        let strict: Query = Query::matching("title", "rust web").and().fuzzy().into();
        assert_eq!(
            strict.json().to_string(),
            r#"{"match":{"title":{"fuzziness":"AUTO","operator":"and","query":"rust web"}}}"#
        );
    }

    #[test]
    fn a_term_query_is_the_expanded_form_too() {
        let query = Query::term("status", "published");
        assert_eq!(query.json().to_string(), r#"{"term":{"status":{"value":"published"}}}"#);
        assert_eq!(
            Query::term("archived", true).json().to_string(),
            r#"{"term":{"archived":{"value":true}}}"#
        );
    }

    #[test]
    fn terms_and_ids_take_a_list() {
        assert_eq!(
            Query::terms("tags", ["rust", "web"]).json().to_string(),
            r#"{"terms":{"tags":["rust","web"]}}"#
        );
        assert_eq!(
            Query::ids(["1", "2"]).json().to_string(),
            r#"{"ids":{"values":["1","2"]}}"#
        );
    }

    #[test]
    fn a_range_collects_its_bounds_under_the_field() {
        let query: Query = Query::range("age").gte(18).lt(65).into();
        assert_eq!(query.json().to_string(), r#"{"range":{"age":{"gte":18,"lt":65}}}"#);
    }

    #[test]
    fn a_bool_leaves_out_the_slots_nobody_filled() {
        let query: Query = Query::bool()
            .must(Query::matching("title", "rust"))
            .filter(Query::term("status", "published"))
            .into();

        let json = query.json().to_string();
        assert!(json.contains(r#""must":[{"match""#), "got {json}");
        assert!(json.contains(r#""filter":[{"term""#), "got {json}");
        assert!(!json.contains("must_not"), "an empty slot reads as a bug in a log: {json}");
        assert!(!json.contains("should"), "got {json}");
    }

    #[test]
    fn any_of_makes_should_a_requirement_because_on_its_own_it_is_not_one() {
        // The trap: `should` beside anything else is only a scoring hint, so a
        // filter built from a bare `should` list matches documents satisfying
        // none of it.
        let bare: Query = Query::bool()
            .must(Query::match_all())
            .should(Query::term("tags", "rust"))
            .into();
        assert!(!bare.json().to_string().contains("minimum_should_match"));

        let required: Query =
            Query::any_of([Query::term("tags", "rust"), Query::term("tags", "web")])
                .must(Query::match_all())
                .into();
        assert!(
            required.json().to_string().contains(r#""minimum_should_match":1"#),
            "got {required:?}"
        );
    }

    #[test]
    fn all_of_puts_everything_in_must() {
        let query: Query =
            Query::all_of([Query::term("a", 1), Query::term("b", 2)]).into();
        let json = query.json().to_string();
        assert!(json.contains(r#""must":[{"term":{"a""#), "got {json}");
        assert!(!json.contains("should"), "got {json}");
    }

    #[test]
    fn a_nested_bool_is_just_a_query_in_a_slot() {
        let query: Query = Query::bool()
            .filter(Query::any_of([Query::term("tags", "rust"), Query::term("tags", "web")]))
            .into();

        assert_eq!(
            query.json().to_string(),
            r#"{"bool":{"filter":[{"bool":{"minimum_should_match":1,"should":[{"term":{"tags":{"value":"rust"}}},{"term":{"tags":{"value":"web"}}}]}}]}}"#
        );
    }

    #[test]
    fn paging_and_sorting_land_beside_the_query_and_not_inside_it() {
        // One level too deep is silently ignored rather than rejected, which is
        // exactly the mistake this builder exists to make impossible.
        let search = Search::new().from(20).size(10).sort_desc("published_at").sort_asc("title");

        assert_eq!(
            body_of(&search),
            r#"{"from":20,"query":{"match_all":{}},"size":10,"sort":[{"published_at":{"order":"desc"}},{"title":{"order":"asc"}}]}"#
        );
    }

    #[test]
    fn source_filtering_uses_the_shape_that_matches_what_was_asked() {
        assert!(body_of(&Search::new().source(["title", "author"]))
            .contains(r#""_source":["title","author"]"#));
        assert!(body_of(&Search::new().without_source()).contains(r#""_source":false"#));
        assert!(
            body_of(&Search::new().source(["a"]).source_excluding(["b"]))
                .contains(r#""_source":{"excludes":["b"],"includes":["a"]}"#)
        );
    }

    #[test]
    fn aggregations_nest_under_aggs_and_can_hold_each_other() {
        let search = Search::new().size(0).aggregate(
            "by_tag",
            Aggregation::terms("tags").size(5).aggregate("price", Aggregation::stats("price")),
        );

        assert_eq!(
            body_of(&search),
            r#"{"aggs":{"by_tag":{"aggs":{"price":{"stats":{"field":"price"}}},"terms":{"field":"tags","size":5}}},"query":{"match_all":{}},"size":0}"#
        );
    }

    #[test]
    fn a_date_histogram_asks_for_a_calendar_interval_rather_than_a_fixed_one() {
        // A fixed 24h day is wrong twice a year in every zone that changes.
        let agg = Aggregation::date_histogram("created_at", "day");
        assert_eq!(
            agg.body().to_string(),
            r#"{"date_histogram":{"calendar_interval":"day","field":"created_at"}}"#
        );
    }

    #[test]
    fn tracking_the_total_is_opt_in_because_it_costs_something() {
        assert!(!body_of(&Search::new()).contains("track_total_hits"));
        assert!(body_of(&Search::new().track_total_hits(true)).contains(r#""track_total_hits":true"#));
        assert!(
            body_of(&Search::new().track_total_hits_up_to(1000))
                .contains(r#""track_total_hits":1000"#)
        );
    }

    #[test]
    fn a_count_body_drops_everything_count_would_reject() {
        let search = Search::new()
            .query(Query::matching("title", "rust"))
            .size(10)
            .from(5)
            .sort_desc("x")
            .aggregate("by_tag", Aggregation::terms("tags"));

        assert_eq!(
            search.count_body().to_string(),
            r#"{"query":{"match":{"title":{"query":"rust"}}}}"#
        );
    }

    #[test]
    fn a_raw_clause_goes_through_untouched() {
        let raw = Json::parse(r#"{"span_first":{"match":{"span_term":{"user":"kimchy"}},"end":3}}"#)
            .unwrap();
        let query: Query = Query::bool().must(Query::raw(raw.clone())).into();

        assert_eq!(query.json().get("bool.must.0"), Some(&raw));
    }
}
