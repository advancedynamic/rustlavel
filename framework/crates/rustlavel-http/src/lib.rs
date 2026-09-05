//! rustlavel-http: the HTTP layer, written from scratch on Tokio's TCP.
//!
//! Parsing, routing, the middleware pipeline, request and response types, the
//! development error page, and a test client that dispatches without a socket.

pub mod body_limit;
pub mod compression;
pub mod cookie;
pub mod cors;
pub mod date;
pub mod error_page;
pub mod etag;
pub mod files;
pub mod flash;
pub mod handler;
pub mod health;
pub mod headers;
pub mod json_resource;
pub mod method;
pub mod middleware;
pub mod panic;
pub mod plugin;
pub mod request;
pub mod request_id;
pub mod response;
pub mod router;
pub mod server;
pub mod status;
pub mod testing;
pub mod timeout;
pub mod trusted_proxies;
pub mod upgrade;
pub mod url;
pub mod versioning;

pub use body_limit::BodyLimit;
pub use compression::Compress;
pub use cookie::{Cookie, SameSite};
pub use cors::Cors;
pub use etag::ETag;
pub use files::Files;
pub use flash::Flash;
// `BoxFuture` is the return type of `Middleware::handle`, so it has to be
// nameable outside this crate — a middleware written in an application cannot
// spell its own signature otherwise.
pub use handler::{BoxFuture, Handler};
pub use health::Health;
pub use headers::Headers;
pub use json_resource::{Attributes, JsonResource, ResourceResponse, attributes};
pub use method::Method;
pub use middleware::{Middleware, Next};
pub use plugin::{Plugin, Setup};
pub use request::Request;
pub use request_id::RequestId;
pub use response::{IntoResponse, Response};
pub use router::{NamedRoutes, Resource, Route, RouteHandle, Router};
pub use server::{Limits, Server};
pub use status::Status;
pub use testing::{TestClient, TestResponse};
pub use timeout::Timeout;
pub use trusted_proxies::{Forwarded, TrustProxies};
pub use upgrade::{Upgrade, Upgraded};
pub use versioning::{ApiVersion, VersionHeader};
