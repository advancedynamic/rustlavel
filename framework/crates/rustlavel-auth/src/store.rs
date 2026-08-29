//! Where sessions live between requests.
//!
//! Two drivers ship: [`MemoryStore`], which is the right answer for tests and
//! for a single-process development server, and [`FileStore`], which is the
//! right answer for a single production node and matches Laravel's default. A
//! Redis or database driver is another implementation of [`SessionStore`] and
//! needs no change here.

use crate::session::{Session, is_valid_id};
use rustlavel_core::{Json, Result};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What a store's operations return.
///
/// Spelled out as a boxed future rather than an `async fn` because
/// [`SessionStore`] has to be usable as `dyn SessionStore` — the middleware
/// holds whichever driver the application picked, and cannot be generic over it
/// without infecting every route with a type parameter.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// The default session lifetime: two hours, as in Laravel.
pub const DEFAULT_LIFETIME: Duration = Duration::from_secs(120 * 60);

pub trait SessionStore: Send + Sync + 'static {
    /// Load a session, or `None` if it is unknown or has expired.
    fn read<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<Session>>;

    /// Persist a session under its current id.
    fn write<'a>(&'a self, session: &'a Session) -> StoreFuture<'a, ()>;

    /// Remove a session. Called on logout and after an id rotation.
    fn destroy<'a>(&'a self, id: &'a str) -> StoreFuture<'a, ()>;

    /// Drop everything past its lifetime, returning how many were removed.
    ///
    /// Expired sessions are already refused by `read`, so this is housekeeping
    /// rather than security: without it a busy site accumulates one dead file
    /// per visitor, forever.
    fn gc(&self) -> StoreFuture<'_, usize>;

    /// How long a session may sit untouched before it is considered gone.
    fn lifetime(&self) -> Duration;
}

/// Whether a session written at `updated_at` is still within `lifetime`.
fn is_live(updated_at: u64, lifetime: Duration) -> bool {
    crate::unix_now().saturating_sub(updated_at) < lifetime.as_secs()
}

/// Sessions held in this process's memory.
///
/// Everything is lost on restart and nothing is shared between processes, which
/// makes it perfect for tests and wrong for anything running more than one
/// worker.
pub struct MemoryStore {
    sessions: Mutex<HashMap<String, Json>>,
    lifetime: Duration,
}

impl MemoryStore {
    pub fn new() -> Self {
        MemoryStore::with_lifetime(DEFAULT_LIFETIME)
    }

    pub fn with_lifetime(lifetime: Duration) -> Self {
        MemoryStore { sessions: Mutex::new(HashMap::new()), lifetime }
    }

    /// How many sessions are held, expired ones included. For tests.
    pub fn len(&self) -> usize {
        self.sessions.lock().expect("session store lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        MemoryStore::new()
    }
}

impl SessionStore for MemoryStore {
    fn read<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<Session>> {
        Box::pin(async move {
            let sessions = self.sessions.lock().expect("session store lock poisoned");
            let Some(payload) = sessions.get(id) else { return Ok(None) };

            let session = Session::from_json(id, payload);
            Ok(is_live(session.updated_at(), self.lifetime).then_some(session))
        })
    }

    fn write<'a>(&'a self, session: &'a Session) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let mut stamped = session.clone();
            stamped.touch();

            let mut sessions = self.sessions.lock().expect("session store lock poisoned");
            sessions.insert(stamped.id().to_string(), stamped.to_json());
            Ok(())
        })
    }

    fn destroy<'a>(&'a self, id: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.sessions.lock().expect("session store lock poisoned").remove(id);
            Ok(())
        })
    }

    fn gc(&self) -> StoreFuture<'_, usize> {
        Box::pin(async move {
            let mut sessions = self.sessions.lock().expect("session store lock poisoned");
            let before = sessions.len();
            sessions.retain(|_, payload| {
                let updated_at = payload.get("updated_at").and_then(Json::as_f64).unwrap_or(0.0);
                is_live(updated_at as u64, self.lifetime)
            });
            Ok(before - sessions.len())
        })
    }

    fn lifetime(&self) -> Duration {
        self.lifetime
    }
}

/// One JSON file per session, under `storage/sessions` by default.
///
/// Survives a restart, needs no service to be running, and is what a single
/// application server should use. It does not work across machines: two nodes
/// behind a load balancer each keep their own directory, and a visitor bounced
/// between them looks logged out on alternate requests.
pub struct FileStore {
    directory: PathBuf,
    lifetime: Duration,
}

