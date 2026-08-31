//! rustlavel-search: Elasticsearch and OpenSearch.
//!
//! **One package serves both.** OpenSearch was forked from Elasticsearch 7.10
//! and the two have not diverged on anything this crate touches: `_search`,
//! `_bulk`, `_doc`, `_mapping`, `_refresh` and `_cluster/health` take the same
//! bodies and answer with the same shapes on either. Nothing here prefers one.
//! [`ClusterInfo::distribution`] says which one answered, for the rare caller
//! that has to branch on it.
//!
//! There is no protocol to write here — the cluster speaks HTTP and JSON, and
//! the framework already has both. What this crate is actually for is the
//! things that go wrong:
//!
//! - **`_bulk` answers 200 when documents were lost.** Per-item failures live
//!   in the body, and a caller that checks the status writes a loader that
//!   silently drops rows. See [`BulkReport`].
//! - **The total on a search is a floor, not a count.** Counting stops at
//!   10,000 by default. See [`TotalHits`].
//! - **A search can succeed on some shards and fail on others**, answering 200
//!   with fewer results and no other sign. See [`SearchResults::is_partial`].
//! - **A document is not searchable the moment it is written.** See
//!   [`SearchClient::refresh`].
//! - **Failures need telling apart.** A rejected write is worth repeating; a
//!   document that does not fit the mapping never is. See [`SearchError`].
//!
//! ```ignore
//! let client = SearchClient::new("http://localhost:9200").basic_auth("elastic", &password);
//!
//! client
//!     .create_index_if_missing(
//!         "posts",
//!         &IndexDefinition::new()
//!             .field("title", Field::text().with_keyword())
//!             .field("tags", Field::keyword())
//!             .field("views", Field::long())
//!             .dynamic_strict(),
//!     )
//!     .await?;
//!
//! client.bulk("posts", &operations).await?.ok_or_error()?;
//! client.refresh("posts").await?;
//!
//! let results = client
//!     .search(
//!         "posts",
//!         &Search::new()
//!             .query(
//!                 Query::bool()
//!                     .must(Query::matching("title", "rust").and())
//!                     .filter(Query::range("views").gte(100)),
//!             )
//!             .aggregate("by_tag", Aggregation::terms("tags").size(10))
//!             .track_total_hits(true),
//!     )
//!     .await?;
//!
//! println!("{} matches", results.total);
//! ```
//!
//! Tests need no cluster: [`SearchClient::faking`] is `Http::fake()`, and the
//! integration tests under `tests/` run against a real server only when
//! `ELASTICSEARCH_URL` is set.

pub mod client;
pub mod document;
pub mod error;
pub mod index;
pub mod query;
pub mod response;

pub use client::{ClusterHealth, ClusterInfo, HealthStatus, SearchClient};
pub use document::{BulkItem, BulkOperation, BulkReport, Document, IndexedDocument, WriteResult};
pub use error::{Result, SearchError};
pub use index::{Field, IndexDefinition};
pub use query::{Aggregation, BoolQuery, Match, Order, Query, Range, Search};
pub use response::{
    Aggregations, Bucket, Hit, SearchResults, Shards, StatsResult, TermsResult, TotalHits,
    TotalRelation,
};

/// Everything an application normally needs, in one import.
pub mod prelude {
    pub use crate::client::SearchClient;
    pub use crate::document::{BulkOperation, BulkReport, Document};
    pub use crate::error::SearchError;
    pub use crate::index::{Field, IndexDefinition};
    pub use crate::query::{Aggregation, Order, Query, Search};
    pub use crate::response::{Hit, SearchResults, TotalHits};
}
