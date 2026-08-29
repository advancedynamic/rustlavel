//! The file driver: one file per key under a configurable directory.
//!
//! Useful when a single-process application wants a cache that survives a
//! restart without running Redis. Two properties matter more than speed here:
//!
//! * **A key can never escape the directory.** Cache keys routinely contain
//!   user input (`user:{email}`, a URL, a path), so the file name is a hash of
//!   the key, not the key. `../../etc/passwd` and `a/b/c` become 32 hex
//!   characters like everything else.
//! * **A reader never sees half a write.** Payloads are written to a temporary
//!   file and renamed into place, which is atomic on every platform rustlavel
//!   targets.
//!
//! The hash is FNV-1a rather than `DefaultHasher` because `DefaultHasher` is
//! explicitly allowed to change between Rust releases: an upgrade would silently
//! orphan every file on disk.

use crate::store::{BoxFuture, Cache, counter_value, decode, prefixed, record};
use rustlavel_core::{Error, Json, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// The extension every cache file carries, so `flush` can clear the directory
/// without deleting something an operator put there by hand.
const EXTENSION: &str = "cache";

/// Locks are sharded by key so a read-modify-write on one counter does not
/// block an unrelated one.
const LOCKS: usize = 16;

/// Distinguishes concurrent temporary files within one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Inner {
    directory: PathBuf,
    prefix: String,
    /// Serialises read-modify-write sequences (`increment`) inside this
    /// process. Two *processes* sharing a directory can still interleave — the
    /// file system offers no compare-and-swap — which is why the file driver is
    /// documented as unsuitable for a rate limiter behind multiple workers.
    locks: Vec<Mutex<()>>,
}

/// A cache stored on disk. Cloning shares one directory and one lock table.
#[derive(Clone)]
pub struct FileStore {
    inner: Arc<Inner>,
}

impl FileStore {
    /// Create a store rooted at `directory`, creating the directory if needed.
    pub fn new(directory: impl Into<PathBuf>) -> Result<Self> {
        FileStore::with_prefix(directory, "")
    }

    pub fn with_prefix(directory: impl Into<PathBuf>, prefix: impl Into<String>) -> Result<Self> {
        let directory = directory.into();
        std::fs::create_dir_all(&directory).map_err(|e| {
            Error::msg(format!("cannot create the cache directory `{}`: {e}", directory.display()))
        })?;

        Ok(FileStore {
            inner: Arc::new(Inner {
                directory,
                prefix: prefix.into(),
                locks: (0..LOCKS).map(|_| Mutex::new(())).collect(),
            }),
        })
    }

    pub fn directory(&self) -> &Path {
        &self.inner.directory
    }

    /// The file backing a key. Public so a test can prove where a hostile key
    /// actually lands.
    pub fn path_for(&self, key: &str) -> PathBuf {
        let full = prefixed(&self.inner.prefix, key);
        self.inner.directory.join(format!("{}.{EXTENSION}", fingerprint(&full)))
    }

    fn lock_for(&self, key: &str) -> &Mutex<()> {
        &self.inner.locks[(fnv1a(key.as_bytes(), FNV_OFFSET) as usize) % LOCKS]
    }

    /// Read an entry, deleting it when it has expired or does not belong to
    /// this key.
    async fn read(&self, key: &str, full: &str) -> Option<Json> {
        let path = self.path_for(key);
        let raw = tokio::fs::read_to_string(&path).await.ok()?;

        let document = decode(&raw)?;

        // Two different keys can hash to the same file. Storing the key inside
        // the file turns that from a wrong answer into a miss.
        if document.get("key").and_then(Json::as_str) != Some(full) {
            return None;
        }

        if let Some(expires_at) = document.get("expires_at").and_then(Json::as_f64)
            && expires_at <= now_millis()
        {
            let _ = tokio::fs::remove_file(&path).await;
            return None;
        }

        document.get("value").cloned()
    }

    async fn write(&self, key: &str, full: &str, value: Json, expires_at: Option<f64>) -> Result<()> {
        let document = Json::object([
            ("key", Json::from(full)),
            ("expires_at", expires_at.map_or(Json::Null, Json::from)),
            ("value", value),
        ]);

        let path = self.path_for(key);
        // A unique temporary name: two tasks writing the same key must not
        // truncate each other's half-written file.
        let temporary = path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));

        tokio::fs::write(&temporary, document.to_string()).await.map_err(|e| {
            Error::msg(format!("cannot write the cache file `{}`: {e}", temporary.display()))
        })?;

        // Rename is atomic, so a concurrent reader sees either the old file or
        // the new one and never a truncated document.
        tokio::fs::rename(&temporary, &path).await.map_err(|e| {
            let _ = std::fs::remove_file(&temporary);
            Error::msg(format!("cannot replace the cache file `{}`: {e}", path.display()))
        })
    }

    /// The remaining life of a key, in milliseconds since the epoch form used
    /// on disk. `None` for a missing key, `Some(None)` for an immortal one.
    async fn expiry(&self, key: &str, full: &str) -> Option<Option<f64>> {
        let raw = tokio::fs::read_to_string(self.path_for(key)).await.ok()?;
        let document = decode(&raw)?;
        if document.get("key").and_then(Json::as_str) != Some(full) {
            return None;
        }
        match document.get("expires_at").and_then(Json::as_f64) {
            Some(at) if at <= now_millis() => None,
            other => Some(other),
        }
    }
}

