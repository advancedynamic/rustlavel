//! The scheduler: cron, expressed as a builder.
//!
//! ```ignore
//! let mut schedule = schedule(queue.clone());
//!
//! schedule.job(SendDailyReport).daily().at("13:00");
//! schedule.job(PruneSessions).hourly();
//! schedule.job(SyncInventory).every_minutes(5).without_overlapping();
//! schedule.job(BillCustomers).weekly_on(Weekday::Monday, "09:00");
//! schedule.job(RebuildSearchIndex).cron("*/5 * * * *");
//!
//! schedule.run(shutdown).await?;
//! ```
//!
//! Each builder method returns `&mut ScheduledEvent`, the same shape
//! rustlavel-db's schema builder uses, so the chain reads as one sentence.
//! A method that is handed something invalid records the complaint on the event
//! rather than returning a `Result` that would break the chain; the whole set is
//! checked in one go by [`Scheduler::compile`], which `run` calls first.

use crate::cron::{Cron, Weekday};
use crate::job::{BoxFuture, Job, QueuedJob};
use crate::queue::Queue;
use crate::time::{self, unix_now};
use crate::worker::Shutdown;
use rustlavel_core::{Error, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Start building a schedule. The Rust spelling of Laravel's `schedule()`.
pub fn schedule(queue: Arc<dyn Queue>) -> Scheduler {
    Scheduler::new(queue)
}

/// An arbitrary piece of work a schedule can run, for tasks with no job behind
/// them.
type Task = Arc<dyn Fn() -> BoxFuture<'static, Result<()>> + Send + Sync>;

#[derive(Clone)]
enum Action {
    /// Put a job on the queue. What `.job(...)` builds.
    Dispatch(QueuedJob),
    /// Run something in the scheduler's own process.
    Call(Task),
}

/// The five fields, kept as text so `at` can rewrite two of them without
/// re-parsing the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fields {
    minute: String,
    hour: String,
    day_of_month: String,
    month: String,
    day_of_week: String,
}

impl Fields {
    /// Every minute — the starting point every builder method narrows.
    fn every_minute() -> Self {
        Fields {
            minute: "*".into(),
            hour: "*".into(),
            day_of_month: "*".into(),
            month: "*".into(),
            day_of_week: "*".into(),
        }
    }

    fn expression(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.minute, self.hour, self.day_of_month, self.month, self.day_of_week
        )
    }
}

/// One entry in a schedule.
pub struct ScheduledEvent {
    name: String,
    fields: Fields,
    action: Action,
    without_overlapping: bool,
    /// Set while a run of this event is in flight. Shared with the spawned
    /// task, which clears it on the way out — including on a panic, because the
    /// guard that clears it does so on drop.
    running: Arc<AtomicBool>,
    /// The first thing a builder method disagreed with, reported by `compile`.
    error: Option<String>,
}

impl ScheduledEvent {
    fn new(name: String, action: Action) -> Self {
        ScheduledEvent {
            name,
            fields: Fields::every_minute(),
            action,
            without_overlapping: false,
            running: Arc::new(AtomicBool::new(false)),
            error: None,
        }
    }

    /// What this event is called in logs and in a [`TickReport`].
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The cron expression this event has been narrowed to so far.
    pub fn expression(&self) -> String {
        self.fields.expression()
    }

    /// Set the whole expression at once.
    pub fn cron(&mut self, expression: &str) -> &mut Self {
        let parts: Vec<&str> = expression.split_whitespace().collect();

        if parts.len() != 5 {
            return self.complain(format!(
                "`{expression}` has {} field(s); a cron expression needs exactly 5",
                parts.len()
            ));
        }

        self.fields = Fields {
            minute: parts[0].into(),
            hour: parts[1].into(),
            day_of_month: parts[2].into(),
            month: parts[3].into(),
            day_of_week: parts[4].into(),
        };
        self
    }

    /// Every minute. The default, spelled out.
    pub fn every_minute(&mut self) -> &mut Self {
        self.fields = Fields::every_minute();
        self
    }

    /// Every `n` minutes, from the top of the hour.
    pub fn every_minutes(&mut self, n: u32) -> &mut Self {
        if n == 0 || n > 59 {
            return self.complain(format!(
                "every_minutes({n}) is not a schedule: use a value between 1 and 59"
            ));
        }
        self.fields.minute = format!("*/{n}");
        self
    }

