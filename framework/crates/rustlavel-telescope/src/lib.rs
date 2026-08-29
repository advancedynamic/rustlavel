//! rustlavel-telescope: the debugging dashboard, enabled with one line.
//!
//! ```ignore
//! App::new()?
//!     .routes(routes::web::routes)
//!     .plugin(Telescope::default())
//!     .serve()
//!     .await
//! ```
//!
//! Telescope listens on the instrumentation bus core has carried since the
//! first release, keeps what it hears in a bounded in-memory ring buffer, and
//! serves a single self-contained page at `/telescope` that shows requests, the
//! queries and log lines each one produced, and how long everything took.
//!
//! # Why a ring buffer and not SQLite
//!
//! Laravel Telescope writes every entry to a database. That is the right answer
//! in PHP, where the process dies at the end of the request and there is
//! nowhere else to put anything. Rust has a process that lives for weeks, so
//! this crate makes two promises instead:
//!
//! **A debugging tool must never be the reason a request is slow.** Recording
//! an entry costs a redaction pass, a short mutex and a push onto a
//! `VecDeque` — no connection, no transaction, no `fsync`, no `await` on the
//! request's path. Optional persistence is a send into a queue that a writer
//! thread drains; the request never waits for a disk. The buffer is bounded, so
//! a server under load a week from now uses exactly the memory it uses today.
//!
//! **A debugging tool must never add a dependency to production builds.** The
//! whole crate depends on `rustlavel-core` and `rustlavel-http` and nothing
//! else — no embedded database, no serialisation framework, not even an async
//! runtime of its own. Persistence is JSON lines written with the JSON
//! implementation core already has: readable with `tail -f`, recoverable after
//! a `kill -9` at the cost of at most the last line, and compacted at boot so
//! the file can never outgrow the buffer it feeds.
//!
//! # What it records
//!
//! Whatever is dispatched. The framework emits `http.request`, `db.query` and
//! `log` today, and the roadmap promises `ai.call`, `queue.job` and
//! `mail.sent`; an [`Entry`] therefore stores an open field map rather than a
//! fixed enum, and every renderer degrades gracefully — a kind Telescope has
//! never seen still gets a readable summary, a stable colour, a duration and a
//! full field table.
//!
//! # Safety
//!
//! The dashboard shows SQL, log messages, and every field a package chose to
//! emit. So it refuses to mount in production unless someone explicitly says
//! otherwise (`telescope.enabled`, or [`Telescope::even_in_production`]), and
//! values under keys that look like credentials are replaced at record time —
//! before they reach the buffer, the API, or the journal on disk.

pub mod dashboard;
pub mod entry;
pub mod journal;
pub mod plugin;
pub mod recorder;
pub mod redact;
pub mod routes;
pub mod store;

pub use entry::Entry;
pub use journal::Journal;
pub use plugin::Telescope;
pub use recorder::Recorder;
pub use store::{Filter, Store};

/// The event bus is process-global and tests run concurrently, so any test that
/// subscribes has to take this lock first — the same pattern the HTTP crate's
/// error page tests use for the process-wide debug flag.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard};

    static BUS: Mutex<()> = Mutex::new(());

    /// A poisoned guard still grants exclusive access: one failing test should
    /// not cascade into every other test in the crate failing too.
    pub fn exclusive() -> MutexGuard<'static, ()> {
        BUS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
