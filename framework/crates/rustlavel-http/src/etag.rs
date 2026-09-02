//! Entity tags and conditional requests.
//!
//! A client that polls `GET /api/orders` every few seconds is, most of the
//! time, downloading a body identical to the one it already has. With this
//! middleware the response carries an `ETag`; the client sends it back as
//! `If-None-Match`; and when nothing has changed the answer is a `304 Not
//! Modified` with no body at all. The handler still runs — this is a bandwidth
//! saving, not a compute saving — but the bytes stay home.
//!
//! ```ignore
//! App::new()?.middleware(ETag::default())
//! ```
//!
//! A handler that sets its own `ETag` or `Last-Modified` is left alone, and
//! its validators are honoured. Both `If-None-Match` and `If-Modified-Since`
//! are evaluated with the precedence RFC 9110 §13.2.2 gives them: when the
//! client sends both, only the tag counts.

use crate::handler::BoxFuture;
use crate::method::Method;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use crate::status::Status;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, Copy, Default)]
pub struct ETag {
    /// Emit `W/"…"` rather than a strong tag.
    ///
    /// A strong tag promises byte-for-byte identity, which is exactly what a
    /// hash of the body provides — so the default is strong. Weak is for a
    /// handler whose output legitimately varies in ways that do not matter
    /// (a timestamp in a comment, say) and wants the cache to keep matching.
    weak: bool,
}

impl ETag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn weak() -> Self {
        ETag { weak: true }
    }
}

/// A validator for a body: hex length, a dash, and a 64-bit hash.
///
/// The length is included so two bodies that happen to collide on the hash
/// still differ unless they are also the same size. SipHash-1-3 from the
/// standard library is not a cryptographic hash and does not need to be — an
/// entity tag only has to change when the body changes, and a client cannot
/// gain anything by forging one.
pub fn etag_for(body: &[u8]) -> String {
    let mut hasher = std::hash::DefaultHasher::new();
    body.hash(&mut hasher);
    format!("\"{:x}-{:016x}\"", body.len(), hasher.finish())
}

/// Whether any tag in an `If-None-Match` list matches ours.
///
/// The weak comparison of RFC 9110 §8.8.3.2: `W/` prefixes are ignored on
/// both sides, because for a GET the question is "may I keep using what I
/// have", and a weakly-equal representation answers yes.
fn none_match(header: &str, etag: &str) -> bool {
    let ours = etag.trim_start_matches("W/");
    header.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.trim_start_matches("W/") == ours
    })
}

fn not_modified(response: Response) -> Response {
    // RFC 9110 §15.4.5: a 304 carries the headers that would have been sent
    // with a 200 and that the cache needs to update its stored response —
    // and nothing that describes a body, because there is none.
    const KEEP: [&str; 7] =
        ["cache-control", "content-location", "date", "etag", "expires", "vary", "last-modified"];

    let mut stripped = Response::new(Status::NOT_MODIFIED);
    for (name, value) in response.headers.iter() {
        if KEEP.contains(&name) {
            stripped.headers.append(name, value);
        }
    }
    stripped
}

