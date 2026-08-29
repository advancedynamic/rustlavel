//! Jobs the crate's own tests share.
//!
//! Every fixture is addressed by a freshly minted counter id, so two tests
//! running at the same time on the same thread pool can never see each other's
//! results. A single static counter would make a failure depend on which other
//! tests happened to be running, which is the worst kind of flake to chase.

use crate::job::Job;
use rustlavel_core::{Error, Json, Result};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

#[derive(Default, Clone)]
struct Slot {
    runs: u32,
    total: i64,
}

fn slots() -> &'static Mutex<HashMap<u64, Slot>> {
    static SLOTS: OnceLock<Mutex<HashMap<u64, Slot>>> = OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A test's private tally. Copy, so a job payload can carry just its id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counter(u64);

/// Mint a counter nobody else is using.
pub fn counter() -> Counter {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    slots().lock().expect("slots poisoned").insert(id, Slot::default());
    Counter(id)
}

impl Counter {
    pub fn id(self) -> u64 {
        self.0
    }

    /// Record one run, adding `amount` to the tally. Returns the run number.
    pub fn record(self, amount: i64) -> u32 {
        let mut slots = slots().lock().expect("slots poisoned");
        let slot = slots.entry(self.0).or_default();
        slot.runs += 1;
        slot.total += amount;
        slot.runs
    }

    /// How many times a job addressed to this counter has run.
    pub fn runs(self) -> u32 {
        self.read().runs
    }

    /// The sum of every `step` recorded against it.
    pub fn total(self) -> i64 {
        self.read().total
    }

    fn read(self) -> Slot {
        slots().lock().expect("slots poisoned").get(&self.0).cloned().unwrap_or_default()
    }
}

fn counter_of(payload: &Json) -> Result<Counter> {
    payload
        .get("counter")
        .and_then(Json::as_i64)
        .map(|id| Counter(id as u64))
        .ok_or_else(|| Error::msg("a test job payload needs a `counter`"))
}

fn number(payload: &Json, key: &str) -> i64 {
    payload.get(key).and_then(Json::as_i64).unwrap_or(0)
}

/// Adds `step` to its counter and succeeds.
pub struct CountingJob {
    counter: Counter,
    step: i64,
}

impl CountingJob {
    pub fn new(counter: Counter, step: i64) -> Self {
        CountingJob { counter, step }
    }
}

impl Job for CountingJob {
    const NAME: &'static str = "counting";

    fn payload(&self) -> Json {
        Json::object([
            ("counter", Json::from(self.counter.id())),
            ("step", Json::from(self.step)),
        ])
    }

    fn from_payload(payload: &Json) -> Result<Self> {
        Ok(CountingJob { counter: counter_of(payload)?, step: number(payload, "step") })
    }

    fn handle(&self) -> impl Future<Output = Result<()>> + Send {
        let (counter, step) = (self.counter, self.step);
        async move {
            counter.record(step);
            Ok(())
        }
    }
}

/// Records a run and then always fails, so retries and dead-lettering can be
/// observed.
pub struct FailingJob {
    counter: Counter,
    tries: u32,
}

impl FailingJob {
    pub fn new(counter: Counter, tries: u32) -> Self {
        FailingJob { counter, tries }
    }
}

impl Job for FailingJob {
    const NAME: &'static str = "failing";

    fn payload(&self) -> Json {
        Json::object([
            ("counter", Json::from(self.counter.id())),
            ("tries", Json::from(self.tries)),
        ])
    }

    fn from_payload(payload: &Json) -> Result<Self> {
        Ok(FailingJob { counter: counter_of(payload)?, tries: number(payload, "tries") as u32 })
    }

    fn handle(&self) -> impl Future<Output = Result<()>> + Send {
        let counter = self.counter;
        async move {
            let run = counter.record(1);
            Err(Error::msg(format!("attempt {run} went wrong")))
        }
    }

    fn tries(&self) -> u32 {
        self.tries
    }

    fn retry_after(&self) -> Duration {
        Duration::ZERO
    }
}

/// Fails until it has run `succeed_after` times, then succeeds — a flaky
/// dependency, which is what retries exist for.
pub struct FlakyJob {
    counter: Counter,
    succeed_after: u32,
    tries: u32,
}

impl FlakyJob {
    pub fn new(counter: Counter, succeed_after: u32, tries: u32) -> Self {
        FlakyJob { counter, succeed_after, tries }
    }
}

