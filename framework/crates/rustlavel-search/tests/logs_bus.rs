//! The whole path: an ordinary `info!` call, the process-wide event bus, the
//! shipper, a real cluster.
//!
//! One test in its own binary, because installing a shipper subscribes it for
//! the life of the process — beside other tests it would hear their logging.
//! Skips without `ELASTICSEARCH_URL`, like the other live suites.

use rustlavel_core::Json;
use rustlavel_search::logs::index_for;
use rustlavel_search::{LogShipper, Query, Search, SearchClient};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[tokio::test]
async fn an_ordinary_log_call_reaches_the_cluster_and_a_dead_cluster_does_not_feed_on_itself() {
    let Some(url) = std::env::var("ELASTICSEARCH_URL").ok().filter(|url| !url.is_empty()) else {
        eprintln!("skipping: ELASTICSEARCH_URL is not set");
        return;
    };
    let client = SearchClient::new(url);
    let client = match (std::env::var("ELASTICSEARCH_USER"), std::env::var("ELASTICSEARCH_PASSWORD")) {
        (Ok(user), Ok(password)) if !user.is_empty() => client.basic_auth(&user, &password),
        _ => client,
    };
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let prefix = format!("rustlavel-logs-bus-{nanos}");

    let shipper = LogShipper::new(client.clone())
        .index(prefix.clone())
        .service("bus-test")
        .interval(Duration::from_millis(100))
        .install();

    rustlavel_core::info!("hello from an ordinary log call");
    rustlavel_core::warn!("and a warning with {} in it", "formatting");
    rustlavel_core::debug!("below the default level, so not shipped");

    shipper.flush().await;
    let index = index_for(&prefix, SystemTime::now());
    client.refresh(&index).await.unwrap();

    let found = client
        .search(&index, &Search::new().query(Query::match_all()).track_total_hits(true))
        .await
        .unwrap();
    let messages: Vec<&str> =
        found.hits.iter().filter_map(|hit| hit.source.get("message").and_then(Json::as_str)).collect();
    assert_eq!(found.total.value, 2, "{messages:?}");
    assert!(messages.contains(&"hello from an ordinary log call"));
    assert!(messages.contains(&"and a warning with formatting in it"));
    client.delete_index(&index).await.unwrap();

    // A second shipper pointed at nothing, with the first still subscribed.
    // Its failures are logged by the HTTP client and by itself; if any of that
    // were shipped, the count of lines would keep growing while it failed.
    let dead = LogShipper::new(SearchClient::new("http://127.0.0.1:9").retries(0))
        .index("never".to_string())
        .install();
    rustlavel_core::info!("one line for the dead cluster");
    dead.flush().await;
    let after_first = dead.stats();
    for _ in 0..5 {
        dead.flush().await;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let after_more = dead.stats();

    assert_eq!(after_more.pending, 0, "nothing keeps arriving: {after_more:?}");
    assert_eq!(
        after_more.dropped, after_first.dropped,
        "its own failures were not queued and lost again: {after_first:?} then {after_more:?}"
    );
}
