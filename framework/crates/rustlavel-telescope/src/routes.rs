//! The routes Telescope mounts on the application's own router.
//!
//! The dashboard is served by the framework rather than by a separate process
//! or a second port: it can then see the same store the recorder writes to,
//! needs no extra deployment, and disappears entirely when the plugin is not
//! enabled.
//!
//! The page is deliberately thin — one HTML document plus three JSON endpoints
//! — because the interesting part is what the store already knows. The API is
//! useful on its own too: `curl localhost:8000/telescope/api/entries?kind=db.query`
//! is a perfectly good way to look at the last queries from a terminal.

use crate::dashboard::{Page, PageOptions};
use crate::entry::Entry;
use crate::store::{Filter, Store};
use rustlavel_core::Json;
use rustlavel_http::{Request, Response, Router, Status};
use std::sync::Arc;

/// How many entries one listing call may return, whatever `?limit=` asks for.
/// The page polls, so it never needs more, and a bounded response keeps a
/// misplaced `?limit=100000` from serialising the whole buffer.
const MAX_LIMIT: usize = 500;
const DEFAULT_LIMIT: usize = 200;

/// Mount the dashboard and its API under `mount`.
pub fn register(router: &mut Router, store: Store, options: PageOptions) {
    let listing = format!("{}/api/entries", options.mount);
    let detail = format!("{}/api/entries/{{id}}", options.mount);
    let mount = options.mount.clone();

    // The shell is built once at boot; a request only pays for the entries it
    // is about to show.
    let page = Arc::new(Page::new(&options));
    let opening = store.clone();
    router
        .get(&mount, move |_req: Request| {
            let (page, store) = (Arc::clone(&page), opening.clone());
            async move {
                let initial = Filter { limit: Some(DEFAULT_LIMIT), ..Filter::default() };
                Response::html(page.render(&store.to_json(&initial)))
            }
        })
        .name("telescope");

    let entries = store.clone();
    router
        .get(&listing, move |req: Request| {
            let store = entries.clone();
            async move { Response::json(store.to_json(&filter_from(&req))) }
        })
        .name("telescope.entries");

    let one = store.clone();
    router
        .get(&detail, move |req: Request| {
            let store = one.clone();
            async move {
                match req.param_as::<u64>("id").and_then(|id| store.get(id)) {
                    Some(entry) => Response::json(with_related(&store, &entry)),
                    // An entry the buffer has already evicted is genuinely
                    // gone, so this is a 404 rather than an empty object.
                    None => Response::new(Status::NOT_FOUND).with_json(Json::object([(
                        "message",
                        Json::from("no such entry — it may have been evicted from the buffer"),
                    )])),
                }
            }
        })
        .name("telescope.entry");

    let clearing = store.clone();
    router
        .delete(&listing, move |_req: Request| {
            let store = clearing.clone();
            async move {
                let cleared = store.len();
                store.clear();
                Response::json(Json::object([
                    ("ok", Json::from(true)),
                    ("cleared", Json::from(cleared)),
                ]))
            }
        })
        .name("telescope.clear");
}

fn filter_from(request: &Request) -> Filter {
    Filter {
        kind: request.query("kind").filter(|kind| !kind.is_empty()).map(str::to_string),
        after: request.query("after").and_then(|value| value.parse().ok()),
        limit: Some(
            request
                .query("limit")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(DEFAULT_LIMIT)
                .clamp(1, MAX_LIMIT),
        ),
    }
}

