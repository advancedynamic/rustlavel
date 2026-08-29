//! Shared scaffolding for this crate's own tests.
//!
//! Tests run in parallel, and the instrumentation bus in `rustlavel_core` is
//! process-wide: two tests that subscribe at the same time would see each
//! other's events. Anything that subscribes takes this lock first, and the
//! guard clears the registry again on the way out.

use std::sync::{Mutex, MutexGuard, OnceLock};

pub struct EventsGuard(#[allow(dead_code)] MutexGuard<'static, ()>);

impl Drop for EventsGuard {
    fn drop(&mut self) {
        rustlavel_core::events::clear_subscribers();
    }
}

pub fn events_lock() -> EventsGuard {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    let guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        // A test that panicked while holding the lock poisoned it; the next
        // test still deserves to run rather than inherit the failure.
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    rustlavel_core::events::clear_subscribers();
    EventsGuard(guard)
}
