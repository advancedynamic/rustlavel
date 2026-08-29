//! Files on local disk.

use crate::{Entry, Storage, Visibility, normalize};
use rustlavel_core::{Error, Result};
use std::path::{Path, PathBuf};

/// Stores files under a root directory.
#[derive(Clone)]
pub struct LocalStorage {
    root: PathBuf,
    /// Prefix for [`Storage::url`], when the root is served over HTTP.
    url_prefix: Option<String>,
}

impl LocalStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        LocalStorage { root: root.into(), url_prefix: None }
    }

    /// Say how this root is reached over HTTP, so `url` can answer.
    pub fn served_at(mut self, prefix: impl Into<String>) -> Self {
        self.url_prefix = Some(prefix.into());
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, path: &str) -> Result<PathBuf> {
        Ok(self.root.join(normalize(path)?))
    }

    fn missing(&self, path: &str) -> Error {
        Error::msg(format!("`{path}` is not in {}", self.root.display()))
    }
}

impl Storage for LocalStorage {
    async fn put(&self, path: &str, contents: Vec<u8>) -> Result<()> {
        self.put_with(path, contents, Visibility::Private).await
    }

    async fn put_with(&self, path: &str, contents: Vec<u8>, _visibility: Visibility) -> Result<()> {
        // Local disk has no visibility of its own; the web server decides what
        // is reachable. Recorded in the signature so a caller writing portable
        // code does not have to branch.
        let target = self.resolve(path)?;

        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(Error::Io)?;
        }
        tokio::fs::write(&target, contents).await.map_err(Error::Io)
    }

    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let target = self.resolve(path)?;
        match tokio::fs::read(&target).await {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(self.missing(path)),
            Err(e) => Err(Error::Io(e)),
        }
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        Ok(tokio::fs::metadata(self.resolve(path)?).await.is_ok())
    }

    async fn delete(&self, path: &str) -> Result<()> {
        match tokio::fs::remove_file(self.resolve(path)?).await {
            Ok(()) => Ok(()),
            // Deleting something that is already gone is the desired state.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    async fn size(&self, path: &str) -> Result<u64> {
        match tokio::fs::metadata(self.resolve(path)?).await {
            Ok(metadata) => Ok(metadata.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(self.missing(path)),
            Err(e) => Err(Error::Io(e)),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        let directory =
            if prefix.trim_matches('/').is_empty() { self.root.clone() } else { self.resolve(prefix)? };

        let mut entries = Vec::new();
        let mut reader = match tokio::fs::read_dir(&directory).await {
            Ok(reader) => reader,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
            Err(e) => return Err(Error::Io(e)),
        };

        while let Some(entry) = reader.next_entry().await.map_err(Error::Io)? {
            let metadata = entry.metadata().await.map_err(Error::Io)?;
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();

            entries.push(Entry {
                path: relative,
                size: metadata.len(),
                is_directory: metadata.is_dir(),
            });
        }

        // Directory order is filesystem-dependent; sorting makes it repeatable.
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    fn url(&self, path: &str) -> Option<String> {
        let prefix = self.url_prefix.as_ref()?;
        let clean = normalize(path).ok()?;
        Some(format!("{}/{clean}", prefix.trim_end_matches('/')))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test gets its own root: tests run concurrently.
    fn storage(name: &str) -> LocalStorage {
        let root = std::env::temp_dir().join(format!("rustlavel-storage-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        LocalStorage::new(root)
    }

    #[tokio::test]
    async fn writes_reads_and_deletes() {
        let storage = storage("roundtrip");

        storage.put("notes/hello.txt", b"hi".to_vec()).await.unwrap();

        assert!(storage.exists("notes/hello.txt").await.unwrap());
        assert_eq!(storage.get("notes/hello.txt").await.unwrap(), b"hi");
        assert_eq!(storage.size("notes/hello.txt").await.unwrap(), 2);

        storage.delete("notes/hello.txt").await.unwrap();
        assert!(!storage.exists("notes/hello.txt").await.unwrap());
    }

    #[tokio::test]
    async fn creates_missing_directories() {
        let storage = storage("nested");

        storage.put("a/b/c/deep.txt", b"x".to_vec()).await.unwrap();
        assert_eq!(storage.get("a/b/c/deep.txt").await.unwrap(), b"x");
    }

    #[tokio::test]
    async fn reading_something_absent_says_where_it_looked() {
        let storage = storage("missing");

        let error = storage.get("nope.txt").await.unwrap_err().to_string();
        assert!(error.contains("nope.txt"));
        assert!(error.contains("rustlavel-storage-missing"));
    }

    #[tokio::test]
    async fn deleting_something_absent_is_not_an_error() {
        assert!(storage("idempotent").delete("nope.txt").await.is_ok());
    }

    #[tokio::test]
    async fn refuses_to_write_outside_the_root() {
        let storage = storage("traversal");

        assert!(storage.put("../escaped.txt", b"x".to_vec()).await.is_err());
        assert!(storage.get("../../etc/passwd").await.is_err());
    }

    #[tokio::test]
    async fn lists_a_directory_in_a_stable_order() {
        let storage = storage("listing");
        storage.put("docs/b.txt", b"b".to_vec()).await.unwrap();
        storage.put("docs/a.txt", b"aa".to_vec()).await.unwrap();

        let entries = storage.list("docs").await.unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "docs/a.txt");
        assert_eq!(entries[0].size, 2);
        assert!(!entries[0].is_directory);
    }

    #[tokio::test]
    async fn listing_a_missing_prefix_is_empty_rather_than_an_error() {
        assert!(storage("empty-listing").list("nowhere").await.unwrap().is_empty());
    }

    #[test]
    fn urls_are_only_produced_when_the_root_is_served() {
        let storage = storage("urls");
        assert_eq!(storage.url("a/b.png"), None);

        let served = storage.served_at("https://cdn.example.com/files/");
        assert_eq!(served.url("a/b.png").as_deref(), Some("https://cdn.example.com/files/a/b.png"));
    }
}
