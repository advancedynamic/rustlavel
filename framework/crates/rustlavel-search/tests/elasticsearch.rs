//! Against a real cluster.
//!
//! The unit tests assert that the *bodies* are right, against fixtures copied
//! from a live server. These assert that a server accepts them and answers the
//! way the fixtures say — which is a different claim, and the only one that
//! catches a wrong URL, a header the cluster rejects, or a response shape that
//! moved between versions.
//!
//! They run only when `ELASTICSEARCH_URL` is set, so `cargo test` stays green
//! on a machine with no cluster.
//!
//! ```text
//! docker run -d --name rustlavel-es -p 9200:9200 \
//!   -e discovery.type=single-node -e xpack.security.enabled=false \
//!   -e ES_JAVA_OPTS="-Xms512m -Xmx512m" \
//!   docker.elastic.co/elasticsearch/elasticsearch:8.15.0
//!
//! # It takes about half a minute to answer. Wait for it:
//! until curl -sf http://localhost:9200 >/dev/null; do sleep 2; done
//!
//! export ELASTICSEARCH_URL=http://localhost:9200
//! cargo test -p rustlavel-search
//!
//! docker rm -f rustlavel-es
//! ```
//!
//! OpenSearch answers all of this identically and is worth pointing the same
//! suite at:
//!
//! ```text
//! docker run -d --name rustlavel-os -p 9200:9200 \
//!   -e discovery.type=single-node -e DISABLE_SECURITY_PLUGIN=true \
//!   -e OPENSEARCH_JAVA_OPTS="-Xms512m -Xmx512m" \
//!   opensearchproject/opensearch:2.11.0
//! ```
//!
//! With security switched on, set `ELASTICSEARCH_USER` and
//! `ELASTICSEARCH_PASSWORD` as well.
//!
//! Every test owns an index named after itself, because the suite runs
//! concurrently and a shared index would make the results depend on the order.

use rustlavel_search::prelude::*;
use rustlavel_search::{IndexedDocument, TotalRelation, WriteResult};
use rustlavel_core::Json;

/// A client for the cluster, or a skip.
macro_rules! cluster {
    () => {
        match std::env::var("ELASTICSEARCH_URL") {
            Ok(url) if !url.is_empty() => {
                let client = SearchClient::new(url);
                match (
                    std::env::var("ELASTICSEARCH_USER"),
                    std::env::var("ELASTICSEARCH_PASSWORD"),
                ) {
                    (Ok(user), Ok(password)) if !user.is_empty() => {
                        client.basic_auth(&user, &password)
                    }
                    _ => client,
                }
            }
            _ => {
                eprintln!("skipping: ELASTICSEARCH_URL is not set");
                return;
            }
        }
    };
}

fn json(body: &str) -> Json {
    Json::parse(body).expect("valid JSON in a test")
}

/// The index one test owns, created fresh.
///
/// `refresh_interval: -1` turns off the automatic refresh, so nothing becomes
/// searchable until the test asks. Without it these tests would race the
/// cluster's one-second timer and fail intermittently — and the timer would be
/// hiding whether the code calls `refresh` at all.
async fn fresh_index(client: &SearchClient, name: &str, definition: IndexDefinition) -> String {
    let index = format!("rustlavel-test-{name}");
    client.delete_index(&index).await.expect("cleaning up a previous run");
    client
        .create_index(&index, &definition.shards(1).replicas(0).refresh_interval("-1"))
        .await
        .expect("creating the index");
    index
}

fn posts_mapping() -> IndexDefinition {
    IndexDefinition::new()
        .field("title", Field::text().with_keyword())
        .field("tags", Field::keyword())
        .field("views", Field::long())
        .field("published_at", Field::date())
        .dynamic_strict()
}