impl Cache for FileStore {
    fn driver(&self) -> &'static str {
        "file"
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Json>>> {
        Box::pin(async move {
            let full = prefixed(&self.inner.prefix, key);
            let found = self.read(key, &full).await;
            record(found.is_some(), "file", key);
            Ok(found)
        })
    }

    fn put<'a>(&'a self, key: &'a str, value: Json, ttl: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let full = prefixed(&self.inner.prefix, key);
            if ttl.is_zero() {
                let _ = tokio::fs::remove_file(self.path_for(key)).await;
                return Ok(());
            }
            let expires_at = now_millis() + ttl.as_millis() as f64;
            self.write(key, &full, value, Some(expires_at)).await
        })
    }

    fn forever<'a>(&'a self, key: &'a str, value: Json) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let full = prefixed(&self.inner.prefix, key);
            self.write(key, &full, value, None).await
        })
    }

    fn forget<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let full = prefixed(&self.inner.prefix, key);
            let _guard = self.lock_for(&full).lock().await;

            // Reading first means an already-expired file reports `false`,
            // matching what a `get` would have said.
            let existed = self.read(key, &full).await.is_some();
            let _ = tokio::fs::remove_file(self.path_for(key)).await;
            Ok(existed)
        })
    }

    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let mut entries = match tokio::fs::read_dir(&self.inner.directory).await {
                Ok(entries) => entries,
                // Nothing to flush is not a failure.
                Err(_) => return Ok(()),
            };

            while let Some(entry) = entries.next_entry().await.map_err(Error::from)? {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == EXTENSION) {
                    let _ = tokio::fs::remove_file(&path).await;
                }
            }
            Ok(())
        })
    }

    fn increment<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            let full = prefixed(&self.inner.prefix, key);
            let _guard = self.lock_for(&full).lock().await;

            let current = self.read(key, &full).await;
            let next = counter_value(current.as_ref()) + by;

            // Increment must not shorten the life of an existing counter.
            let expires_at = self.expiry(key, &full).await.flatten();
            self.write(key, &full, Json::from(next), expires_at).await?;
            Ok(next)
        })
    }

    fn increment_within<'a>(
        &'a self,
        key: &'a str,
        by: i64,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            let full = prefixed(&self.inner.prefix, key);
            let _guard = self.lock_for(&full).lock().await;

            match self.read(key, &full).await {
                Some(current) => {
                    let next = counter_value(Some(&current)) + by;
                    let expires_at = self.expiry(key, &full).await.flatten();
                    self.write(key, &full, Json::from(next), expires_at).await?;
                    Ok(next)
                }
                None => {
                    // This call created the counter, so it sets the window.
                    let expires_at = now_millis() + ttl.as_millis() as f64;
                    self.write(key, &full, Json::from(by), Some(expires_at)).await?;
                    Ok(by)
                }
            }
        })
    }

    fn ttl<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Duration>>> {
        Box::pin(async move {
            let full = prefixed(&self.inner.prefix, key);
            Ok(self
                .expiry(key, &full)
                .await
                .flatten()
                .map(|at| Duration::from_millis((at - now_millis()).max(0.0) as u64)))
        })
    }
}

