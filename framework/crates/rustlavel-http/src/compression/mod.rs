//! Response compression.
//!
//! The codec is written here — DEFLATE (RFC 1951), and the gzip (RFC 1952)
//! and zlib (RFC 1950) framings around it — rather than borrowed, like every
//! other protocol in this framework. The middleware that applies it to
//! responses lives in this file.

pub mod checksum;
pub mod deflate;
pub mod gzip;

use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;

/// Compress responses for clients that ask.
///
/// ```ignore
/// App::new()?.middleware(Compress::default())
/// ```
///
/// JSON is the best case compression has: a list of a hundred users is mostly
/// the same twenty key names repeated a hundred times, and typically shrinks
/// by 70–80%. The cost is CPU on the server, which is why small bodies are
/// left alone — below a kilobyte the headers outweigh the saving — and why a
/// body that is already compressed (an image, a zip, anything with a
/// `Content-Encoding`) is never touched.
///
/// `gzip` is preferred over `deflate` when the client accepts both, because
/// browsers historically disagreed about whether "deflate" meant the zlib
/// format or a raw stream, and gzip never had that problem. When `deflate` is
/// what is asked for, the zlib framing is sent, which is what every modern
/// client means by it.
///
/// A strong `ETag` on the response is weakened, because the compressed bytes
/// are a different representation of the same resource and a strong tag
/// promises byte-for-byte identity. The validator still works; it just says so
/// honestly.
#[derive(Debug, Clone, Copy)]
pub struct Compress {
    /// Bodies smaller than this are sent as they are.
    min_size: usize,
}

impl Default for Compress {
    fn default() -> Self {
        Compress { min_size: 1024 }
    }
}

impl Compress {
    pub fn new() -> Self {
        Self::default()
    }

    /// The size below which a body is not worth compressing.
    pub fn min_size(mut self, bytes: usize) -> Self {
        self.min_size = bytes;
        self
    }
}

/// The encodings this middleware can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Gzip,
    Deflate,
}

impl Encoding {
    fn token(self) -> &'static str {
        match self {
            Encoding::Gzip => "gzip",
            Encoding::Deflate => "deflate",
        }
    }
}

/// Pick an encoding from an `Accept-Encoding` header, or `None` to send plain.
///
/// RFC 9110 §12.5.3: a list of codings with optional `q` weights, where a
/// weight of zero means "not acceptable" and `*` stands for anything not
/// otherwise named. Among what is acceptable, gzip wins ties for the reason
/// given on [`Compress`].
fn negotiate(accept_encoding: &str) -> Option<Encoding> {
    let mut gzip: Option<f32> = None;
    let mut deflate: Option<f32> = None;
    let mut wildcard: Option<f32> = None;

    for part in accept_encoding.split(',') {
        let mut pieces = part.split(';');
        let coding = pieces.next().unwrap_or("").trim().to_ascii_lowercase();
        let weight = pieces
            .find_map(|p| p.trim().strip_prefix("q=").or_else(|| p.trim().strip_prefix("Q=")))
            .and_then(|q| q.trim().parse::<f32>().ok())
            .unwrap_or(1.0);
        match coding.as_str() {
            "gzip" | "x-gzip" => gzip = Some(weight),
            "deflate" => deflate = Some(weight),
            "*" => wildcard = Some(weight),
            _ => {}
        }
    }

    let gzip = gzip.or(wildcard).unwrap_or(0.0);
    let deflate = deflate.or(wildcard).unwrap_or(0.0);
    if gzip <= 0.0 && deflate <= 0.0 {
        None
    } else if gzip >= deflate {
        Some(Encoding::Gzip)
    } else {
        Some(Encoding::Deflate)
    }
}

/// Whether a body of this type shrinks under compression.
///
/// Text does, and so does anything structured as text — JSON, XML, SVG,
/// JavaScript, form data. Images, audio, video and archives were compressed
/// by their own encoders already, and running DEFLATE over them costs CPU to
/// make them very slightly larger.
fn is_compressible(content_type: Option<&str>) -> bool {
    let Some(content_type) = content_type else { return false };
    let mime = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    mime.starts_with("text/")
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
        || matches!(
            mime.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/ecmascript"
                | "application/x-www-form-urlencoded"
                | "application/graphql"
                | "application/ld+json"
                | "application/manifest+json"
                | "application/wasm"
                | "image/svg+xml"
                | "font/ttf"
                | "font/otf"
        )
}

