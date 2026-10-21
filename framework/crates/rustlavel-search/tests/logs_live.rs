//! The log shipper against a real Elasticsearch or OpenSearch.
//!
//! The unit tests use a fake, which proves the request has the right shape and
//! nothing about whether a cluster accepts it. These run only when
//! `ELASTICSEARCH_URL` is set, like `elasticsearch.rs` beside them, and skip
//! with a printed line when it is not.
//!
//! Each test ships to an index prefix of its own and records events straight
//! into its own shipper instead of subscribing to the process-wide bus, because
//! the suite runs concurrently and a global subscriber would hear every test.

use rustlavel_core::events::Event;
use rustlavel_core::Json;
use rustlavel_search::logs::index_for;
use rustlavel_search::{LogShipper, Query, Search, SearchClient};
use std::time::{SystemTime, UNIX_EPOCH};

/// A client for the cluster, or a skip. Reads the same variables as
/// `elasticsearch.rs`, so the nightly job — which leaves security on — works.
fn cluster() -> Option<SearchClient> {
    let url = std::env::var("ELASTICSEARCH_URL").ok().filter(|url| !url.is_empty())?;
    let client = SearchClient::new(url);
    Some(match (std::env::var("ELASTICSEARCH_USER"), std::env::var("ELASTICSEARCH_PASSWORD")) {
        (Ok(user), Ok(password)) if !user.is_empty() => client.basic_auth(&user, &password),
        _ => client,
    })
}

macro_rules! cluster {
    () => {
        match cluster() {
            Some(client) => client,
            None => {
                eprintln!("skipping: ELASTICSEARCH_URL is not set");
                return;
            }
        }
    };
}

/// A prefix no other test or earlier run shares.
fn prefix(test: &str) -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("rustlavel-logs-{test}-{nanos}")
}

fn line(level: &str, message: &str) -> Event {
    Event::new("log").with("level", level).with("message", message)
}

#[tokio::test]
async fn shipped_lines_are_searchable_with_a_date_timestamp_and_the_ecs_fields() {
    let client = cluster!();
    let prefix = prefix("shipped");
    let shipper = LogShipper::new(client.clone()).index(prefix.clone()).service("live-test");

    shipper.record(&line("info", "first"));
    shipper.record(&line("error", "second"));
    shipper.record(&Event::new("log").with("level", "warn").with("message", "third").with("user_id", 7));
    shipper.flush().await;

    let stats = shipper.stats();
    assert_eq!((stats.shipped, stats.failed, stats.dropped), (3, 0, 0), "{stats:?}");

    let index = index_for(&prefix, SystemTime::now());
    client.refresh(&index).await.unwrap();

    let found = client
        .search(&index, &Search::new().query(Query::match_all()).track_total_hits(true))
        .await
        .unwrap();
    assert_eq!(found.total.value, 3);

    let errors = client
        .search(&index, &Search::new().query(Query::term("log.level", "error")))
        .await
        .unwrap();
    assert_eq!(errors.hits.len(), 1);
    assert_eq!(errors.hits[0].source.get("message").and_then(Json::as_str), Some("second"));
    assert_eq!(
        errors.hits[0].source.get("service").and_then(|s| s.get("name")).and_then(Json::as_str),
        Some("live-test")
    );

    // The cluster read `@timestamp` as a date, which is what makes time-range
    // queries and Kibana's time picker work without anybody writing a mapping.
    let mapping = client.get_mapping(&index).await.unwrap();
    // `Json::get` reads a dotted path and a daily index name has dots in it, so
    // the index is looked up as a key rather than as a path.
    let kind = mapping
        .as_object()
        .and_then(|indices| indices.get(&index))
        .and_then(|m| m.get("mappings"))
        .and_then(|m| m.get("properties"))
        .and_then(|p| p.get("@timestamp"))
        .and_then(|t| t.get("type"))
        .and_then(Json::as_str);
    assert_eq!(kind, Some("date"));

    client.delete_index(&index).await.unwrap();
}

#[tokio::test]
async fn a_line_that_does_not_fit_the_mapping_is_counted_and_the_rest_still_land() {
    let client = cluster!();
    let prefix = prefix("conflict");
    let index = index_for(&prefix, SystemTime::now());

    // `labels.n` becomes a number in the first document, so a string in the
    // second cannot be indexed.
    let shipper = LogShipper::new(client.clone()).index(prefix.clone());
    shipper.record(&Event::new("log").with("level", "info").with("message", "a").with("n", 1));
    shipper.flush().await;
    shipper.record(&Event::new("log").with("level", "info").with("message", "b").with("n", "many"));
    shipper.record(&line("info", "c"));
    shipper.flush().await;

    let stats = shipper.stats();
    assert_eq!((stats.shipped, stats.failed), (2, 1), "{stats:?}: one mapping conflict, two fine");

    client.refresh(&index).await.unwrap();
    let found = client
        .search(&index, &Search::new().query(Query::match_all()).track_total_hits(true))
        .await
        .unwrap();
    assert_eq!(found.total.value, 2);

    client.delete_index(&index).await.unwrap();
}

#[tokio::test]
async fn an_unreachable_cluster_loses_the_lines_and_does_not_hang() {
    // Nothing listens here. This one needs no cluster, so it is not skipped —
    // but it is a test of the failure path against a real socket, which a fake
    // cannot be.
    let client = SearchClient::new("http://127.0.0.1:9").retries(0);
    let shipper = LogShipper::new(client);
    shipper.record(&line("info", "nobody is listening"));

    let started = std::time::Instant::now();
    shipper.flush().await;

    let stats = shipper.stats();
    assert_eq!(stats.shipped, 0);
    assert_eq!(stats.pending + stats.dropped as usize, 1, "{stats:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(60), "a dead cluster must not hang a flush");
}
