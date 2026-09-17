# rustlavel-queue

Rustlavel queues: background jobs, workers, scheduling, and a dead-letter store.

Part of [Rustlavel](https://github.com/advancedynamic/rustlavel), a full-stack web
framework for Rust written from scratch — no Axum, no hyper, no SeaORM. Tokio is the
only large dependency.

## Progress, and chains

A job that takes a minute is a spinner for a minute unless it says how far it
has got. Override `handle_with` and report; a request handler in the web
process reads the same store:

```rust
impl Job for Render {
    // …
    async fn handle_with(&self, ctx: &JobContext) -> Result<()> {
        ctx.progress(10, "probing").await;
        ctx.progress(60, "rendering clip 3 of 5").await;
        Ok(())
    }
}

// In the web process:
let progress = DatabaseProgress::new(db.clone());
progress.get(&job_id).await?          // Progress { percent, stage, finished, failed, .. }
```

`rustlavel queue:work` reports to the database automatically when the
application has one. The worker, not the job, marks a job finished on success
and failed on the last attempt — whatever the job last said, because a job that
crashed at 40% is not at 40%.

A pipeline is a chain: the next step is dispatched when the one before it
succeeds, and a step that fails for good ends the chain there, with the unrun
steps kept in the dead-letter entry rather than silently gone.

```rust
queue.dispatch_chain(
    Chain::new(Transcribe { video })
        .then(DetectMoments { video })
        .then(RenderClips { video })
        .named(&video_id),          // progress for the whole under `chain:<id>`
).await?;
```

The chain rides inside the `payload` column, so a `jobs` table created by 0.7
carries one with no migration. The progress table is new: register
`CreateJobProgressTable`, or let `DatabaseQueue::migrate` create it.

## Using it

Enable it through the meta-crate rather than depending on this one directly, so the
versions stay in step:

```toml
[dependencies]
rustlavel = { version = "0.7", features = ["queue"] }
```

## Documentation

- [API documentation](https://docs.rs/rustlavel-queue)
- [The framework](https://github.com/advancedynamic/rustlavel), including the roadmap
  and the design rules this crate is written under

## Licence

MIT.
