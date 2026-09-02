//! A circuit breaker for outbound calls.
//!
//! Retrying is the right answer to a request that failed by accident, and the
//! wrong answer to a service that is down. When an upstream stops answering,
//! every caller retrying it turns one outage into three: the upstream cannot
//! recover under the load, this application's tasks all sit blocked waiting
//! for timeouts, and whoever called *this* application times out in turn.
//!
//! A breaker stops that by refusing to make a call it expects to fail:
//!
//! ```ignore
//! let http = Client::new().retries(2).breaker(CircuitBreaker::new());
//!
//! match http.get("https://api.example.com/rates").send().await {
//!     Ok(response) => &hellip;,
//!     // Nothing was sent, so nothing can have had an effect — which is
//!     // exactly when falling back to a cached answer is safe.
//!     Err(Error::Unavailable(_)) => cached_rates(),
//!     Err(error) => return Err(error),
//! }
//! ```
//!
//! Three states, and the middle one is the point:
//!
//! - **Closed.** Calls go through, and their outcomes are counted.
//! - **Open.** Calls are refused immediately, without a socket, for
//!   [`CircuitBreaker::reset_after`]. This is what gives the upstream room to
//!   recover, and what stops this process from filling up with tasks waiting
//!   on a timeout.
//! - **Half-open.** After that pause, a few calls are let through as probes.
//!   Enough of them succeeding closes the breaker; one failing opens it again
//!   for another pause. Without this state a breaker either never recovers or
//!   recovers by sending the full load at a service that has not come back.
//!
//! It trips on a failure *rate* over a sliding window, not a raw count: five
//! failures means something very different in ten calls than in ten thousand.
//! Below [`CircuitBreaker::minimum_calls`] it never trips at all, so a service
//! is not written off on the strength of its first two requests.
//!
//! A breaker is kept **per host**, so a slow payment provider does not stop
//! this application from talking to its own search cluster.
//!
//! **A 5xx counts as a failure; a 4xx does not.** A 404 or a 422 is this
//! application getting something wrong, and repeating that a thousand times
//! says nothing about whether the upstream is healthy. Widen or narrow it with
//! [`CircuitBreaker::count_failure_when`].

use rustlavel_core::{Error, Result};
use rustlavel_http::Status;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How many slices the sliding window is cut into.
///
/// Ten is the usual choice: fine enough that the window advances smoothly
/// rather than forgetting everything at once, coarse enough that the
/// bookkeeping is a handful of integers.
const BUCKETS: u64 = 10;

/// What a breaker is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Calls go through.
    Closed,
    /// Calls are refused without being attempted.
    Open,
    /// A few calls are allowed through to see whether the upstream is back.
    HalfOpen,
}

/// Decides whether a response counts against the upstream.
type FailureRule = Arc<dyn Fn(Status) -> bool + Send + Sync>;

#[derive(Clone)]
pub struct CircuitBreaker {
    settings: Settings,
    hosts: Arc<Mutex<HashMap<String, Circuit>>>,
}