impl Middleware for Compress {
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response> {
        let Some(encoding) = request.header("accept-encoding").and_then(negotiate) else {
            return next.run(request);
        };
        let settings = *self;

        Box::pin(async move {
            let response = next.run(request).await;
            settings.encode(encoding, response)
        })
    }
}

impl Compress {
    fn encode(&self, encoding: Encoding, mut response: Response) -> Response {
        // Nothing to gain, or something that says not to.
        let no_transform = response
            .headers
            .get("cache-control")
            .is_some_and(|cc| cc.split(',').any(|d| d.trim().eq_ignore_ascii_case("no-transform")));
        if response.body.len() < self.min_size
            || response.headers.contains("content-encoding")
            || !(200..300).contains(&response.status.code())
            || no_transform
            || !is_compressible(response.headers.content_type())
        {
            return response;
        }

        let compressed = match encoding {
            Encoding::Gzip => gzip::compress(&response.body),
            Encoding::Deflate => gzip::zlib_compress(&response.body),
        };
        // Incompressible after all — random tokens, say. Sending the larger
        // form would be paying CPU to waste bandwidth.
        if compressed.len() >= response.body.len() {
            return response;
        }

        response.body = compressed;
        response.headers.set("content-encoding", encoding.token());
        // Content-Length is written from the body at serialisation time, so
        // a stale one set by the handler cannot be sent; but remove it anyway
        // so nothing that inspects the response in between is misled.
        response.headers.remove("content-length");

        if let Some(etag) = response.headers.get("etag")
            && !etag.starts_with("W/")
        {
            let weakened = format!("W/{etag}");
            response.headers.set("etag", weakened);
        }

        let vary = response.headers.get("vary").unwrap_or("").to_string();
        if !vary.split(',').any(|v| v.trim().eq_ignore_ascii_case("accept-encoding")) {
            let value = if vary.is_empty() {
                "accept-encoding".to_string()
            } else {
                format!("{vary}, accept-encoding")
            };
            response.headers.set("vary", value);
        }
        response
    }
}

#[cfg(test)]
mod middleware_tests {
    use super::*;
    use crate::method::Method;
    use crate::router::Router;
    use crate::status::Status;
    use crate::testing::TestClient;
    use rustlavel_core::Json;

    fn big_json() -> Json {
        Json::Array(
            (0..200)
                .map(|i| {
                    Json::object([
                        ("id", Json::from(i)),
                        ("name", Json::from(format!("user-{i}"))),
                        ("email", Json::from(format!("user-{i}@example.com"))),
                        ("role", Json::from("member")),
                    ])
                })
                .collect(),
        )
    }