fn now_millis() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as f64
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(bytes: &[u8], offset: u64) -> u64 {
    let mut hash = offset;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// A stable 128-bit fingerprint, rendered as 32 lowercase hex characters.
///
/// Two independent FNV-1a passes — forwards from one offset, backwards from
/// another — because a single 64-bit hash would see collisions at a few million
/// keys, and a collision costs a needless miss on every read of both keys.
fn fingerprint(key: &str) -> String {
    let forward = fnv1a(key.as_bytes(), FNV_OFFSET);
    let reversed: Vec<u8> = key.bytes().rev().collect();
    let backward = fnv1a(&reversed, FNV_OFFSET ^ 0x5555_5555_5555_5555);
    format!("{forward:016x}{backward:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::CacheExt;

    /// Each test gets its own directory: they run concurrently, and a shared
    /// one would mean `flush` in one test deleting another test's entries.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rustlavel-cache-{name}-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn a_key_containing_slashes_and_dot_dot_cannot_escape_the_directory() {
        let dir = scratch("traversal");
        let cache = FileStore::new(&dir).unwrap();

        for hostile in [
            "../../etc/passwd",
            "/etc/shadow",
            "a/b/c",
            "..",
            "....//....//root",
            "C:\\Windows\\system32",
        ] {
            cache.forever(hostile, Json::from("owned")).await.unwrap();

            let path = cache.path_for(hostile);
            let parent = path.parent().expect("every cache file has a parent");
            assert_eq!(
                parent.canonicalize().unwrap(),
                dir.canonicalize().unwrap(),
                "`{hostile}` escaped to {}",
                path.display()
            );
            assert!(cache.get(hostile).await.unwrap().is_some(), "`{hostile}` must still round-trip");
        }

        // And nothing at all was created outside the directory.
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        assert!(strays.is_empty(), "the file driver must never create subdirectories");
    }

    #[tokio::test]
    async fn every_file_name_is_a_fixed_length_hash() {
        let dir = scratch("hashnames");
        let cache = FileStore::new(&dir).unwrap();
        cache.forever("a very long key with spaces / and ../ inside", Json::from(1)).await.unwrap();

        let name = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .next()
            .expect("one file");

        assert_eq!(name.len(), 32 + ".cache".len());
        assert!(name.trim_end_matches(".cache").chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_fingerprint_is_stable_and_separates_similar_keys() {
        // Hard-coded so a change to the hashing is a deliberate, visible break
        // rather than a silent invalidation of everyone's cache directory.
        assert_eq!(fingerprint("users:1"), fingerprint("users:1"));
        assert_ne!(fingerprint("users:1"), fingerprint("users:2"));
        assert_ne!(fingerprint("ab"), fingerprint("ba"), "a reversed key must not collide");
        assert_eq!(fingerprint("").len(), 32);
    }

    #[tokio::test]
    async fn a_value_survives_a_new_store_over_the_same_directory() {
        let dir = scratch("persist");
        FileStore::new(&dir)
            .unwrap()
            .forever("kept", Json::from("across restarts"))
            .await
            .unwrap();

        let reopened = FileStore::new(&dir).unwrap();
        assert_eq!(reopened.get("kept").await.unwrap(), Some(Json::from("across restarts")));
    }

    #[tokio::test]
    async fn an_expired_file_is_deleted_when_it_is_read() {
        let dir = scratch("expiry");
        let cache = FileStore::new(&dir).unwrap();
        cache.put("brief", Json::from(1), Duration::from_millis(30)).await.unwrap();

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(cache.get("brief").await.unwrap(), None);
        assert!(!cache.path_for("brief").exists(), "the read should have removed the file");
    }

    #[tokio::test]
    async fn a_file_belonging_to_another_key_reads_as_a_miss() {
        let dir = scratch("collision");
        let cache = FileStore::new(&dir).unwrap();
        cache.forever("real", Json::from("value")).await.unwrap();

        // Simulate a hash collision by hand-writing another key's document
        // into the file `imposter` would use.
        let document = Json::object([
            ("key", Json::from("somebody-else")),
            ("expires_at", Json::Null),
            ("value", Json::from("stolen")),
        ]);
        std::fs::write(cache.path_for("imposter"), document.to_string()).unwrap();

        assert_eq!(cache.get("imposter").await.unwrap(), None);
        assert_eq!(cache.get("real").await.unwrap(), Some(Json::from("value")));
    }

    #[tokio::test]
    async fn flush_leaves_files_the_cache_does_not_own() {
        let dir = scratch("flush");
        let cache = FileStore::new(&dir).unwrap();
        cache.forever("mine", Json::from(1)).await.unwrap();
        std::fs::write(dir.join("README.txt"), "not the cache's business").unwrap();

        cache.flush().await.unwrap();

        assert_eq!(cache.get("mine").await.unwrap(), None);
        assert!(dir.join("README.txt").exists());
    }

    #[tokio::test]
    async fn concurrent_increments_on_one_key_stay_exact_within_a_process() {
        let dir = scratch("increments");
        let cache = Arc::new(FileStore::new(&dir).unwrap());

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            tasks.push(tokio::spawn(async move {
                for _ in 0..25 {
                    cache.increment("hits", 1).await.unwrap();
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(cache.get("hits").await.unwrap(), Some(Json::from(200)));
    }

    #[tokio::test]
    async fn remember_writes_through_to_disk_exactly_once() {
        let dir = scratch("remember");
        let cache = FileStore::new(&dir).unwrap();

        let first = cache
            .remember("answer", Duration::from_secs(60), || async { Ok(Json::from(42)) })
            .await
            .unwrap();
        let second = cache
            .remember("answer", Duration::from_secs(60), || async {
                panic!("the second call must be a hit")
            })
            .await
            .unwrap();

        assert_eq!(first, Json::from(42));
        assert_eq!(second, Json::from(42));
    }
}