    /// On the hour, every hour.
    pub fn hourly(&mut self) -> &mut Self {
        self.fields.minute = "0".into();
        self.fields.hour = "*".into();
        self
    }

    /// Once a day at midnight. Narrow it with [`ScheduledEvent::at`].
    pub fn daily(&mut self) -> &mut Self {
        self.fields.minute = "0".into();
        self.fields.hour = "0".into();
        self
    }

    /// Once a week, on a day and at a time. Times are UTC.
    pub fn weekly_on(&mut self, day: Weekday, time: &str) -> &mut Self {
        self.fields.day_of_week = day.number().to_string();
        self.at(time)
    }

    /// Once a month, on the first, at midnight.
    pub fn monthly(&mut self) -> &mut Self {
        self.daily();
        self.fields.day_of_month = "1".into();
        self
    }

    /// The time of day, as `HH:MM`.
    ///
    /// Sets the hour and minute fields, so `daily().at("13:00")` means one run a
    /// day at one in the afternoon, UTC.
    pub fn at(&mut self, time: &str) -> &mut Self {
        let Some((hour, minute)) = time.split_once(':') else {
            return self.complain(format!("`{time}` is not a time. Write it as `HH:MM`."));
        };

        match (hour.parse::<u32>(), minute.parse::<u32>()) {
            (Ok(hour), Ok(minute)) if hour < 24 && minute < 60 => {
                self.fields.hour = hour.to_string();
                self.fields.minute = minute.to_string();
                self
            }
            _ => self.complain(format!(
                "`{time}` is not a time. Write it as `HH:MM`, with the hour between 00 and 23."
            )),
        }
    }

    /// Skip a run when the previous one is still going.
    ///
    /// A task that takes six minutes on a five-minute schedule would otherwise
    /// pile up until the process falls over. The lock is a flag in this process
    /// — good enough for the scheduler, which is meant to be run once, not once
    /// per node.
    pub fn without_overlapping(&mut self) -> &mut Self {
        self.without_overlapping = true;
        self
    }

    /// True while a run of this event is in flight.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn complain(&mut self, message: String) -> &mut Self {
        if self.error.is_none() {
            self.error = Some(message);
        }
        self
    }
}

/// An event with its expression parsed, ready to be asked about a moment.
struct CompiledEvent {
    name: String,
    cron: Cron,
    action: Action,
    without_overlapping: bool,
    running: Arc<AtomicBool>,
}

/// What one tick did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// Events that were due and were started.
    pub started: Vec<String>,
    /// Events that were due but whose previous run had not finished.
    pub skipped: Vec<String>,
}

/// A set of scheduled events, and the loop that runs them.
pub struct Scheduler {
    queue: Arc<dyn Queue>,
    events: Vec<ScheduledEvent>,
}

impl Scheduler {
    pub fn new(queue: Arc<dyn Queue>) -> Self {
        Scheduler { queue, events: Vec::new() }
    }

    /// Schedule a job. It is dispatched onto the queue when due, not run here:
    /// the scheduler's tick must stay short, and a worker is what runs work.
    ///
    /// The payload is taken now, when the schedule is defined, so a job that
    /// needs fresh data at run time should read it in `handle` rather than
    /// capture it in a field.
    pub fn job<J: Job>(&mut self, job: J) -> &mut ScheduledEvent {
        self.add(ScheduledEvent::new(J::NAME.to_string(), Action::Dispatch(job.to_queued())))
    }

    /// Schedule an already-built envelope — a job with no Rust type behind it.
    pub fn push(&mut self, job: QueuedJob) -> &mut ScheduledEvent {
        self.add(ScheduledEvent::new(job.name.clone(), Action::Dispatch(job)))
    }

    /// Schedule a closure to run in this process. For work with no job behind
    /// it: pruning a cache, touching a heartbeat file.
    pub fn call<F>(&mut self, name: impl Into<String>, task: F) -> &mut ScheduledEvent
    where
        F: Fn() -> BoxFuture<'static, Result<()>> + Send + Sync + 'static,
    {
        self.add(ScheduledEvent::new(name.into(), Action::Call(Arc::new(task))))
    }

