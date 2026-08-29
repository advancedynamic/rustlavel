//! Keeping leases alive in the background.
//!
//! A dynamic credential is only useful because it expires, and it only works
//! because somebody renews it. That somebody is a task: it wakes at the
//! two-thirds point of each lease (see [`Lease::renew_after`]), renews, and
//! goes back to sleep. The third of the lease it deliberately leaves is what a
//! failed renewal is retried inside.
//!
//! What it renews *with* is a callback. The renewer never calls the store
//! itself, so extending a token, a database credential, or something an engine
//! written later issues are all the same job here, and none of them needs a
//! branch in this file.
//!
//! ```ignore
//! let renewer = Renewer::new();
//! renewer.watch("database", lease, move |lease| {
//!     let client = client.clone();
//!     async move { renew_lease(&client, &lease).await }
//! });
//! ```

use crate::lease::Lease;
use rustlavel_core::Result;
use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// The shortest a renewer will ever sleep.
///
/// This is the guard against a busy loop, and it is needed: a lease whose
/// two-thirds point has already passed answers `renew_after()` with zero every
/// time it is asked, so without a floor the task would renew as fast as the
/// store could answer — which is a denial of service against your own Vault,
/// written by accident.
pub const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Renews leases until it is told to stop.
pub struct Renewer {
    stop: watch::Sender<bool>,
    min_interval: Duration,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    watching: std::sync::Arc<AtomicUsize>,
}

impl Default for Renewer {
    fn default() -> Renewer {
        Renewer::new()
    }
}

impl Renewer {
    pub fn new() -> Renewer {
        Renewer {
            // A `watch` rather than a `Notify` because a lease handed over
            // after the shutdown signal must still see it: a watch receiver
            // reads the current value, a notification that already fired is
            // gone.
            stop: watch::channel(false).0,
            min_interval: MIN_INTERVAL,
            tasks: Mutex::new(Vec::new()),
            watching: std::sync::Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The floor on how often a lease may be renewed.
    ///
    /// Tests turn it down to keep themselves quick. Production should not need
    /// to touch it — a lease short enough for this to matter is a lease that
    /// should have been issued longer.
    pub fn min_interval(mut self, interval: Duration) -> Renewer {
        self.min_interval = interval;
        self
    }

    /// How many leases are still being watched.
    pub fn watching(&self) -> usize {
        self.watching.load(Ordering::SeqCst)
    }

    /// Keep a lease alive, calling `renew` each time it comes due.
    ///
    /// `renew` is handed the current lease and returns the renewed one — which
    /// is [`Lease::renewed`] for an extension, or a whole new `Lease` for
    /// something that was rotated rather than extended.
    ///
    /// A lease that cannot be renewed is not watched at all. Asking Vault to
    /// renew something it will not renew is a request that can only fail, and
    /// doing it on a timer is a request that fails forever.
    pub fn watch<F, Fut>(&self, name: impl Into<String>, lease: Lease, renew: F)
    where
        F: Fn(Lease) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Lease>> + Send + 'static,
    {
        let name = name.into();

        if !lease.renewable || !lease.exists() {
            rustlavel_core::debug!("`{name}` carries no renewable lease; nothing to renew");
            return;
        }

        // Without a runtime there is nothing to spawn onto. That happens when
        // an application builds itself outside `#[tokio::main]`, and saying so
        // is far better than the panic `tokio::spawn` would raise from inside
        // a plugin's `register`.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            rustlavel_core::warn!(
                "not renewing the lease on `{name}`: no tokio runtime is running, so there is \
                 nothing to renew it from. The credential will stop working in {}s.",
                lease.remaining().as_secs()
            );
            return;
        };

        let stop = self.stop.subscribe();
        let min_interval = self.min_interval;
        let watching = self.watching.clone();
        watching.fetch_add(1, Ordering::SeqCst);

        let handle = runtime.spawn(async move {
            renew_until_stopped(name, lease, renew, min_interval, stop).await;
            watching.fetch_sub(1, Ordering::SeqCst);
        });

        self.tasks.lock().unwrap_or_else(|e| e.into_inner()).push(handle);
    }

    /// Tell every task to stop, without waiting.
    ///
    /// `send_replace` rather than `send`: a `watch` sender with no receivers
    /// refuses a `send` and leaves the value alone, which would mean stopping
    /// a renewer before it had watched anything did nothing at all — and the
    /// lease handed over a moment later would run on.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }

