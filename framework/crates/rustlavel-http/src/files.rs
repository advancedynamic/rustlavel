//! Serving static files out of a directory, for `public/`.

use crate::handler::{BoxFuture, Handler};
use crate::request::Request;
use crate::response::Response;
use crate::status::Status;
use crate::url;
use std::path::{Path, PathBuf};

/// Serves files from a directory, refusing anything that escapes it.
pub struct Files {
    root: PathBuf,
    /// Served when the request maps to a directory.
    index: Option<String>,
}

impl Files {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Files { root: root.into(), index: Some("index.html".to_string()) }
    }

    pub fn without_index(mut self) -> Self {
        self.index = None;
        self
    }

    fn resolve(&self, path: &str) -> Option<PathBuf> {
        let normalized = url::normalize_path(&url::decode(path))?;
        let mut candidate = self.root.join(normalized.trim_start_matches('/'));

        if candidate.is_dir() {
            candidate = candidate.join(self.index.as_deref()?);
        }

        // Resolving symlinks last is what actually proves the file is inside
        // the root — normalization alone cannot see through a link.
        let real_root = self.root.canonicalize().ok()?;
        let real = candidate.canonicalize().ok()?;
        real.starts_with(&real_root).then_some(real)
    }
}

impl Handler for Files {
    fn call(&self, request: Request) -> BoxFuture<Response> {
        let resolved = self.resolve(request.path());
        Box::pin(async move {
            let Some(path) = resolved else {
                return Response::not_found();
            };
            match tokio::fs::read(&path).await {
                Ok(bytes) => Response::ok()
                    .with_header("content-type", content_type(&path))
                    .with_header("cache-control", "public, max-age=3600")
                    .with_body(bytes),
                Err(_) => Response::new(Status::NOT_FOUND).with_text("Not Found"),
            }
        })
    }
}

/// Guess a content type from the file extension.
pub fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or_default() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        "xml" => "application/xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;

    /// Each test gets its own directory: tests run concurrently, and a shared
    /// fixture would be re-written underneath a test that is reading it.
    fn fixture_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustlavel-files-{name}"));
        std::fs::create_dir_all(dir.join("css")).unwrap();
        std::fs::write(dir.join("index.html"), "<h1>home</h1>").unwrap();
        std::fs::write(dir.join("css/app.css"), "body{}").unwrap();
        dir
    }

    #[tokio::test]
    async fn serves_a_file_with_its_content_type() {
        let files = Files::new(fixture_dir("content-type"));
        let response = files.call(Request::new(Method::Get, "/css/app.css")).await;

        assert_eq!(response.status, Status::OK);
        assert_eq!(response.body_string(), "body{}");
        assert_eq!(response.headers.content_type(), Some("text/css"));
    }

    #[tokio::test]
    async fn serves_the_index_for_a_directory() {
        let files = Files::new(fixture_dir("index"));
        let response = files.call(Request::new(Method::Get, "/")).await;

        assert_eq!(response.body_string(), "<h1>home</h1>");
    }

    #[tokio::test]
    async fn refuses_to_escape_the_root() {
        let files = Files::new(fixture_dir("traversal"));

        for attempt in ["/../../../etc/passwd", "/css/../../etc/passwd", "/%2e%2e/%2e%2e/etc/passwd"] {
            let response = files.call(Request::new(Method::Get, attempt)).await;
            assert_eq!(response.status, Status::NOT_FOUND, "{attempt} should not resolve");
        }
    }
}