impl FileStore {
    /// Laravel's path, relative to the application root.
    pub const DEFAULT_DIRECTORY: &'static str = "storage/sessions";

    pub fn new(directory: impl Into<PathBuf>) -> Self {
        FileStore::with_lifetime(directory, DEFAULT_LIFETIME)
    }

    pub fn with_lifetime(directory: impl Into<PathBuf>, lifetime: Duration) -> Self {
        FileStore { directory: directory.into(), lifetime }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The file a session id maps to, or `None` if the id is not one of ours.
    ///
    /// The id is checked against [`is_valid_id`] before it is joined onto the
    /// directory, so a cookie containing `../../../etc/passwd` never reaches
    /// the filesystem at all. Rejecting by shape is safer than sanitising: an
    /// id we did not mint has nothing to say to us anyway.
    fn path_for(&self, id: &str) -> Option<PathBuf> {
        is_valid_id(id).then(|| self.directory.join(format!("{id}.json")))
    }
}

impl SessionStore for FileStore {
    fn read<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<Session>> {
        Box::pin(async move {
            let Some(path) = self.path_for(id) else { return Ok(None) };
            let Ok(source) = tokio::fs::read_to_string(&path).await else { return Ok(None) };

            // A half-written or hand-edited file logs the visitor out; it does
            // not fail the request.
            let Ok(payload) = Json::parse(&source) else {
                let _ = tokio::fs::remove_file(&path).await;
                return Ok(None);
            };

            let session = Session::from_json(id, &payload);
            if !is_live(session.updated_at(), self.lifetime) {
                let _ = tokio::fs::remove_file(&path).await;
                return Ok(None);
            }
            Ok(Some(session))
        })
    }

    fn write<'a>(&'a self, session: &'a Session) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let Some(path) = self.path_for(session.id()) else {
                return Err(rustlavel_core::Error::msg(
                    "refusing to write a session whose id was not generated by this framework",
                ));
            };

            tokio::fs::create_dir_all(&self.directory).await?;

            let mut stamped = session.clone();
            stamped.touch();
            tokio::fs::write(&path, stamped.to_json().to_string()).await?;
            Ok(())
        })
    }

    fn destroy<'a>(&'a self, id: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            if let Some(path) = self.path_for(id) {
                let _ = tokio::fs::remove_file(path).await;
            }
            Ok(())
        })
    }

    fn gc(&self) -> StoreFuture<'_, usize> {
        Box::pin(async move {
            let Ok(mut entries) = tokio::fs::read_dir(&self.directory).await else {
                return Ok(0);
            };

            let mut removed = 0;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path.extension().is_none_or(|extension| extension != "json") {
                    continue;
                }

                let live = match tokio::fs::read_to_string(&path).await {
                    Ok(source) => Json::parse(&source)
                        .ok()
                        .and_then(|payload| payload.get("updated_at").and_then(Json::as_f64))
                        .is_some_and(|updated_at| is_live(updated_at as u64, self.lifetime)),
                    Err(_) => continue,
                };

                if !live && tokio::fs::remove_file(&path).await.is_ok() {
                    removed += 1;
                }
            }
            Ok(removed)
        })
    }

    fn lifetime(&self) -> Duration {
        self.lifetime
    }
}