    /// Stop, and wait for every task to finish.
    ///
    /// A task is only ever between sleeps or inside one renewal, so this
    /// returns in about as long as one call to the store takes.
    pub async fn shutdown(&self) {
        self.stop();

        let tasks: Vec<JoinHandle<()>> =
            self.tasks.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
        for task in tasks {
            let _ = task.await;
        }
    }
}

/// Dropping the renewer stops the tasks it started.
///
/// Without this a plugin that went out of scope would leave its tasks renewing
/// leases for an application that no longer exists.
impl Drop for Renewer {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

/// No leases, no tokens, nothing to redact — but written out rather than
/// derived, so it stays that way if a field is ever added.
impl std::fmt::Debug for Renewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renewer")
            .field("watching", &self.watching())
            .field("min_interval", &self.min_interval)
            .finish()
    }
}

async fn renew_until_stopped<F, Fut>(
    name: String,
    mut lease: Lease,
    renew: F,
    min_interval: Duration,
    mut stop: watch::Receiver<bool>,
) where
    F: Fn(Lease) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Lease>> + Send + 'static,
{
    let mut wait = lease.renew_after().max(min_interval);

    loop {
        if sleep_or_stop(wait, &mut stop).await {
            rustlavel_core::debug!("stopped renewing `{name}`");
            return;
        }

        match renew(lease.clone()).await {
            Ok(renewed) => {
                rustlavel_core::debug!(
                    "renewed the lease on `{name}` for {}s",
                    renewed.duration.as_secs()
                );
                lease = renewed;

                if !lease.renewable || !lease.exists() {
                    rustlavel_core::warn!(
                        "`{name}` came back from a renewal without a renewable lease. It stops \
                         working in {}s and renewing cannot prevent that — whatever issued it \
                         has to issue a new one.",
                        lease.remaining().as_secs()
                    );
                    return;
                }

                wait = lease.renew_after().max(min_interval);
            }
            Err(error) => {
                if lease.is_expired() {
                    rustlavel_core::error!(
                        "lost the lease on `{name}`: {error}. Whatever it held open — a database \
                         account, a token — has expired, and this process is now using a \
                         credential the store no longer honours."
                    );
                    return;
                }

                rustlavel_core::warn!(
                    "could not renew `{name}`: {error}. Retrying; {}s left on the lease.",
                    lease.remaining().as_secs()
                );

                // Inside the third the two-thirds rule left over, and getting
                // shorter as that third runs out — but never below the floor,
                // so a store that is refusing quickly cannot be hammered.
                wait = (lease.remaining() / 4).max(min_interval);
            }
        }
    }
}

