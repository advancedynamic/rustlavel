//! rustlavel-storage: files, wherever they live.
//!
//! ```ignore
//! storage.put("avatars/7.png", bytes).await?;
//! let bytes = storage.get("avatars/7.png").await?;
//! ```
//!
//! One trait over local disk and any S3-compatible object store, so a feature
//! written against a developer's `storage/app` directory runs unchanged
//! against S3, R2, or MinIO.

pub mod local;
pub mod s3;
pub mod sigv4;

use rustlavel_core::{Config, Error, Result};

pub use local::LocalStorage;
pub use s3::{S3Config, S3Storage};

/// One stored file's metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    /// Whether this is a prefix (a directory) rather than an object.
    pub is_directory: bool,
}

/// Whether a file is readable without credentials.
///
/// Local disk has no such concept, so it is recorded and ignored there; on S3
/// it becomes the object ACL. Naming it in the API keeps a feature from working
/// locally and silently failing in production.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Private,
    Public,
}

/// The operations every driver supports.
pub trait Storage: Send + Sync {
    fn put(&self, path: &str, contents: Vec<u8>) -> impl Future<Output = Result<()>> + Send;

    fn put_with(
        &self,
        path: &str,
        contents: Vec<u8>,
        visibility: Visibility,
    ) -> impl Future<Output = Result<()>> + Send;

    fn get(&self, path: &str) -> impl Future<Output = Result<Vec<u8>>> + Send;

    fn exists(&self, path: &str) -> impl Future<Output = Result<bool>> + Send;

    fn delete(&self, path: &str) -> impl Future<Output = Result<()>> + Send;

    fn size(&self, path: &str) -> impl Future<Output = Result<u64>> + Send;

    /// Everything directly under a prefix.
    fn list(&self, prefix: &str) -> impl Future<Output = Result<Vec<Entry>>> + Send;

    /// A URL a browser can fetch, when the driver can produce one.
    fn url(&self, path: &str) -> Option<String>;
}

/// Reject a path that would escape the storage root.
///
/// Paths often come from user input — an upload's filename, a key in a
/// request — so this runs before every operation rather than at the edges.
pub fn normalize(path: &str) -> Result<String> {
    let mut segments: Vec<&str> = Vec::new();

    for segment in path.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                if segments.pop().is_none() {
                    return Err(traversal(path));
                }
            }
            s if s.contains('\\') || s.contains('\0') => return Err(traversal(path)),
            s => segments.push(s),
        }
    }

    if segments.is_empty() {
        return Err(Error::msg("a storage path cannot be empty"));
    }
    Ok(segments.join("/"))
}

fn traversal(path: &str) -> Error {
    Error::msg(format!(
        "`{path}` escapes the storage root. Paths are relative and may not contain `..`."
    ))
}

/// Guess a content type from the extension, for the `Content-Type` an object
/// store records and serves back.
pub fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
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
        "pdf" => "application/pdf",
        "csv" => "text/csv; charset=utf-8",
        "txt" | "md" => "text/plain; charset=utf-8",
        "zip" => "application/zip",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}

/// Build the configured driver.
///
/// Returns an enum rather than a boxed trait object: `Storage` uses `async fn`,
/// which is not dyn-safe, and two drivers do not justify the machinery that
/// would make it so.
pub enum AnyStorage {
    Local(LocalStorage),
    S3(Box<S3Storage>),
}

impl AnyStorage {
    pub fn from_config(config: &Config, root: &std::path::Path) -> Result<AnyStorage> {
        match config.string("storage.driver", "local").as_str() {
            "local" => {
                let path = config.string("storage.path", "storage/app");
                Ok(AnyStorage::Local(LocalStorage::new(root.join(path))))
            }
            "s3" => Ok(AnyStorage::S3(Box::new(S3Storage::new(S3Config::from_config(config)?)))),
            other => Err(Error::msg(format!(
                "`{other}` is not a storage driver. Use `local` or `s3`."
            ))),
        }
    }
}

impl Storage for AnyStorage {
    async fn put(&self, path: &str, contents: Vec<u8>) -> Result<()> {
        match self {
            AnyStorage::Local(driver) => driver.put(path, contents).await,
            AnyStorage::S3(driver) => driver.put(path, contents).await,
        }
    }

    async fn put_with(&self, path: &str, contents: Vec<u8>, visibility: Visibility) -> Result<()> {
        match self {
            AnyStorage::Local(driver) => driver.put_with(path, contents, visibility).await,
            AnyStorage::S3(driver) => driver.put_with(path, contents, visibility).await,
        }
    }

    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        match self {
            AnyStorage::Local(driver) => driver.get(path).await,
            AnyStorage::S3(driver) => driver.get(path).await,
        }
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        match self {
            AnyStorage::Local(driver) => driver.exists(path).await,
            AnyStorage::S3(driver) => driver.exists(path).await,
        }
    }

    async fn delete(&self, path: &str) -> Result<()> {
        match self {
            AnyStorage::Local(driver) => driver.delete(path).await,
            AnyStorage::S3(driver) => driver.delete(path).await,
        }
    }

    async fn size(&self, path: &str) -> Result<u64> {
        match self {
            AnyStorage::Local(driver) => driver.size(path).await,
            AnyStorage::S3(driver) => driver.size(path).await,
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        match self {
            AnyStorage::Local(driver) => driver.list(prefix).await,
            AnyStorage::S3(driver) => driver.list(prefix).await,
        }
    }

    fn url(&self, path: &str) -> Option<String> {
        match self {
            AnyStorage::Local(driver) => driver.url(path),
            AnyStorage::S3(driver) => driver.url(path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_ordinary_paths() {
        assert_eq!(normalize("avatars/7.png").unwrap(), "avatars/7.png");
        assert_eq!(normalize("/avatars//7.png").unwrap(), "avatars/7.png");
        assert_eq!(normalize("a/./b").unwrap(), "a/b");
        assert_eq!(normalize("a/b/../c").unwrap(), "a/c");
    }

    #[test]
    fn refuses_to_escape_the_root() {
        for attempt in ["../secrets", "a/../../secrets", "..", "a\\b", "a\0b"] {
            assert!(normalize(attempt).is_err(), "{attempt:?} should be rejected");
        }
    }

    #[test]
    fn an_empty_path_is_an_error() {
        assert!(normalize("").is_err());
        assert!(normalize("/").is_err());
    }

    #[test]
    fn guesses_content_types_from_the_extension() {
        assert_eq!(content_type("a/b/photo.PNG"), "image/png");
        assert_eq!(content_type("report.pdf"), "application/pdf");
        assert_eq!(content_type("noextension"), "application/octet-stream");
    }
}