    fn client(compress: Compress) -> TestClient {
        let mut router = Router::new();
        router.middleware(compress);
        router.get("/users", |_req: Request| async { Response::json(big_json()) });
        router.get("/tiny", |_req: Request| async { Response::json(Json::object([("ok", Json::from(true))])) });
        router.get("/image", |_req: Request| async {
            Response::ok().with_header("content-type", "image/png").with_body(vec![0u8; 4096])
        });
        router.get("/already", |_req: Request| async {
            Response::ok()
                .with_header("content-type", "text/plain")
                .with_header("content-encoding", "br")
                .with_body(vec![b'x'; 4096])
        });
        router.get("/no-transform", |_req: Request| async {
            Response::text("y".repeat(4096)).with_header("cache-control", "no-transform")
        });
        router.get("/tagged", |_req: Request| async {
            Response::text("z".repeat(4096)).with_header("etag", "\"abc\"").with_header("vary", "Origin")
        });
        router.get("/random", |_req: Request| async {
            // A pseudo-random body that DEFLATE cannot shrink.
            let mut state = 0x9E37_79B9_7F4A_7C15_u64;
            let body: Vec<u8> = (0..4096)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    (state & 0xFF) as u8
                })
                .collect();
            Response::ok().with_header("content-type", "text/plain").with_body(body)
        });
        router.get("/missing", |_req: Request| async { Response::not_found().with_text("n".repeat(4096)) });
        TestClient::new(router)
    }

    fn get(path: &str, accept: &str) -> Request {
        Request::new(Method::Get, path).with_header("accept-encoding", accept)
    }

    #[tokio::test]
    async fn a_json_body_is_gzipped_and_round_trips() {
        let plain = client(Compress::new()).get("/users").await;
        let response = client(Compress::new()).send(get("/users", "gzip, deflate, br")).await;

        assert_eq!(response.header("content-encoding"), Some("gzip"));
        assert_eq!(response.header("vary"), Some("accept-encoding"));
        let compressed = response.body_bytes();
        assert!(compressed.len() < plain.body().len() / 3, "{} vs {}", compressed.len(), plain.body().len());
        let restored = gzip::decompress(compressed).expect("valid gzip");
        assert_eq!(String::from_utf8(restored).unwrap(), plain.body());
    }

    #[tokio::test]
    async fn deflate_means_the_zlib_format() {
        let response = client(Compress::new()).send(get("/users", "deflate")).await;
        assert_eq!(response.header("content-encoding"), Some("deflate"));
        gzip::zlib_decompress(response.body_bytes()).expect("zlib-framed, as browsers expect");
    }

    #[tokio::test]
    async fn without_accept_encoding_nothing_changes() {
        let response = client(Compress::new()).get("/users").await;
        assert_eq!(response.header("content-encoding"), None);
        assert!(response.body().starts_with('['));
    }

    #[tokio::test]
    async fn small_bodies_are_left_alone() {
        let response = client(Compress::new()).send(get("/tiny", "gzip")).await;
        assert_eq!(response.header("content-encoding"), None);
        assert_eq!(response.body(), "{\"ok\":true}");
    }

    #[tokio::test]
    async fn the_threshold_is_configurable() {
        let response = client(Compress::new().min_size(0)).send(get("/tiny", "gzip")).await;
        // Still not compressed: the compressed form of eleven bytes is larger.
        assert_eq!(response.header("content-encoding"), None);
    }

    #[tokio::test]
    async fn incompressible_types_and_already_encoded_bodies_are_skipped() {
        let client = client(Compress::new());
        assert_eq!(client.send(get("/image", "gzip")).await.header("content-encoding"), None);
        assert_eq!(client.send(get("/already", "gzip")).await.header("content-encoding"), Some("br"));
        assert_eq!(client.send(get("/no-transform", "gzip")).await.header("content-encoding"), None);
    }

    #[tokio::test]
    async fn a_body_that_does_not_shrink_is_sent_as_it_was() {
        let response = client(Compress::new()).send(get("/random", "gzip")).await;
        assert_eq!(response.header("content-encoding"), None);
        assert_eq!(response.body_bytes().len(), 4096);
    }

    #[tokio::test]
    async fn only_successful_responses_are_compressed() {
        let response = client(Compress::new()).send(get("/missing", "gzip")).await;
        let response = response.assert_status(404);
        assert_eq!(response.header("content-encoding"), None);
    }

    #[tokio::test]
    async fn a_strong_etag_becomes_weak_and_vary_is_appended() {
        let response = client(Compress::new()).send(get("/tagged", "gzip")).await;
        assert_eq!(response.header("etag"), Some("W/\"abc\""));
        assert_eq!(response.header("vary"), Some("Origin, accept-encoding"));
    }

    #[tokio::test]
    async fn head_keeps_the_headers_a_get_would_have() {
        let request = Request::new(Method::Head, "/users").with_header("accept-encoding", "gzip");
        let response = client(Compress::new()).send(request).await;
        assert_eq!(response.status(), Status::OK.code());
        assert_eq!(response.header("content-encoding"), Some("gzip"));
    }

    #[test]
    fn negotiation_follows_the_weights() {
        assert_eq!(negotiate("gzip, deflate, br"), Some(Encoding::Gzip));
        assert_eq!(negotiate("deflate"), Some(Encoding::Deflate));
        assert_eq!(negotiate("x-gzip"), Some(Encoding::Gzip));
        assert_eq!(negotiate("deflate;q=1.0, gzip;q=0.5"), Some(Encoding::Deflate));
        assert_eq!(negotiate("gzip;q=0, deflate"), Some(Encoding::Deflate));
        assert_eq!(negotiate("gzip;q=0, deflate;q=0"), None);
        assert_eq!(negotiate("*"), Some(Encoding::Gzip));
        assert_eq!(negotiate("*;q=0, gzip"), Some(Encoding::Gzip));
        assert_eq!(negotiate("br"), None);
        assert_eq!(negotiate("identity"), None);
        assert_eq!(negotiate(""), None);
        assert_eq!(negotiate("GZIP ; Q=0.8"), Some(Encoding::Gzip));
    }

    #[test]
    fn compressibility_is_decided_by_type() {
        assert!(is_compressible(Some("application/json; charset=utf-8")));
        assert!(is_compressible(Some("text/html")));
        assert!(is_compressible(Some("application/problem+json")));
        assert!(is_compressible(Some("image/svg+xml")));
        assert!(!is_compressible(Some("image/png")));
        assert!(!is_compressible(Some("application/zip")));
        assert!(!is_compressible(Some("video/mp4")));
        assert!(!is_compressible(None));
    }
}