    fn add(&mut self, event: ScheduledEvent) -> &mut ScheduledEvent {
        self.events.push(event);
        self.events.last_mut().expect("just pushed")
    }

    pub fn events(&self) -> &[ScheduledEvent] {
        &self.events
    }

    /// Parse every expression, reporting all the bad ones at once.
    ///
    /// Called by `run` before the first tick, so a typo in a schedule stops the
    /// process at boot rather than at 3 a.m. on a Sunday.
    fn compile(&self) -> Result<Vec<CompiledEvent>> {
        let mut compiled = Vec::with_capacity(self.events.len());
        let mut problems = Vec::new();

        for event in &self.events {
            if let Some(error) = &event.error {
                problems.push(format!("`{}`: {error}", event.name));
                continue;
            }

            match Cron::parse(&event.expression()) {
                Ok(cron) => compiled.push(CompiledEvent {
                    name: event.name.clone(),
                    cron,
                    action: event.action.clone(),
                    without_overlapping: event.without_overlapping,
                    running: Arc::clone(&event.running),
                }),
                Err(error) => problems.push(format!("`{}`: {error}", event.name)),
            }
        }

        if problems.is_empty() {
            Ok(compiled)
        } else {
            Err(Error::msg(format!("this schedule cannot run:\n  {}", problems.join("\n  "))))
        }
    }

    /// Check every expression without running anything.
    pub fn validate(&self) -> Result<()> {
        self.compile().map(|_| ())
    }

    /// The events due at a given moment, by name. For tests and `schedule:list`.
    pub fn due_at(&self, at: i64) -> Result<Vec<String>> {
        let moment = time::from_unix(at);
        Ok(self
            .compile()?
            .into_iter()
            .filter(|event| event.cron.matches(&moment))
            .map(|event| event.name)
            .collect())
    }

    /// When each event next runs after `at`, for `schedule:list`.
    pub fn next_runs_after(&self, at: i64) -> Result<Vec<(String, Option<i64>)>> {
        Ok(self
            .compile()?
            .into_iter()
            .map(|event| (event.name, event.cron.next_after(at)))
            .collect())
    }

    /// Run everything due at `at`, and return without waiting for it.
    ///
    /// Each due event is spawned rather than awaited: a scheduler that waited
    /// would be late for everything behind a slow task, which is the very thing
    /// `without_overlapping` exists to survive.
    pub async fn tick(&self, at: i64) -> Result<TickReport> {
        let moment = time::from_unix(at);
        let mut report = TickReport::default();

        for event in self.compile()? {
            if !event.cron.matches(&moment) {
                continue;
            }

            // `compare_exchange` rather than a read-then-write: two ticks can
            // only overlap if they raced, and a racy check would let both
            // through exactly when it matters.
            let claimed = event
                .running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok();

            // When overlapping is allowed the flag is only advisory, so losing
            // the claim is not a reason to skip.
            if !claimed && event.without_overlapping {
                rustlavel_core::warn!(
                    "scheduled task `{}` is still running; skipping this run",
                    event.name
                );
                report.skipped.push(event.name);
                continue;
            }

            report.started.push(event.name.clone());
            let queue = Arc::clone(&self.queue);
            let guard = RunningGuard(Arc::clone(&event.running));
            let name = event.name.clone();
            let action = event.action.clone();

            tokio::spawn(async move {
                // Held for the whole run and dropped afterwards, so the flag is
                // cleared even if the task panics.
                let _guard = guard;

                let result = match action {
                    Action::Dispatch(job) => queue.push(job).await.map(|_| ()),
                    Action::Call(task) => task().await,
                };

                if let Err(error) = result {
                    rustlavel_core::error!("scheduled task `{name}` failed: {error}");
                }
            });
        }

        Ok(report)
    }

    /// Run the schedule until shutdown, waking on each minute boundary.
    ///
    /// Aligning to the boundary rather than sleeping a fixed minute is what
    /// keeps a schedule from drifting: every tick is asked about a whole minute,
    /// so an expression matches exactly once however long a tick took.
    pub async fn run(&self, shutdown: Shutdown) -> Result<()> {
        // Fail loudly at boot rather than silently never running an event.
        self.compile()?;

        let mut last_tick: Option<i64> = None;

        loop {
            if shutdown.is_signalled() {
                return Ok(());
            }

            let now = unix_now();
            let minute = time::start_of_minute(now);

            if last_tick != Some(minute) {
                last_tick = Some(minute);
                self.tick(minute).await?;
            }

            // Sleep to just past the next boundary, so the tick above always
            // sees a fresh minute.
            let next = time::start_of_minute(unix_now()) + 60;
            let wait = Duration::from_millis(((next - unix_now()).max(0) as u64) * 1000 + 50);

            if shutdown.wait_for(wait).await {
                return Ok(());
            }
        }
    }
}

