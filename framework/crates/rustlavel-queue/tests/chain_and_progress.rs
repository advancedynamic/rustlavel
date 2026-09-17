//! Chains run in order and stop on failure; progress is readable from outside
//! the worker. Through the memory queue, which is enough: the worker's logic
//! is the same for every driver, and the database's part — the envelope — has
//! its own test below.

use std::sync::Arc;

use rustlavel_core::{Json, Result};
use rustlavel_queue::{
    Chain, Job, JobContext, JobRegistry, MemoryProgress, MemoryQueue, ProgressStore, Queue, QueueExt,
    QueuedJob, Worker,
};

/// Records the order steps ran in, per run — tests share a process and must
/// not share a list, so each names its own.
static ORDER: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

fn order(run: &str) -> Vec<String> {
    ORDER.lock().unwrap().iter().filter(|(r, _)| r == run).map(|(_, s)| s.clone()).collect()
}

struct Step {
    run: String,
    name: String,
    fail: bool,
}

fn step(run: &str, name: &str, fail: bool) -> Step {
    Step { run: run.into(), name: name.into(), fail }
}

impl Job for Step {
    const NAME: &'static str = "step";
    fn payload(&self) -> Json {
        Json::object([
            ("run", Json::from(self.run.as_str())),
            ("name", Json::from(self.name.as_str())),
            ("fail", Json::from(self.fail)),
        ])
    }
    fn from_payload(payload: &Json) -> Result<Self> {
        Ok(Step {
            run: payload.get("run").and_then(Json::as_str).unwrap_or("?").to_string(),
            name: payload.get("name").and_then(Json::as_str).unwrap_or("?").to_string(),
            fail: payload.get("fail").and_then(Json::as_bool).unwrap_or(false),
        })
    }
    fn tries(&self) -> u32 {
        1
    }
    async fn handle(&self) -> Result<()> {
        ORDER.lock().unwrap().push((self.run.clone(), self.name.clone()));
        if self.fail {
            return Err(rustlavel_core::Error::msg(format!("{} failed on purpose", self.name)));
        }
        Ok(())
    }
}

/// A job that reports as it goes.
struct Render;

impl Job for Render {
    const NAME: &'static str = "render";
    fn payload(&self) -> Json {
        Json::Null
    }
    fn from_payload(_: &Json) -> Result<Self> {
        Ok(Render)
    }
    async fn handle(&self) -> Result<()> {
        Ok(())
    }
    async fn handle_with(&self, ctx: &JobContext) -> Result<()> {
        ctx.progress(10, "probing").await;
        ctx.progress(60, "rendering clip 3 of 5").await;
        Ok(())
    }
}

fn worker(queue: Arc<MemoryQueue>, progress: Arc<dyn ProgressStore>) -> Worker {
    let mut registry = JobRegistry::new();
    registry.register::<Step>().register::<Render>();
    Worker::new(queue, Arc::new(registry)).reporting_to(progress)
}

async fn drain(worker: &Worker) {
    while worker.run_once().await.unwrap().is_some() {}
}

#[tokio::test]
async fn a_chain_runs_its_steps_in_order_one_at_a_time() {
    let queue = Arc::new(MemoryQueue::new());
    let progress: Arc<dyn ProgressStore> = Arc::new(MemoryProgress::new());
    let worker = worker(Arc::clone(&queue), Arc::clone(&progress));

    queue
        .dispatch_chain(
            Chain::new(step("v7", "transcribe", false))
                .then(step("v7", "detect", false))
                .then(step("v7", "render", false))
                .named("video-7"),
        )
        .await
        .unwrap();

    // Only the first step is on the queue; the rest wait inside it.
    assert_eq!(queue.size("default").await.unwrap(), 1, "every step was pushed at once");

    worker.run_once().await.unwrap();
    assert_eq!(order("v7"), ["transcribe"]);
    assert_eq!(queue.size("default").await.unwrap(), 1, "the next step was not pushed");

    drain(&worker).await;
    assert_eq!(order("v7"), ["transcribe", "detect", "render"]);
    assert_eq!(queue.size("default").await.unwrap(), 0);

    // The chain is finished, and says so under its own key.
    let whole = progress.get("chain:video-7").await.unwrap().expect("the chain has progress");
    assert!(whole.finished);
    assert_eq!(whole.percent, 100);
    assert!(whole.failed.is_none());
}