impl Job for FlakyJob {
    const NAME: &'static str = "flaky";

    fn payload(&self) -> Json {
        Json::object([
            ("counter", Json::from(self.counter.id())),
            ("succeed_after", Json::from(self.succeed_after)),
            ("tries", Json::from(self.tries)),
        ])
    }

    fn from_payload(payload: &Json) -> Result<Self> {
        Ok(FlakyJob {
            counter: counter_of(payload)?,
            succeed_after: number(payload, "succeed_after") as u32,
            tries: number(payload, "tries") as u32,
        })
    }

    fn handle(&self) -> impl Future<Output = Result<()>> + Send {
        let (counter, succeed_after) = (self.counter, self.succeed_after);
        async move {
            let run = counter.record(1);
            if run >= succeed_after {
                Ok(())
            } else {
                Err(Error::msg(format!("attempt {run} was too early")))
            }
        }
    }

    fn tries(&self) -> u32 {
        self.tries
    }

    fn retry_after(&self) -> Duration {
        Duration::ZERO
    }
}

/// Records a run and then panics, which a worker must survive.
pub struct PanickingJob {
    counter: Counter,
    tries: u32,
}

impl PanickingJob {
    pub fn new(counter: Counter, tries: u32) -> Self {
        PanickingJob { counter, tries }
    }
}

impl Job for PanickingJob {
    const NAME: &'static str = "panicking";

    fn payload(&self) -> Json {
        Json::object([
            ("counter", Json::from(self.counter.id())),
            ("tries", Json::from(self.tries)),
        ])
    }

    fn from_payload(payload: &Json) -> Result<Self> {
        Ok(PanickingJob { counter: counter_of(payload)?, tries: number(payload, "tries") as u32 })
    }

    fn handle(&self) -> impl Future<Output = Result<()>> + Send {
        let counter = self.counter;
        async move {
            counter.record(1);
            // Across an await point too, so the catch has to survive a real
            // suspension rather than an eager panic.
            tokio::task::yield_now().await;
            panic!("this job panicked on purpose");
        }
    }

    fn tries(&self) -> u32 {
        self.tries
    }

    fn retry_after(&self) -> Duration {
        Duration::ZERO
    }
}

/// Sleeps before recording, so shutdown and overlap behaviour have something
/// slow to observe.
pub struct SlowJob {
    counter: Counter,
    millis: u64,
}

impl SlowJob {
    pub fn new(counter: Counter, millis: u64) -> Self {
        SlowJob { counter, millis }
    }
}

impl Job for SlowJob {
    const NAME: &'static str = "slow";

    fn payload(&self) -> Json {
        Json::object([
            ("counter", Json::from(self.counter.id())),
            ("millis", Json::from(self.millis)),
        ])
    }

    fn from_payload(payload: &Json) -> Result<Self> {
        Ok(SlowJob { counter: counter_of(payload)?, millis: number(payload, "millis") as u64 })
    }

    fn handle(&self) -> impl Future<Output = Result<()>> + Send {
        let (counter, millis) = (self.counter, self.millis);
        async move {
            tokio::time::sleep(Duration::from_millis(millis)).await;
            counter.record(1);
            Ok(())
        }
    }
}

/// A registry holding every fixture job, which most tests want.
pub fn registry() -> crate::job::JobRegistry {
    let mut registry = crate::job::JobRegistry::new();
    registry.register::<CountingJob>();
    registry.register::<FailingJob>();
    registry.register::<FlakyJob>();
    registry.register::<PanickingJob>();
    registry.register::<SlowJob>();
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_counter_is_private_to_whoever_minted_it() {
        let first = counter();
        let second = counter();

        first.record(5);
        first.record(2);

        assert_eq!(first.runs(), 2);
        assert_eq!(first.total(), 7);
        assert_eq!(second.runs(), 0);
        assert_eq!(second.total(), 0);
    }

    #[tokio::test]
    async fn the_fixtures_round_trip_through_their_payloads() {
        let tally = counter();
        let job = CountingJob::new(tally, 4);
        let rebuilt = CountingJob::from_payload(&job.payload()).unwrap();

        rebuilt.handle().await.unwrap();
        assert_eq!(tally.total(), 4);
    }

    #[test]
    fn a_payload_without_a_counter_is_rejected() {
        assert!(CountingJob::from_payload(&Json::Null).is_err());
    }
}