/// One entry plus whatever else was recorded during the same request, which is
/// the whole point of the detail view: a slow request next to the queries that
/// made it slow.
fn with_related(store: &Store, entry: &Entry) -> Json {
    let related: Vec<Json> = store.related(entry.id).iter().map(Entry::to_json).collect();
    Json::object([("entry", entry.to_json()), ("related", Json::Array(related))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_core::Event;
    use rustlavel_http::TestClient;
    use std::time::Duration;

    fn options() -> PageOptions {
        PageOptions {
            mount: "/telescope".to_string(),
            app: "Rustlavel".to_string(),
            slow_ms: 100.0,
            capacity: 500,
        }
    }

    /// A store with one request and the query recorded during it.
    fn seeded() -> Store {
        let store = Store::new();
        store.record(
            &Event::new("db.query")
                .with("sql", "select * from users")
                .with("rows", 2)
                .with("ok", true)
                .took(Duration::from_millis(4)),
        );
        store.record(
            &Event::new("http.request")
                .with("method", "GET")
                .with("path", "/users")
                .with("status", 200)
                .took(Duration::from_millis(180)),
        );
        store
    }

    /// Every test here makes a request, and the router dispatches an
    /// `http.request` event for each one — so they all have to take the bus
    /// lock, or they will land in a subscriber another test is asserting on.
    fn client(store: Store) -> TestClient {
        let mut router = Router::new();
        register(&mut router, store, options());
        TestClient::new(router)
    }

    #[tokio::test]
    async fn the_dashboard_route_returns_html_showing_what_was_recorded() {
        let _guard = crate::test_support::exclusive_async().await;
        client(seeded())
            .get("/telescope")
            .await
            .assert_ok()
            .assert_header("content-type", "text/html; charset=utf-8")
            .assert_see("Telescope")
            .assert_see("\"mount\":\"/telescope\"")
            // The first batch is embedded, so the recorded path is in the HTML.
            .assert_see("GET /users")
            .assert_see("select * from users");
    }

    #[tokio::test]
    async fn a_trailing_slash_reaches_the_dashboard_too() {
        let _guard = crate::test_support::exclusive_async().await;
        client(seeded()).get("/telescope/").await.assert_ok().assert_see("Telescope");
    }

    #[tokio::test]
    async fn the_listing_endpoint_returns_entries_newest_first() {
        let _guard = crate::test_support::exclusive_async().await;
        client(seeded())
            .get("/telescope/api/entries")
            .await
            .assert_ok()
            .assert_json("entries.0.kind", "http.request")
            .assert_json("entries.0.summary", "GET /users")
            .assert_json("entries.1.kind", "db.query")
            .assert_json("total", 2)
            .assert_json("capacity", 500);
    }

    #[tokio::test]
    async fn the_listing_endpoint_filters_by_kind() {
        let _guard = crate::test_support::exclusive_async().await;
        let response = client(seeded()).get("/telescope/api/entries?kind=db.query").await.assert_ok();

        assert_eq!(response.json().get("entries").unwrap().as_array().unwrap().len(), 1);
        response.assert_json("entries.0.kind", "db.query");
    }

    #[tokio::test]
    async fn the_listing_endpoint_polls_incrementally_with_after() {
        let _guard = crate::test_support::exclusive_async().await;
        let response = client(seeded()).get("/telescope/api/entries?after=1").await.assert_ok();

        assert_eq!(response.json().get("entries").unwrap().as_array().unwrap().len(), 1);
        response.assert_json("entries.0.id", 2);
    }

    #[tokio::test]
    async fn the_listing_endpoint_honours_a_limit() {
        let _guard = crate::test_support::exclusive_async().await;
        let response = client(seeded()).get("/telescope/api/entries?limit=1").await.assert_ok();

        assert_eq!(response.json().get("entries").unwrap().as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_detail_endpoint_returns_one_entry_with_what_it_caused() {
        let _guard = crate::test_support::exclusive_async().await;
        client(seeded())
            .get("/telescope/api/entries/2")
            .await
            .assert_ok()
            .assert_json("entry.id", 2)
            .assert_json("entry.fields.path", "/users")
            .assert_json("related.0.kind", "db.query");
    }

    #[tokio::test]
    async fn the_detail_endpoint_404s_for_an_unknown_id() {
        let _guard = crate::test_support::exclusive_async().await;
        let client = client(seeded());

        client.get("/telescope/api/entries/9999").await.assert_not_found().assert_see("no such entry");
        // A non-numeric id is a miss too, not a 500.
        client.get("/telescope/api/entries/nonsense").await.assert_not_found();
    }

    #[tokio::test]
    async fn deleting_the_collection_clears_the_buffer() {
        let _guard = crate::test_support::exclusive_async().await;
        let store = seeded();
        let client = client(store.clone());

        client.delete("/telescope/api/entries").await.assert_ok().assert_json("cleared", 2);
        assert!(store.is_empty());
        client.get("/telescope/api/entries").await.assert_json("total", 0);
    }

    #[tokio::test]
    async fn the_kind_counts_the_filter_bar_needs_come_with_the_listing() {
        let _guard = crate::test_support::exclusive_async().await;
        client(seeded())
            .get("/telescope/api/entries")
            .await
            .assert_json("kinds.0.kind", "db.query")
            .assert_json("kinds.0.count", 1)
            .assert_json("kinds.1.kind", "http.request");
    }
}