/// A pipeline whose second step fails must not run its third, and must say
/// what happened under the chain's key so the UI can stop spinning.
#[tokio::test]
async fn a_failed_step_ends_the_chain_and_reports_the_failure() {
    let queue = Arc::new(MemoryQueue::new());
    let progress: Arc<dyn ProgressStore> = Arc::new(MemoryProgress::new());
    let worker = worker(Arc::clone(&queue), Arc::clone(&progress));

    queue
        .dispatch_chain(
            Chain::new(step("d", "a", false))
                .then(step("d", "b", true))
                .then(step("d", "c", false))
                .named("doomed"),
        )
        .await
        .unwrap();

    drain(&worker).await;
    assert_eq!(order("d"), ["a", "b"], "a step after the failure ran");

    let whole = progress.get("chain:doomed").await.unwrap().expect("recorded");
    assert!(whole.finished);
    assert!(whole.failed.as_deref().unwrap_or("").contains("b failed"), "{whole:?}");

    // And the remaining step is visible in the dead-letter entry, not gone.
    let failed = queue.failed_jobs().await.unwrap();
    assert_eq!(failed.len(), 1);
    let stored = &failed[0].payload;
    assert!(stored.get("$then").is_some(), "the unrun steps were dropped: {stored}");
}

/// The web process asks the store, not the worker.
#[tokio::test]
async fn progress_is_readable_by_job_id_while_and_after_it_runs() {
    let queue = Arc::new(MemoryQueue::new());
    let progress: Arc<dyn ProgressStore> = Arc::new(MemoryProgress::new());
    let worker = worker(Arc::clone(&queue), Arc::clone(&progress));

    let id = queue.dispatch(&Render).await.unwrap();
    worker.run_once().await.unwrap();

    let done = progress.get(&id).await.unwrap().expect("the job reported");
    // The worker's word beats the job's last report: it returned Ok, so 100.
    assert_eq!(done.percent, 100);
    assert!(done.finished);
    assert_eq!(done.stage, "finished");
}

/// A job that never reports still gets a record when it finishes, so a UI
/// polling for it sees "done" rather than nothing forever.
#[tokio::test]
async fn a_silent_job_is_still_marked_finished() {
    let queue = Arc::new(MemoryQueue::new());
    let progress: Arc<dyn ProgressStore> = Arc::new(MemoryProgress::new());
    let worker = worker(Arc::clone(&queue), Arc::clone(&progress));

    let id = queue.dispatch(&step("q", "quiet", false)).await.unwrap();
    worker.run_once().await.unwrap();
    assert!(progress.get(&id).await.unwrap().expect("recorded").finished);
}

/// The chain rides inside the payload, so a jobs table from 0.7 carries one
/// with no migration — and a payload with no chain is stored exactly as before.
#[test]
fn the_envelope_round_trips_and_leaves_a_plain_payload_alone() {
    let plain = QueuedJob::new("render", Json::object([("video", Json::from(7))]));
    assert_eq!(plain.stored_payload(), plain.payload, "a chainless job was wrapped");

    let chained = Chain::new(step("e", "a", false))
        .then(step("e", "b", false))
        .then(step("e", "c", false))
        .named("v")
        .into_queued();
    let stored = chained.stored_payload();
    assert!(stored.get("$then").is_some());

    let back = QueuedJob::new("step", Json::Null).with_stored_payload(stored);
    assert_eq!(back.chain.as_deref(), Some("v"));
    assert_eq!(back.then.len(), 2);
    assert_eq!(back.payload, chained.payload);
    assert_eq!(back.then[1].payload.get("name").and_then(Json::as_str), Some("c"));

    // Peeling one step off carries the chain id and the remainder forward.
    let next = back.next_in_chain().expect("two steps remain");
    assert_eq!(next.chain.as_deref(), Some("v"));
    assert_eq!(next.then.len(), 1);
    assert!(next.next_in_chain().unwrap().next_in_chain().is_none());
}