async fn seed(client: &SearchClient, index: &str) {
    let documents = [
        (
            "1",
            r#"{"title":"Rust web frameworks","tags":["rust","web"],"views":120,"published_at":"2024-05-01"}"#,
        ),
        ("2", r#"{"title":"Rust and Tokio","tags":["rust"],"views":45,"published_at":"2024-05-02"}"#),
        ("3", r#"{"title":"Gardening in May","tags":["garden"],"views":7,"published_at":"2024-05-03"}"#),
    ];

    for (id, body) in documents {
        client.index_document(index, id, json(body)).await.expect("indexing");
    }
    client.refresh(index).await.expect("refreshing");
}

#[tokio::test]
async fn the_cluster_says_what_it_is_and_whether_it_can_serve() {
    let client = cluster!();

    let info = client.info().await.unwrap();
    assert!(!info.version.is_empty(), "no version: {info:?}");
    assert!(
        info.distribution == "elasticsearch" || info.distribution == "opensearch",
        "unexpected distribution: {info:?}"
    );

    let health = client.health().await.unwrap();
    assert!(health.status.is_usable(), "the cluster is {}", health.status);
}

#[tokio::test]
async fn an_index_is_created_with_its_mapping_and_deleted_again() {
    let client = cluster!();
    let index = fresh_index(&client, "lifecycle", posts_mapping()).await;

    assert!(client.index_exists(&index).await.unwrap());

    // The mapping is the one that was asked for, sub-field included.
    let mapping = client.get_mapping(&index).await.unwrap();
    let properties = format!("{index}.mappings.properties");
    assert_eq!(
        mapping.get(&format!("{properties}.title.type")).and_then(Json::as_str),
        Some("text")
    );
    assert_eq!(
        mapping.get(&format!("{properties}.title.fields.keyword.type")).and_then(Json::as_str),
        Some("keyword")
    );
    assert_eq!(
        mapping.get(&format!("{index}.mappings.dynamic")).and_then(Json::as_str),
        Some("strict")
    );

    // Adding a field is allowed; the create is not, a second time.
    client
        .put_mapping(&index, &IndexDefinition::new().field("summary", Field::text()))
        .await
        .unwrap();
    assert!(
        !client.create_index_if_missing(&index, &IndexDefinition::new()).await.unwrap(),
        "it was already there"
    );

    assert!(client.delete_index(&index).await.unwrap());
    assert!(!client.index_exists(&index).await.unwrap());
    assert!(!client.delete_index(&index).await.unwrap(), "deleting twice is not an error");
}

/// The gap that makes tests flake and "save then list" show stale data.
///
/// The index has automatic refresh switched off, so this is deterministic
/// rather than a race with the cluster's one-second timer: the document is
/// durable the moment it is written and invisible to search until `refresh`.
#[tokio::test]
async fn a_document_is_not_searchable_until_the_index_is_refreshed() {
    let client = cluster!();
    let index = fresh_index(&client, "near-real-time", posts_mapping()).await;

    client.index_document(&index, "1", json(r#"{"title":"Rust"}"#)).await.unwrap();

    // Readable by id straight away...
    assert!(client.get_document(&index, "1").await.unwrap().is_some());
    // ...and not by search.
    let before = client.search(&index, &Search::new()).await.unwrap();
    assert!(before.is_empty(), "the document was searchable before a refresh: {before:?}");

    client.refresh(&index).await.unwrap();

    let after = client.search(&index, &Search::new()).await.unwrap();
    assert_eq!(after.len(), 1);

    client.delete_index(&index).await.unwrap();
}

#[tokio::test]
async fn a_document_is_written_read_updated_and_deleted() {
    let client = cluster!();
    let index = fresh_index(&client, "documents", posts_mapping()).await;

    let written: IndexedDocument =
        client.index_document(&index, "1", json(r#"{"title":"Rust","views":1}"#)).await.unwrap();
    assert_eq!(written.result, WriteResult::Created);

    let read = client.get_document(&index, "1").await.unwrap().unwrap();
    assert_eq!(read.string("title"), Some("Rust"));
    assert_eq!((read.seq_no, read.primary_term), (written.seq_no, written.primary_term));

    let updated = client.update_document(&index, "1", json(r#"{"views":2}"#)).await.unwrap();
    assert_eq!(updated.result, WriteResult::Updated);
    let read = client.get_document(&index, "1").await.unwrap().unwrap();
    assert_eq!(read.field("views").and_then(Json::as_i64), Some(2));
    assert_eq!(read.string("title"), Some("Rust"), "an update must not drop the other fields");

    // An update that changes nothing is a noop, not a write.
    let again = client.update_document(&index, "1", json(r#"{"views":2}"#)).await.unwrap();
    assert_eq!(again.result, WriteResult::NoOp);

    // A missing document is `None`; a missing index is still an error.
    assert!(client.get_document(&index, "nope").await.unwrap().is_none());
    assert!(client.document_exists(&index, "1").await.unwrap());

    assert!(client.delete_document(&index, "1").await.unwrap());
    assert!(!client.delete_document(&index, "1").await.unwrap());

    // An id that would otherwise change the URL survives the round trip.
    client.index_document(&index, "a/b c", json(r#"{"title":"encoded"}"#)).await.unwrap();
    assert_eq!(
        client.get_document(&index, "a/b c").await.unwrap().unwrap().string("title"),
        Some("encoded")
    );

    client.delete_index(&index).await.unwrap();
}

#[tokio::test]
async fn a_search_returns_the_documents_the_query_asked_for() {
    let client = cluster!();
    let index = fresh_index(&client, "search", posts_mapping()).await;
    seed(&client, &index).await;

    let results = client
        .search(
            &index,
            &Search::new()
                .query(
                    Query::bool()
                        .must(Query::matching("title", "rust").and())
                        .filter(Query::range("views").gte(50)),
                )
                .track_total_hits(true),
        )
        .await
        .unwrap();

    assert_eq!(results.ids(), vec!["1"], "only the Rust post over 50 views");
    assert_eq!(results.total.exact(), Some(1));
    assert!(!results.is_partial());
    assert_eq!(results.hits[0].string("title"), Some("Rust web frameworks"));

    // A term query against the analysed field finds nothing, and against the
    // keyword sub-field finds the document — the reason `with_keyword` exists.
    let analysed = client
        .search(&index, &Search::query_only(Query::term("title", "Rust web frameworks")))
        .await
        .unwrap();
    assert!(analysed.is_empty(), "a term query cannot match an analysed field");

    let exact = client
        .search(&index, &Search::query_only(Query::term("title.keyword", "Rust web frameworks")))
        .await
        .unwrap();
    assert_eq!(exact.ids(), vec!["1"]);

    // Sorting replaces scoring, paging works, and `_source` filtering trims.
    let page = client
        .search(
            &index,
            &Search::new().sort_desc("views").from(1).size(1).source(["title"]),
        )
        .await
        .unwrap();
    assert_eq!(page.ids(), vec!["2"], "the second-most-viewed post");
    assert_eq!(page.max_score, None, "a sorted search scores nothing");
    assert!(page.hits[0].field("views").is_none(), "_source was filtered to the title");

    assert_eq!(client.count(&index, &Search::new()).await.unwrap(), 3);

    client.delete_index(&index).await.unwrap();
}

#[tokio::test]
async fn an_aggregation_buckets_and_summarises() {
    let client = cluster!();
    let index = fresh_index(&client, "aggregations", posts_mapping()).await;
    seed(&client, &index).await;

    let results = client
        .search(
            &index,
            &Search::new()
                .size(0)
                .aggregate(
                    "by_tag",
                    Aggregation::terms("tags").size(10).aggregate("views", Aggregation::stats("views")),
                )
                .aggregate("total_views", Aggregation::sum("views")),
        )
        .await
        .unwrap();

    assert!(results.is_empty(), "size 0 pays for no hits");

    let tags = results.aggregations.terms("by_tag").unwrap();
    assert!(!tags.is_approximate(), "ten buckets is enough for three tags");
    assert_eq!(tags.count("rust"), 2);
    assert_eq!(tags.count("web"), 1);
    assert_eq!(tags.count("garden"), 1);

    let rust_views = tags.get("rust").unwrap().aggregations.stats("views").unwrap();
    assert_eq!(rust_views.count, 2);
    assert_eq!(rust_views.sum, 165.0);
    assert_eq!(rust_views.avg, Some(82.5));

    assert_eq!(results.aggregations.value("total_views"), Some(172.0));

    // A terms aggregation smaller than the data admits what it left out.
    let truncated = client
        .search(
            &index,
            &Search::new().size(0).aggregate("by_tag", Aggregation::terms("tags").size(1)),
        )
        .await
        .unwrap();
    let tags = truncated.aggregations.terms("by_tag").unwrap();
    assert_eq!(tags.buckets.len(), 1);
    assert!(tags.is_approximate(), "two tags did not fit and nothing else says so");

    client.delete_index(&index).await.unwrap();
}

/// The trap, against a real cluster.
///
/// The bulk request answers 200. One document did not fit the mapping and one
/// lost a race with an existing id, and neither the status nor `?` says so.
#[tokio::test]
async fn a_bulk_that_answered_200_lost_two_of_its_four_documents() {
    let client = cluster!();
    let index = fresh_index(&client, "bulk", posts_mapping()).await;

    client.index_document(&index, "3", json(r#"{"title":"already here"}"#)).await.unwrap();

    let operations = [
        BulkOperation::index(json(r#"{"title":"Fine","views":1}"#)).id("1"),
        // A string where the mapping says `long`.
        BulkOperation::index(json(r#"{"title":"Broken","views":"many"}"#)).id("2"),
        // An id that is already taken.
        BulkOperation::create("3", json(r#"{"title":"Duplicate"}"#)),
        BulkOperation::index(json(r#"{"title":"Also fine","views":4}"#)).id("4"),
    ];

    // This is the call a careless caller writes, and it succeeds.
    let report = client.bulk(&index, &operations).await.unwrap();

    assert!(report.errors, "the cluster's own summary");
    assert_eq!(report.succeeded(), 2);
    assert_eq!(report.failed(), 2);
    assert_eq!(report.failed_positions(), vec![1, 2], "in the order they were sent");

    let failures = report.failures();
    assert!(
        matches!(failures[0].error, Some(SearchError::MapperParsing { .. })),
        "got {:?}",
        failures[0].error
    );
    assert!(
        matches!(failures[1].error, Some(SearchError::VersionConflict { .. })),
        "got {:?}",
        failures[1].error
    );
    assert!(
        report.retryable_positions().is_empty(),
        "neither failure gets better by being repeated"
    );

    let error = report.ok_or_error().unwrap_err();
    assert!(matches!(error, SearchError::BulkPartialFailure { failed: 2, total: 4, .. }));

    // And the index really is missing them.
    client.refresh(&index).await.unwrap();
    let remaining = client.search(&index, &Search::new()).await.unwrap();
    let mut ids = remaining.ids();
    ids.sort_unstable();
    assert_eq!(ids, vec!["1", "3", "4"], "document 2 was silently dropped");

    // A delete and an update in the same request work too.
    let report = client
        .bulk(
            &index,
            &[BulkOperation::delete("1"), BulkOperation::update("4", json(r#"{"views":99}"#))],
        )
        .await
        .unwrap();
    report.ok_or_error().unwrap();
    assert_eq!(
        client.get_document(&index, "4").await.unwrap().unwrap().field("views").and_then(Json::as_i64),
        Some(99)
    );

    client.delete_index(&index).await.unwrap();
}

#[tokio::test]
async fn a_concurrent_write_is_refused_rather_than_silently_overwriting() {
    let client = cluster!();
    let index = fresh_index(&client, "conflicts", posts_mapping()).await;

    let written = client.index_document(&index, "1", json(r#"{"views":1}"#)).await.unwrap();

    // Writing at an id that is taken is a conflict rather than an overwrite.
    let error = client
        .create_document(&index, "1", json(r#"{"views":2}"#))
        .await
        .unwrap_err();
    assert!(error.is_conflict(), "got {error:?}");
    assert!(!error.is_retryable(), "resending would overwrite whoever won");

    // Read-modify-write: the second writer holds a stale version and is told so
    // instead of quietly discarding the first writer's change.
    client.index_document(&index, "1", json(r#"{"views":10}"#)).await.unwrap();
    let stale = client
        .index_document_if_unchanged(
            &index,
            "1",
            json(r#"{"views":20}"#),
            written.seq_no,
            written.primary_term,
        )
        .await
        .unwrap_err();
    assert!(stale.is_conflict(), "got {stale:?}");

    // With the current numbers it goes through.
    let current = client.get_document(&index, "1").await.unwrap().unwrap();
    let ok = client
        .index_document_if_unchanged(
            &index,
            "1",
            json(r#"{"views":20}"#),
            current.seq_no,
            current.primary_term,
        )
        .await
        .unwrap();
    assert_eq!(ok.result, WriteResult::Updated);

    client.delete_index(&index).await.unwrap();
}

#[tokio::test]
async fn a_document_that_does_not_fit_the_mapping_is_refused_and_never_retried() {
    let client = cluster!();
    let index = fresh_index(&client, "mapping", posts_mapping()).await;

    let wrong_type = client
        .index_document(&index, "1", json(r#"{"views":"many"}"#))
        .await
        .unwrap_err();
    assert!(matches!(wrong_type, SearchError::MapperParsing { .. }), "got {wrong_type:?}");
    assert!(!wrong_type.is_retryable());
    assert!(
        wrong_type.to_string().contains("For input string"),
        "the cause is the half that says what to fix: {wrong_type}"
    );

    // `dynamic: strict` is what turns a typo into an error instead of a new
    // field nobody will ever query.
    let typo = client
        .index_document(&index, "2", json(r#"{"titel":"typo"}"#))
        .await
        .unwrap_err();
    assert!(matches!(typo, SearchError::MapperParsing { .. }), "got {typo:?}");
    assert!(typo.to_string().contains("titel"), "got {typo}");

    client.delete_index(&index).await.unwrap();
}

#[tokio::test]
async fn searching_an_index_that_does_not_exist_says_so_rather_than_returning_nothing() {
    let client = cluster!();
    // Never created, and named so that no other test could have made it.
    let index = "rustlavel-test-no-such-index-ever";
    let _ = client.delete_index(index).await;

    let error = client.search(index, &Search::new()).await.unwrap_err();

    assert!(error.is_index_not_found(), "got {error:?}");
    assert!(error.to_string().contains(index), "got {error}");
    assert!(!error.is_retryable());

    // The same for reading a document out of it — and that is *not* the same
    // answer as a missing document, which is `None`.
    let error = client.get_document(index, "1").await.unwrap_err();
    assert!(error.is_index_not_found(), "got {error:?}");

    assert!(!client.index_exists(index).await.unwrap());
}

#[tokio::test]
async fn the_total_is_a_floor_until_it_is_asked_to_be_exact() {
    let client = cluster!();
    let index = fresh_index(&client, "totals", posts_mapping()).await;

    // Fewer documents than the cutoff, so the difference is made by
    // `track_total_hits` and nothing else.
    let operations: Vec<BulkOperation> = (0..12)
        .map(|n| BulkOperation::index(json(&format!(r#"{{"views":{n}}}"#))).id(n.to_string()))
        .collect();
    client.bulk(&index, &operations).await.unwrap().ok_or_error().unwrap();
    client.refresh(&index).await.unwrap();

    let capped = client
        .search(&index, &Search::new().size(1).track_total_hits_up_to(5))
        .await
        .unwrap();
    assert_eq!(capped.total.relation, TotalRelation::AtLeast);
    assert_eq!(capped.total.exact(), None, "a floor must not be handed out as a count");
    assert_eq!(capped.total.to_string(), "at least 5");

    let exact = client
        .search(&index, &Search::new().size(1).track_total_hits(true))
        .await
        .unwrap();
    assert_eq!(exact.total.exact(), Some(12));

    client.delete_index(&index).await.unwrap();
}