/// Sleep, unless shutdown comes first. `true` means stop.
async fn sleep_or_stop(wait: Duration, stop: &mut watch::Receiver<bool>) -> bool {
    // Checked before selecting: a receiver that subscribed after the signal has
    // already marked it seen, and `changed()` would then wait for a second one
    // that never comes.
    if *stop.borrow() {
        return true;
    }

    tokio::select! {
        _ = tokio::time::sleep(wait) => *stop.borrow(),
        // An error means the sender is gone, which is a stop as much as a
        // signal is.
        _ = stop.changed() => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_core::Error;
    use std::sync::Arc;
    use std::time::Instant;

    /// A lease that is `elapsed` old, so a test can start one part-way through.
    fn aged(duration: Duration, elapsed: Duration) -> Lease {
        Lease {
            id: "database/creds/app/abc".into(),
            duration,
            renewable: true,
            issued: Instant::now() - elapsed,
        }
    }

    /// Wait for a condition, or give up. Never asserts on how long something
    /// took, only that it eventually happened — a loaded machine may be slow,
    /// but it cannot make the renewer fire early.
    async fn within(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        done()
    }

    #[tokio::test]
    async fn renews_at_the_two_thirds_point_and_not_before() {
        let renewals = Arc::new(AtomicUsize::new(0));
        let counter = renewals.clone();

        let renewer = Renewer::new().min_interval(Duration::from_millis(10));
        renewer.watch("database", aged(Duration::from_millis(300), Duration::ZERO), move |lease| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(lease.renewed(Duration::from_millis(300), true))
            }
        });

        // The two-thirds point is 200ms away, so nothing may have happened yet.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(renewals.load(Ordering::SeqCst), 0, "renewed far too early");

        assert!(
            within(Duration::from_secs(3), || renewals.load(Ordering::SeqCst) >= 1).await,
            "the lease was never renewed"
        );

        renewer.shutdown().await;
    }

    #[tokio::test]
    async fn retries_a_renewal_that_failed_within_the_time_that_is_left() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();

        let renewer = Renewer::new().min_interval(Duration::from_millis(10));
        // Two thirds of the way through already, so the first attempt is due
        // at once and there is a third of the lease left to retry inside.
        renewer.watch(
            "database",
            aged(Duration::from_secs(30), Duration::from_secs(20)),
            move |lease| {
                let counter = counter.clone();
                async move {
                    // The first attempt fails, the second succeeds.
                    if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err(Error::msg("the secret store is on a standby node"))
                    } else {
                        Ok(lease.renewed(Duration::from_secs(30), true))
                    }
                }
            },
        );

        assert!(
            within(Duration::from_secs(3), || attempts.load(Ordering::SeqCst) >= 2).await,
            "a failed renewal was never retried"
        );

        renewer.shutdown().await;
    }

    #[tokio::test]
    async fn gives_up_when_the_lease_is_lost_rather_than_retrying_forever() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();

        let renewer = Renewer::new().min_interval(Duration::from_millis(10));
        renewer.watch("database", aged(Duration::from_millis(60), Duration::ZERO), move |_| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(Error::msg("permission denied"))
            }
        });

        // The task ends on its own once the lease expires: there is nothing
        // left to renew, and a task that kept trying would log forever.
        assert!(
            within(Duration::from_secs(3), || renewer.watching() == 0).await,
            "the renewer never gave up on an expired lease"
        );
        assert!(attempts.load(Ordering::SeqCst) >= 1, "it gave up without trying");
    }

    #[tokio::test]
    async fn stops_cleanly_on_shutdown() {
        let renewals = Arc::new(AtomicUsize::new(0));
        let counter = renewals.clone();

        let renewer = Renewer::new();
        // An hour-long lease: nothing is due for forty minutes, so shutting
        // down has to interrupt the sleep rather than wait it out.
        renewer.watch("token", aged(Duration::from_secs(3600), Duration::ZERO), move |lease| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(lease.renewed(Duration::from_secs(3600), true))
            }
        });
        assert_eq!(renewer.watching(), 1);

        let stopped = tokio::time::timeout(Duration::from_secs(5), renewer.shutdown()).await;

        assert!(stopped.is_ok(), "shutdown waited for the sleep instead of interrupting it");
        assert_eq!(renewer.watching(), 0);
        assert_eq!(renewals.load(Ordering::SeqCst), 0, "nothing was due");
    }

    #[tokio::test]
    async fn a_lease_handed_over_after_shutdown_stops_immediately() {
        let renewer = Renewer::new().min_interval(Duration::from_millis(10));
        renewer.stop();

        renewer.watch("late", aged(Duration::from_secs(30), Duration::from_secs(29)), |lease| async move {
            Ok(lease.renewed(Duration::from_secs(30), true))
        });

        assert!(
            within(Duration::from_secs(3), || renewer.watching() == 0).await,
            "a task started after shutdown kept running"
        );
    }

    #[tokio::test]
    async fn a_lease_that_cannot_be_renewed_is_not_watched_at_all() {
        let renewer = Renewer::new();
        let mut lease = aged(Duration::from_secs(60), Duration::ZERO);
        lease.renewable = false;

        renewer.watch("static", lease, |lease| async move { Ok(lease) });
        renewer.watch("none", Lease::none(), |lease| async move { Ok(lease) });

        assert_eq!(renewer.watching(), 0);
    }

    #[tokio::test]
    async fn an_overdue_lease_does_not_become_a_busy_loop() {
        let renewals = Arc::new(AtomicUsize::new(0));
        let counter = renewals.clone();

        // Renewed into a lease that is already past its own two-thirds point,
        // which is what would spin without the floor.
        let renewer = Renewer::new().min_interval(Duration::from_millis(50));
        renewer.watch("database", aged(Duration::from_secs(30), Duration::from_secs(29)), move |_| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(Lease {
                    id: "database/creds/app/abc".into(),
                    duration: Duration::from_secs(30),
                    renewable: true,
                    issued: Instant::now() - Duration::from_secs(29),
                })
            }
        });

        tokio::time::sleep(Duration::from_millis(250)).await;
        renewer.shutdown().await;

        let count = renewals.load(Ordering::SeqCst);
        assert!(count >= 1, "it should still have renewed");
        assert!(count <= 10, "one renewal per 50ms at most, got {count} in 250ms");
    }
}