impl Middleware for ETag {
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response> {
        // Only a safe method has a representation to validate. A conditional
        // PUT (`If-Match`) is a different mechanism, for lost-update protection,
        // and belongs to the handler that knows the resource.
        if !matches!(request.method(), Method::Get | Method::Head) {
            return next.run(request);
        }

        let if_none_match = request.header("if-none-match").map(str::to_string);
        let if_modified_since =
            request.header("if-modified-since").and_then(crate::date::parse_http_date);
        let weak = self.weak;

        Box::pin(async move {
            let mut response = next.run(request).await;

            // Only a full, successful representation gets a tag. A 404 body is
            // not a version of anything, and a 206 has its own rules.
            if response.status != Status::OK || response.body.is_empty() {
                return response;
            }

            if !response.headers.contains("etag") {
                let tag = etag_for(&response.body);
                response.headers.set("etag", if weak { format!("W/{tag}") } else { tag });
            }

            let etag = response.headers.get("etag").unwrap_or_default().to_string();

            // If-None-Match wins outright when present, even if the date would
            // have said otherwise (RFC 9110 §13.2.2 step 3).
            if let Some(header) = if_none_match {
                return if none_match(&header, &etag) { not_modified(response) } else { response };
            }

            if let (Some(since), Some(modified)) = (
                if_modified_since,
                response.headers.get("last-modified").and_then(crate::date::parse_http_date),
            ) && modified <= since
            {
                return not_modified(response);
            }

            response
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::Router;
    use crate::testing::TestClient;

    fn client(etag: ETag) -> TestClient {
        let mut router = Router::new();
        router.middleware(etag);
        router.get("/report", |_req: Request| async {
            Response::json(rustlavel_core::Json::object([("total", rustlavel_core::Json::from("42"))]))
                .with_header("cache-control", "private, max-age=0")
        });
        router.get("/dated", |_req: Request| async {
            Response::text("dated").with_header("last-modified", "Sun, 06 Nov 1994 08:49:37 GMT")
        });
        router.get("/own-tag", |_req: Request| async {
            Response::text("v7").with_header("etag", "\"version-7\"")
        });
        router.get("/empty", |_req: Request| async { Response::no_content() });
        router.get("/missing", |_req: Request| async { Response::not_found().with_text("no") });
        router.post("/report", |_req: Request| async { Response::text("created") });
        TestClient::new(router)
    }

    #[tokio::test]
    async fn a_successful_get_is_tagged() {
        let response = client(ETag::new()).get("/report").await;
        let tag = response.header("etag").expect("an etag").to_string();
        assert!(tag.starts_with('"') && tag.ends_with('"'), "a strong tag is quoted: {tag}");
        assert_eq!(tag, etag_for(response.body().as_bytes()));
    }

    #[tokio::test]
    async fn the_same_body_gives_the_same_tag_and_a_different_body_a_different_one() {
        assert_eq!(etag_for(b"hello"), etag_for(b"hello"));
        assert_ne!(etag_for(b"hello"), etag_for(b"hello!"));
        assert_ne!(etag_for(b"ab"), etag_for(b"ba"));
    }

    #[tokio::test]
    async fn a_matching_if_none_match_is_answered_with_304_and_no_body() {
        let client = client(ETag::new());
        let first = client.get("/report").await;
        let tag = first.header("etag").unwrap().to_string();

        let request = Request::new(Method::Get, "/report").with_header("if-none-match", &tag);
        let second = client.send(request).await;

        let second = second.assert_status(304);
        assert_eq!(second.body(), "");
        assert_eq!(second.header("etag"), Some(tag.as_str()), "the tag travels with the 304");
        assert_eq!(second.header("cache-control"), Some("private, max-age=0"));
        assert_eq!(second.header("content-type"), None, "a 304 describes no body");
        assert_eq!(second.header("content-length"), None);
    }

    #[tokio::test]
    async fn a_stale_tag_gets_the_full_response() {
        let request = Request::new(Method::Get, "/report").with_header("if-none-match", "\"something-old\"");
        let response = client(ETag::new()).send(request).await;
        let response = response.assert_ok();
        assert!(response.body().contains("42"));
    }

    #[tokio::test]
    async fn a_list_of_tags_and_a_star_both_match() {
        let tag = etag_for(b"x");
        assert!(none_match(&format!("\"other\", {tag}"), &tag));
        assert!(none_match("*", &tag));
        assert!(!none_match("\"other\"", &tag));
    }

    #[tokio::test]
    async fn weak_and_strong_forms_of_the_same_tag_compare_equal() {
        // Weak comparison, as RFC 9110 §8.8.3.2 requires for If-None-Match.
        assert!(none_match("W/\"abc\"", "\"abc\""));
        assert!(none_match("\"abc\"", "W/\"abc\""));
    }

    #[tokio::test]
    async fn the_weak_variant_emits_a_weak_tag() {
        let response = client(ETag::weak()).get("/report").await;
        assert!(response.header("etag").unwrap().starts_with("W/\""));
    }

    #[tokio::test]
    async fn a_handlers_own_tag_is_respected() {
        let client = client(ETag::new());
        assert_eq!(client.get("/own-tag").await.header("etag"), Some("\"version-7\""));

        let request = Request::new(Method::Get, "/own-tag").with_header("if-none-match", "\"version-7\"");
        client.send(request).await.assert_status(304);
    }

    #[tokio::test]
    async fn if_modified_since_is_honoured_against_last_modified() {
        let client = client(ETag::new());

        let later = Request::new(Method::Get, "/dated").with_header("if-modified-since", "Mon, 07 Nov 1994 00:00:00 GMT");
        client.send(later).await.assert_status(304);

        let same = Request::new(Method::Get, "/dated").with_header("if-modified-since", "Sun, 06 Nov 1994 08:49:37 GMT");
        client.send(same).await.assert_status(304);

        let earlier = Request::new(Method::Get, "/dated").with_header("if-modified-since", "Sat, 05 Nov 1994 00:00:00 GMT");
        client.send(earlier).await.assert_ok();
    }

    #[tokio::test]
    async fn an_unreadable_if_modified_since_is_ignored() {
        let request = Request::new(Method::Get, "/dated").with_header("if-modified-since", "last tuesday");
        client(ETag::new()).send(request).await.assert_ok();
    }

    #[tokio::test]
    async fn if_none_match_takes_precedence_over_the_date() {
        // A fresh date says 304; a stale tag says 200. The tag wins.
        let request = Request::new(Method::Get, "/dated")
            .with_header("if-none-match", "\"stale\"")
            .with_header("if-modified-since", "Mon, 07 Nov 1994 00:00:00 GMT");
        client(ETag::new()).send(request).await.assert_ok();
    }

    #[tokio::test]
    async fn only_successful_bodies_are_tagged() {
        let client = client(ETag::new());
        assert_eq!(client.get("/empty").await.header("etag"), None);
        assert_eq!(client.get("/missing").await.header("etag"), None);
    }

    #[tokio::test]
    async fn writes_are_never_tagged_or_short_circuited() {
        let request = Request::new(Method::Post, "/report").with_header("if-none-match", "*");
        let response = client(ETag::new()).send(request).await;
        let response = response.assert_ok();
        assert_eq!(response.header("etag"), None);
        assert_eq!(response.body(), "created");
    }

    #[tokio::test]
    async fn head_is_tagged_like_get() {
        let request = Request::new(Method::Head, "/report");
        let response = client(ETag::new()).send(request).await;
        assert!(response.header("etag").is_some());
    }
}