/// Clears an event's "running" flag when the run ends, however it ends.
struct RunningGuard(Arc<AtomicBool>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryQueue;
    use crate::tests_support::{CountingJob, counter};
    use crate::time::to_unix;

    fn scheduler() -> (Arc<MemoryQueue>, Scheduler) {
        let queue = Arc::new(MemoryQueue::new());
        let scheduler = Scheduler::new(queue.clone());
        (queue, scheduler)
    }

    #[test]
    fn the_builder_methods_produce_the_expressions_they_promise() {
        let (_queue, mut schedule) = scheduler();
        let tally = counter();

        assert_eq!(schedule.job(CountingJob::new(tally, 1)).every_minute().expression(), "* * * * *");
        assert_eq!(schedule.job(CountingJob::new(tally, 1)).hourly().expression(), "0 * * * *");
        assert_eq!(schedule.job(CountingJob::new(tally, 1)).daily().expression(), "0 0 * * *");
        assert_eq!(
            schedule.job(CountingJob::new(tally, 1)).daily().at("13:00").expression(),
            "0 13 * * *"
        );
        assert_eq!(
            schedule.job(CountingJob::new(tally, 1)).every_minutes(5).expression(),
            "*/5 * * * *"
        );
        assert_eq!(
            schedule
                .job(CountingJob::new(tally, 1))
                .weekly_on(Weekday::Monday, "09:30")
                .expression(),
            "30 9 * * 1"
        );
        assert_eq!(schedule.job(CountingJob::new(tally, 1)).monthly().expression(), "0 0 1 * *");
        assert_eq!(
            schedule.job(CountingJob::new(tally, 1)).cron("*/5 * * * *").expression(),
            "*/5 * * * *"
        );

        schedule.validate().unwrap();
    }

    #[test]
    fn a_bad_time_is_reported_when_the_schedule_is_checked_not_when_it_is_written() {
        let (_queue, mut schedule) = scheduler();
        schedule.job(CountingJob::new(counter(), 1)).daily().at("25:00");

        let error = schedule.validate().unwrap_err().to_string();
        assert!(error.contains("`counting`"), "{error}");
        assert!(error.contains("`25:00` is not a time"), "{error}");
    }

    #[test]
    fn every_bad_event_is_reported_at_once() {
        let (_queue, mut schedule) = scheduler();
        schedule.call("first", || Box::pin(async { Ok(()) })).at("nonsense");
        schedule.call("second", || Box::pin(async { Ok(()) })).cron("not a cron expression");
        schedule.call("third", || Box::pin(async { Ok(()) })).every_minutes(0);

        let error = schedule.validate().unwrap_err().to_string();
        assert!(error.contains("`first`"), "{error}");
        assert!(error.contains("`second`"), "{error}");
        assert!(error.contains("`third`"), "{error}");
    }

    #[test]
    fn a_schedule_reports_what_is_due_and_what_comes_next() {
        let (_queue, mut schedule) = scheduler();
        schedule.job(CountingJob::new(counter(), 1)).daily().at("13:00");

        let one_pm = to_unix(2026, 8, 29, 13, 0, 0);
        assert_eq!(schedule.due_at(one_pm).unwrap(), vec!["counting".to_string()]);
        assert!(schedule.due_at(one_pm + 60).unwrap().is_empty());

        let next = schedule.next_runs_after(to_unix(2026, 8, 29, 14, 0, 0)).unwrap();
        assert_eq!(next[0].1, Some(to_unix(2026, 8, 30, 13, 0, 0)));
    }

    #[tokio::test]
    async fn a_due_job_is_dispatched_onto_the_queue_rather_than_run() {
        let (queue, mut schedule) = scheduler();
        let tally = counter();
        schedule.job(CountingJob::new(tally, 1)).daily().at("13:00");

        let report = schedule.tick(to_unix(2026, 8, 29, 13, 0, 0)).await.unwrap();
        assert_eq!(report.started, vec!["counting".to_string()]);

        // The dispatch is spawned, so give it a moment to land.
        for _ in 0..100 {
            if queue.size("default").await.unwrap() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        assert_eq!(queue.size("default").await.unwrap(), 1);
        assert_eq!(tally.runs(), 0, "the scheduler queues work, it does not do it");
    }

    #[tokio::test]
    async fn a_job_that_is_not_due_is_left_alone() {
        let (queue, mut schedule) = scheduler();
        schedule.job(CountingJob::new(counter(), 1)).daily().at("13:00");

        let report = schedule.tick(to_unix(2026, 8, 29, 9, 0, 0)).await.unwrap();

        assert!(report.started.is_empty());
        assert_eq!(queue.size("default").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn without_overlapping_skips_a_run_while_the_last_one_is_still_going() {
        let (_queue, mut schedule) = scheduler();
        let tally = counter();

        schedule
            .call("slow-task", move || {
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    tally.record(1);
                    Ok(())
                })
            })
            .every_minute()
            .without_overlapping();

        let first = schedule.tick(to_unix(2026, 8, 29, 13, 0, 0)).await.unwrap();
        assert_eq!(first.started, vec!["slow-task".to_string()]);

        // The task is still sleeping, so the next minute's run is skipped.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = schedule.tick(to_unix(2026, 8, 29, 13, 1, 0)).await.unwrap();
        assert!(second.started.is_empty());
        assert_eq!(second.skipped, vec!["slow-task".to_string()]);

        // Once it finishes, the lock is released and the next run goes ahead.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(tally.runs(), 1);

        let third = schedule.tick(to_unix(2026, 8, 29, 13, 2, 0)).await.unwrap();
        assert_eq!(third.started, vec!["slow-task".to_string()]);
    }

    #[tokio::test]
    async fn without_it_a_slow_task_is_allowed_to_overlap() {
        let (_queue, mut schedule) = scheduler();
        let tally = counter();

        schedule.call("slow-task", move || {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                tally.record(1);
                Ok(())
            })
        });

        schedule.tick(to_unix(2026, 8, 29, 13, 0, 0)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = schedule.tick(to_unix(2026, 8, 29, 13, 1, 0)).await.unwrap();

        assert_eq!(second.started, vec!["slow-task".to_string()], "the second run started too");

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(tally.runs(), 2, "both runs completed");
    }

    #[tokio::test]
    async fn a_panicking_task_releases_the_overlap_lock() {
        let (_queue, mut schedule) = scheduler();

        schedule
            .call("panicking-task", || {
                Box::pin(async {
                    tokio::task::yield_now().await;
                    panic!("scheduled work can panic too");
                })
            })
            .every_minute()
            .without_overlapping();

        schedule.tick(to_unix(2026, 8, 29, 13, 0, 0)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let second = schedule.tick(to_unix(2026, 8, 29, 13, 1, 0)).await.unwrap();
        assert_eq!(
            second.started,
            vec!["panicking-task".to_string()],
            "a task that blew up must not hold the lock forever"
        );
    }

    #[tokio::test]
    async fn run_refuses_to_start_with_a_broken_expression() {
        let (_queue, mut schedule) = scheduler();
        schedule.job(CountingJob::new(counter(), 1)).cron("* * *");

        let shutdown = Shutdown::new();
        shutdown.signal();

        assert!(schedule.run(shutdown).await.is_err());
    }

    #[tokio::test]
    async fn run_stops_on_shutdown() {
        let (_queue, mut schedule) = scheduler();
        schedule.job(CountingJob::new(counter(), 1)).daily().at("03:00");

        let shutdown = Shutdown::new();
        let signal = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            signal.signal();
        });

        let started = std::time::Instant::now();
        schedule.run(shutdown).await.unwrap();

        assert!(started.elapsed() < Duration::from_secs(30), "it should not wait out a full minute");
    }

    #[test]
    fn an_event_knows_its_own_name() {
        let (_queue, mut schedule) = scheduler();
        schedule.job(CountingJob::new(counter(), 1)).daily();

        assert_eq!(schedule.events()[0].name(), "counting");
        assert!(!schedule.events()[0].is_running());
    }
}