#[derive(Clone)]
struct Settings {
    failure_rate: f64,
    minimum_calls: u32,
    window: Duration,
    reset_after: Duration,
    probes: u32,
    is_failure: FailureRule,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl CircuitBreaker {
    /// Trip above a 50% failure rate, once at least 20 calls are in a
    /// 60-second window; pause for 30 seconds; then probe with 3 calls.
    ///
    /// These are Resilience4j's defaults, which are reasonable for an API a
    /// request path depends on. A background job talking to something flaky
    /// wants a longer pause; a call already behind a cache wants a shorter
    /// one.
    pub fn new() -> Self {
        CircuitBreaker {
            settings: Settings {
                failure_rate: 0.5,
                minimum_calls: 20,
                window: Duration::from_secs(60),
                reset_after: Duration::from_secs(30),
                probes: 3,
                is_failure: Arc::new(|status: Status| status.code() >= 500),
            },
            hosts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The failure rate that opens the breaker, from 0.0 to 1.0.
    pub fn failure_rate(mut self, rate: f64) -> Self {
        self.settings.failure_rate = rate.clamp(0.0, 1.0);
        self
    }

    /// How many calls must be in the window before the rate is believed.
    pub fn minimum_calls(mut self, calls: u32) -> Self {
        self.settings.minimum_calls = calls.max(1);
        self
    }

    /// How far back the failure rate is measured.
    pub fn window(mut self, window: Duration) -> Self {
        self.settings.window = window.max(Duration::from_millis(BUCKETS));
        self
    }

    /// How long the breaker stays open before probing.
    pub fn reset_after(mut self, pause: Duration) -> Self {
        self.settings.reset_after = pause;
        self
    }

    /// How many probes must succeed to close the breaker again.
    pub fn probes(mut self, probes: u32) -> Self {
        self.settings.probes = probes.max(1);
        self
    }

    /// Decide which responses count against the upstream.
    ///
    /// The default is `status.code() >= 500`. Adding 429 is defensible when a
    /// shared rate limit means the whole host is unusable, and a mistake when
    /// one busy endpoint would take the rest of the host down with it.
    pub fn count_failure_when(mut self, rule: impl Fn(Status) -> bool + Send + Sync + 'static) -> Self {
        self.settings.is_failure = Arc::new(rule);
        self
    }

    /// What this breaker is doing for a host. `Closed` for one never called.
    pub fn state(&self, host: &str) -> State {
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        match hosts.get_mut(host) {
            Some(circuit) => circuit.state(&self.settings, Instant::now()),
            None => State::Closed,
        }
    }

    /// Ask to make a call.
    ///
    /// `Ok(Permit)` means go ahead and report the outcome on the permit;
    /// `Err(Error::Unavailable)` means the breaker is open and nothing was
    /// sent.
    pub fn acquire(&self, host: &str) -> Result<Permit> {
        let now = Instant::now();
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        let circuit = hosts.entry(host.to_string()).or_default();

        match circuit.state(&self.settings, now) {
            State::Closed => Ok(Permit::new(self.clone(), host.to_string(), false)),
            State::Open => {
                let for_another = self
                    .settings
                    .reset_after
                    .saturating_sub(now.saturating_duration_since(circuit.opened_at.unwrap_or(now)));
                Err(Error::Unavailable(format!(
                    "{host} is not being called: too many of the last requests to it failed, \
                     so the circuit is open for another {} second(s). Nothing was sent.",
                    for_another.as_secs().max(1)
                )))
            }
            State::HalfOpen => {
                // Exactly as many probes as configured go through at once. The
                // rest are refused, because sending the full load at a service
                // that has not recovered is how a breaker makes things worse.
                // `then`, not `then_some`: the latter would evaluate `left - 1`
                // even at zero, which is an overflow rather than a refusal.
                let took_one = circuit
                    .probes_left
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                        (left > 0).then(|| left - 1)
                    })
                    .is_ok();
                if took_one {
                    Ok(Permit::new(self.clone(), host.to_string(), true))
                } else {
                    Err(Error::Unavailable(format!(
                        "{host} is being probed after a failure and is not taking other calls \
                         yet. Nothing was sent."
                    )))
                }
            }
        }
    }

    /// Whether a response counts as the upstream failing.
    pub fn counts_as_failure(&self, status: Status) -> bool {
        (self.settings.is_failure)(status)
    }

    fn record(&self, host: &str, was_probe: bool, failed: bool) {
        let now = Instant::now();
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        let Some(circuit) = hosts.get_mut(host) else { return };

        // Re-read the state: a probe may have been overtaken by another
        // probe's failure, which already re-opened the circuit.
        match circuit.state(&self.settings, now) {
            State::HalfOpen if was_probe => {
                if failed {
                    circuit.open(now, &self.settings);
                    rustlavel_core::debug!("circuit for {host} opened again: a probe failed");
                } else {
                    circuit.probe_successes += 1;
                    if circuit.probe_successes >= self.settings.probes {
                        circuit.close(now);
                        rustlavel_core::info!("circuit for {host} closed: the probes succeeded");
                    }
                }
            }
            // A call that started before the circuit opened, finishing after.
            // Its outcome is stale; counting it would either re-open a circuit
            // that just closed or pollute a fresh window.
            _ if was_probe => {}
            _ => {
                circuit.count(now, &self.settings, failed);
                if circuit.should_open(&self.settings) {
                    circuit.open(now, &self.settings);
                    rustlavel_core::warn!(
                        "circuit for {host} opened: {:.0}% of the last {} calls failed",
                        circuit.failure_rate() * 100.0,
                        circuit.total()
                    );
                }
            }
        }
    }

    /// Forget everything, for a test or an operator who knows better.
    pub fn reset(&self) {
        self.hosts.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

impl std::fmt::Debug for CircuitBreaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircuitBreaker")
            .field("failure_rate", &self.settings.failure_rate)
            .field("minimum_calls", &self.settings.minimum_calls)
            .field("window", &self.settings.window)
            .field("reset_after", &self.settings.reset_after)
            .field("probes", &self.settings.probes)
            .finish()
    }
}

/// Permission to make one call. Report the outcome, or drop it to report
/// nothing.
///
/// Dropping without reporting gives the permit back and records no data
/// point. That is the honest answer for a call that was cancelled: it says
/// nothing about the upstream, and holding the permit would wedge a half-open
/// circuit that can then neither close nor open.
pub struct Permit {
    breaker: CircuitBreaker,
    host: String,
    probe: bool,
    reported: bool,
}

impl std::fmt::Debug for Permit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Permit").field("host", &self.host).field("probe", &self.probe).finish()
    }
}