/// A store shared between the middleware and anything else that needs it.
pub type SharedStore = Arc<dyn SessionStore>;

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own. Tests run concurrently, so sharing one
    /// fixture directory would make them delete each other's session files.
    fn scratch(label: &str) -> PathBuf {
        let directory = std::env::temp_dir()
            .join(format!("rustlavel-auth-{label}-{}", crate::random::hex(8)));
        std::fs::create_dir_all(&directory).expect("could not create the scratch directory");
        directory
    }

    fn saved_session() -> Session {
        let mut session = Session::new();
        session.put("user_id", 7);
        session.flash("status", "saved");
        session.age_flash();
        session
    }

    #[tokio::test]
    async fn the_memory_store_round_trips_a_session() {
        let store = MemoryStore::new();
        let session = saved_session();

        store.write(&session).await.unwrap();
        let loaded = store.read(session.id()).await.unwrap().expect("session should be found");

        assert_eq!(loaded.id(), session.id());
        assert_eq!(loaded.get("user_id").and_then(Json::as_i64), Some(7));
        assert_eq!(loaded.get_string("status").as_deref(), Some("saved"));
    }

    #[tokio::test]
    async fn the_memory_store_forgets_unknown_and_destroyed_sessions() {
        let store = MemoryStore::new();
        let session = saved_session();
        store.write(&session).await.unwrap();

        assert!(store.read(&Session::new_id()).await.unwrap().is_none());

        store.destroy(session.id()).await.unwrap();
        assert!(store.read(session.id()).await.unwrap().is_none());
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn the_memory_store_refuses_and_collects_expired_sessions() {
        let store = MemoryStore::with_lifetime(Duration::ZERO);
        let session = saved_session();
        store.write(&session).await.unwrap();

        assert!(store.read(session.id()).await.unwrap().is_none(), "a zero lifetime expires at once");
        assert_eq!(store.len(), 1, "reading does not remove it from memory");
        assert_eq!(store.gc().await.unwrap(), 1);
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn the_file_store_persists_a_session_across_stores() {
        let directory = scratch("persists");
        let session = saved_session();

        FileStore::new(&directory).write(&session).await.unwrap();

        // A second store, as a restarted process would build.
        let reopened = FileStore::new(&directory);
        let loaded = reopened.read(session.id()).await.unwrap().expect("session should be on disk");

        assert_eq!(loaded.get("user_id").and_then(Json::as_i64), Some(7));
        assert_eq!(loaded.get_string("status").as_deref(), Some("saved"));
        assert!(directory.join(format!("{}.json", session.id())).exists());

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn the_file_store_creates_its_directory_on_first_write() {
        let directory = scratch("creates").join("nested").join("sessions");
        assert!(!directory.exists());

        let store = FileStore::new(&directory);
        let session = saved_session();
        store.write(&session).await.unwrap();

        assert!(directory.is_dir());
        assert!(store.read(session.id()).await.unwrap().is_some());

        let _ = std::fs::remove_dir_all(directory.parent().unwrap().parent().unwrap());
    }

    #[tokio::test]
    async fn the_file_store_destroys_and_expires_sessions() {
        let directory = scratch("destroys");
        let session = saved_session();

        let store = FileStore::new(&directory);
        store.write(&session).await.unwrap();
        store.destroy(session.id()).await.unwrap();
        assert!(store.read(session.id()).await.unwrap().is_none());

        let expiring = FileStore::with_lifetime(&directory, Duration::ZERO);
        expiring.write(&session).await.unwrap();
        assert!(expiring.read(session.id()).await.unwrap().is_none());
        assert!(
            !directory.join(format!("{}.json", session.id())).exists(),
            "reading an expired session should clean the file up"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn the_file_store_collects_expired_files_and_leaves_live_ones() {
        let directory = scratch("collects");
        let store = FileStore::new(&directory);
        let first = saved_session();
        let second = saved_session();

        store.write(&first).await.unwrap();
        store.write(&second).await.unwrap();

        // Both were written a moment ago, so a real lifetime collects nothing.
        assert_eq!(store.gc().await.unwrap(), 0);
        assert!(store.read(first.id()).await.unwrap().is_some());

        // A store whose lifetime has already elapsed collects both.
        let expired = FileStore::with_lifetime(&directory, Duration::ZERO);
        assert_eq!(expired.gc().await.unwrap(), 2);
        assert!(store.read(first.id()).await.unwrap().is_none());

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn the_file_store_never_lets_an_id_escape_its_directory() {
        let directory = scratch("traversal");
        let store = FileStore::new(&directory);

        assert!(store.read("../../../etc/passwd").await.unwrap().is_none());
        assert!(store.read("").await.unwrap().is_none());

        let hostile = Session::with_id("../escaped");
        assert!(store.write(&hostile).await.is_err());
        assert!(!directory.parent().unwrap().join("escaped.json").exists());

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn a_corrupt_session_file_logs_the_visitor_out_instead_of_failing() {
        let directory = scratch("corrupt");
        let id = Session::new_id();
        std::fs::write(directory.join(format!("{id}.json")), "{not json").unwrap();

        let store = FileStore::new(&directory);
        assert!(store.read(&id).await.unwrap().is_none());
        assert!(!directory.join(format!("{id}.json")).exists());

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn both_stores_satisfy_the_trait_object() {
        let stores: Vec<SharedStore> =
            vec![Arc::new(MemoryStore::new()), Arc::new(FileStore::new(scratch("dyn")))];

        for store in stores {
            let session = saved_session();
            store.write(&session).await.unwrap();

            assert!(store.read(session.id()).await.unwrap().is_some());
            assert_eq!(store.lifetime(), DEFAULT_LIFETIME);
        }
    }
}