impl Permit {
    fn new(breaker: CircuitBreaker, host: String, probe: bool) -> Self {
        Permit { breaker, host, probe, reported: false }
    }

    pub fn success(mut self) {
        self.reported = true;
        self.breaker.record(&self.host, self.probe, false);
    }

    pub fn failure(mut self) {
        self.reported = true;
        self.breaker.record(&self.host, self.probe, true);
    }

    /// Report by status, using the breaker's own rule.
    pub fn record_status(self, status: Status) {
        if self.breaker.counts_as_failure(status) {
            self.failure()
        } else {
            self.success()
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if self.reported || !self.probe {
            return;
        }
        // A probe that was never reported hands its permit back, so half-open
        // does not run out of them and stall.
        let mut hosts = self.breaker.hosts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(circuit) = hosts.get_mut(&self.host) {
            circuit.probes_left.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// One host's breaker.
#[derive(Debug, Default)]
struct Circuit {
    /// Buckets of the sliding window: (bucket number, successes, failures).
    buckets: VecDeque<(u64, u32, u32)>,
    origin: Option<Instant>,
    opened_at: Option<Instant>,
    half_open: bool,
    probes_left: AtomicU32,
    probe_successes: u32,
}

impl Circuit {
    /// The current state, moving Open to HalfOpen when the pause has elapsed.
    fn state(&mut self, settings: &Settings, now: Instant) -> State {
        let Some(opened_at) = self.opened_at else { return State::Closed };

        if self.half_open {
            return State::HalfOpen;
        }
        if now.saturating_duration_since(opened_at) >= settings.reset_after {
            self.half_open = true;
            self.probes_left = AtomicU32::new(settings.probes);
            self.probe_successes = 0;
            return State::HalfOpen;
        }
        State::Open
    }

    fn open(&mut self, now: Instant, settings: &Settings) {
        self.opened_at = Some(now);
        self.half_open = false;
        self.probe_successes = 0;
        self.probes_left = AtomicU32::new(settings.probes);
        // The window starts again, so the calls that opened the breaker are
        // not still there to open it a second time the moment it closes.
        self.buckets.clear();
    }

    fn close(&mut self, _now: Instant) {
        self.opened_at = None;
        self.half_open = false;
        self.probe_successes = 0;
        self.buckets.clear();
    }

    /// Which slice of the window `now` falls in.
    fn bucket_of(&mut self, now: Instant, settings: &Settings) -> u64 {
        let origin = *self.origin.get_or_insert(now);
        let width = settings.window / BUCKETS as u32;
        (now.saturating_duration_since(origin).as_nanos() / width.as_nanos().max(1)) as u64
    }

    fn count(&mut self, now: Instant, settings: &Settings, failed: bool) {
        let bucket = self.bucket_of(now, settings);

        // Anything older than the window is no longer evidence about now.
        while let Some(&(number, _, _)) = self.buckets.front() {
            if number + BUCKETS <= bucket {
                self.buckets.pop_front();
            } else {
                break;
            }
        }

        match self.buckets.back_mut() {
            Some((number, successes, failures)) if *number == bucket => {
                if failed {
                    *failures += 1
                } else {
                    *successes += 1
                }
            }
            _ => self.buckets.push_back((bucket, u32::from(!failed), u32::from(failed))),
        }
    }

    fn total(&self) -> u32 {
        self.buckets.iter().map(|(_, s, f)| s + f).sum()
    }

    fn failures(&self) -> u32 {
        self.buckets.iter().map(|(_, _, f)| f).sum()
    }

    fn failure_rate(&self) -> f64 {
        match self.total() {
            0 => 0.0,
            total => f64::from(self.failures()) / f64::from(total),
        }
    }

    fn should_open(&self, settings: &Settings) -> bool {
        self.opened_at.is_none()
            && self.total() >= settings.minimum_calls
            && self.failure_rate() >= settings.failure_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breaker() -> CircuitBreaker {
        CircuitBreaker::new()
            .minimum_calls(4)
            .failure_rate(0.5)
            .reset_after(Duration::from_millis(60))
            .probes(2)
    }

    fn fail(breaker: &CircuitBreaker, host: &str, times: usize) {
        for _ in 0..times {
            breaker.acquire(host).expect("closed").failure();
        }
    }

    fn succeed(breaker: &CircuitBreaker, host: &str, times: usize) {
        for _ in 0..times {
            breaker.acquire(host).expect("closed").success();
        }
    }

    #[test]
    fn a_new_breaker_is_closed_and_lets_everything_through() {
        let breaker = breaker();
        assert_eq!(breaker.state("api.example"), State::Closed);
        succeed(&breaker, "api.example", 50);
        assert_eq!(breaker.state("api.example"), State::Closed);
    }

    #[test]
    fn it_does_not_trip_below_the_minimum_however_bad_the_rate() {
        // Three failures out of three is a 100% failure rate, and still not
        // enough to write a service off.
        let breaker = breaker();
        fail(&breaker, "api.example", 3);
        assert_eq!(breaker.state("api.example"), State::Closed);
    }

    #[test]
    fn it_trips_once_the_rate_and_the_volume_are_both_reached() {
        let breaker = breaker();
        succeed(&breaker, "api.example", 2);
        fail(&breaker, "api.example", 2);
        assert_eq!(breaker.state("api.example"), State::Open, "4 calls, half of them failed");
    }

    #[test]
    fn a_low_failure_rate_over_many_calls_does_not_trip_it() {
        // The reason the threshold is a rate: five failures in a hundred calls
        // is a healthy service, and a raw count of five would have tripped.
        let breaker = CircuitBreaker::new().minimum_calls(10).failure_rate(0.5);
        succeed(&breaker, "api.example", 95);
        fail(&breaker, "api.example", 5);
        assert_eq!(breaker.state("api.example"), State::Closed);
    }

    #[test]
    fn an_open_breaker_refuses_without_sending_anything() {
        let breaker = breaker();
        fail(&breaker, "api.example", 4);

        let error = breaker.acquire("api.example").expect_err("refused");
        assert!(matches!(error, Error::Unavailable(_)), "{error:?}");
        let message = error.to_string();
        assert!(message.contains("api.example"), "{message}");
        assert!(message.contains("Nothing was sent"), "{message}");
    }

    #[test]
    fn breakers_are_kept_per_host() {
        let breaker = breaker();
        fail(&breaker, "payments.example", 4);

        assert_eq!(breaker.state("payments.example"), State::Open);
        assert_eq!(breaker.state("search.example"), State::Closed, "an unrelated host is unaffected");
        breaker.acquire("search.example").expect("still closed").success();
    }

    #[tokio::test]
    async fn after_the_pause_it_probes_and_closes_on_success() {
        let breaker = breaker();
        fail(&breaker, "api.example", 4);
        assert_eq!(breaker.state("api.example"), State::Open);

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(breaker.state("api.example"), State::HalfOpen);

        breaker.acquire("api.example").expect("a probe").success();
        assert_eq!(breaker.state("api.example"), State::HalfOpen, "one probe of two");
        breaker.acquire("api.example").expect("a probe").success();
        assert_eq!(breaker.state("api.example"), State::Closed, "both probes succeeded");
    }

    #[tokio::test]
    async fn one_failing_probe_opens_it_again_for_another_pause() {
        let breaker = breaker();
        fail(&breaker, "api.example", 4);
        tokio::time::sleep(Duration::from_millis(80)).await;

        breaker.acquire("api.example").expect("a probe").failure();
        assert_eq!(breaker.state("api.example"), State::Open, "still not healthy");
        breaker.acquire("api.example").expect_err("refused again");

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(breaker.state("api.example"), State::HalfOpen, "and it probes again after");
    }

    #[tokio::test]
    async fn half_open_lets_through_only_as_many_probes_as_configured() {
        let breaker = breaker();
        fail(&breaker, "api.example", 4);
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Two permits held at once, as two concurrent tasks would.
        let first = breaker.acquire("api.example").expect("probe one");
        let second = breaker.acquire("api.example").expect("probe two");
        breaker.acquire("api.example").expect_err("the third is refused, not queued");

        first.success();
        second.success();
        assert_eq!(breaker.state("api.example"), State::Closed);
    }

    #[tokio::test]
    async fn a_probe_that_is_dropped_gives_its_permit_back() {
        // A cancelled request says nothing about the upstream. If its permit
        // were lost, half-open would run out and the breaker would neither
        // close nor open again.
        let breaker = breaker();
        fail(&breaker, "api.example", 4);
        tokio::time::sleep(Duration::from_millis(80)).await;

        drop(breaker.acquire("api.example").expect("probe one"));
        drop(breaker.acquire("api.example").expect("probe two"));
        drop(breaker.acquire("api.example").expect("permits came back"));

        assert_eq!(breaker.state("api.example"), State::HalfOpen, "no outcome was recorded");
        breaker.acquire("api.example").expect("a probe").success();
        breaker.acquire("api.example").expect("a probe").success();
        assert_eq!(breaker.state("api.example"), State::Closed);
    }

    #[tokio::test]
    async fn closing_forgets_the_failures_that_opened_it() {
        // Otherwise the calls that tripped the breaker are still in the window
        // when it closes, and the next failure trips it straight back.
        let breaker = breaker();
        fail(&breaker, "api.example", 4);
        tokio::time::sleep(Duration::from_millis(80)).await;
        succeed(&breaker, "api.example", 2);
        assert_eq!(breaker.state("api.example"), State::Closed);

        fail(&breaker, "api.example", 1);
        assert_eq!(breaker.state("api.example"), State::Closed, "one failure is not four");
    }

    #[tokio::test]
    async fn failures_age_out_of_the_window() {
        let breaker = CircuitBreaker::new()
            .minimum_calls(4)
            .failure_rate(0.5)
            .window(Duration::from_millis(100));

        fail(&breaker, "api.example", 3);
        assert_eq!(breaker.state("api.example"), State::Closed, "not yet at the minimum");

        // Past the window, so those three are no longer evidence about now.
        tokio::time::sleep(Duration::from_millis(160)).await;
        fail(&breaker, "api.example", 3);
        assert_eq!(breaker.state("api.example"), State::Closed, "the old failures aged out");
    }

    #[test]
    fn a_4xx_is_not_the_upstreams_fault_and_a_5xx_is() {
        let breaker = breaker();
        assert!(!breaker.counts_as_failure(Status::NOT_FOUND));
        assert!(!breaker.counts_as_failure(Status::UNPROCESSABLE));
        assert!(!breaker.counts_as_failure(Status::TOO_MANY_REQUESTS));
        assert!(breaker.counts_as_failure(Status::INTERNAL_ERROR));
        assert!(breaker.counts_as_failure(Status::SERVICE_UNAVAILABLE));

        // Four hundred 404s do not open it.
        for _ in 0..400 {
            breaker.acquire("api.example").expect("closed").record_status(Status::NOT_FOUND);
        }
        assert_eq!(breaker.state("api.example"), State::Closed);
    }

    #[test]
    fn the_failure_rule_can_be_replaced() {
        let breaker = breaker().count_failure_when(|status| status.code() == 429);
        assert!(breaker.counts_as_failure(Status::TOO_MANY_REQUESTS));
        assert!(!breaker.counts_as_failure(Status::INTERNAL_ERROR));

        for _ in 0..4 {
            breaker.acquire("api.example").expect("closed").record_status(Status::TOO_MANY_REQUESTS);
        }
        assert_eq!(breaker.state("api.example"), State::Open);
    }

    #[test]
    fn reset_forgets_everything() {
        let breaker = breaker();
        fail(&breaker, "api.example", 4);
        assert_eq!(breaker.state("api.example"), State::Open);
        breaker.reset();
        assert_eq!(breaker.state("api.example"), State::Closed);
    }

    #[test]
    fn an_unavailable_error_is_a_503_and_says_which_dependency() {
        let breaker = breaker();
        fail(&breaker, "payments.example", 4);
        let error = breaker.acquire("payments.example").expect_err("open");
        assert_eq!(error.status(), 503);
        assert_eq!(error.title(), "Dependency Unavailable");
    }

    #[tokio::test]
    async fn many_tasks_racing_on_one_host_agree_on_the_outcome() {
        let breaker = CircuitBreaker::new().minimum_calls(100).failure_rate(0.5);
        let mut tasks = Vec::new();
        for i in 0..200 {
            let breaker = breaker.clone();
            tasks.push(tokio::spawn(async move {
                if let Ok(permit) = breaker.acquire("api.example") {
                    if i % 2 == 0 { permit.failure() } else { permit.success() }
                }
            }));
        }
        for task in tasks {
            task.await.expect("no task panicked");
        }
        // Exactly at the threshold with the volume met, so it must have opened.
        assert_eq!(breaker.state("api.example"), State::Open);
    }
}
