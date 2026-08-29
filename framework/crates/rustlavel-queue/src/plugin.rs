//! The dashboard, attached the way every optional package is:
//!
//! ```ignore
//! App::new().plugin(QueueDashboard::new(queue).watching(["default", "emails"]))
//! ```
//!
//! One explicit line in `main.rs`, no auto-discovery. Registering it also puts
//! the queue into the application context, so a handler can reach it with
//! `req.state::<Arc<dyn Queue>>()` and dispatch a job without the application
//! wiring that up separately.
//!
//! JSON rather than HTML: this is the data a Horizon-style dashboard is drawn
//! from, and rustlavel-view is not a dependency of this crate.

use crate::job::DEFAULT_QUEUE;
use crate::queue::{Queue, stats};
use rustlavel_http::plugin::{Plugin, Setup};
use rustlavel_http::{Request, Response};
use std::sync::Arc;

/// A read-only view of what the queue is doing.
pub struct QueueDashboard {
    queue: Arc<dyn Queue>,
    path: String,
    queues: Vec<String>,
}

impl QueueDashboard {
    pub fn new(queue: Arc<dyn Queue>) -> Self {
        QueueDashboard {
            queue,
            path: "/queue".to_string(),
            queues: vec![DEFAULT_QUEUE.to_string()],
        }
    }

    /// Serve the dashboard somewhere other than `/queue`.
    pub fn at(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// Which queues to report sizes for.
    ///
    /// Named explicitly because a driver cannot list its queues without a scan,
    /// and a dashboard that scans a jobs table on every page load is a dashboard
    /// that takes production down.
    pub fn watching<I, S>(mut self, queues: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.queues = queues.into_iter().map(Into::into).collect();
        self
    }
}

impl Plugin for QueueDashboard {
    fn name(&self) -> &'static str {
        "queue"
    }

    fn register(self: Box<Self>, setup: &mut Setup<'_>) {
        let QueueDashboard { queue, path, queues } = *self;

        setup.state(Arc::clone(&queue));

        setup.router.get(&path, move |_request: Request| {
            let queue = Arc::clone(&queue);
            let queues = queues.clone();

            async move {
                match stats(queue.as_ref(), &queues).await {
                    Ok(body) => Response::json(body),
                    // A dashboard that cannot read the queue should say so,
                    // not render an empty page that looks like an idle system.
                    Err(error) => Response::json(rustlavel_core::Json::object([(
                        "error",
                        rustlavel_core::Json::from(error.to_string()),
                    )]))
                    .with_status(rustlavel_http::Status::SERVICE_UNAVAILABLE),
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::QueuedJob;
    use crate::memory::MemoryQueue;
    use rustlavel_core::{Config, Context, Json};
    use rustlavel_http::TestClient;
    use rustlavel_http::router::Router;

    /// Register the plugin the way an application would, and return a client.
    async fn dashboard(queue: Arc<dyn Queue>) -> TestClient {
        let mut router = Router::new();
        let config = Config::with_defaults();
        let mut context = Some(Context::builder());

        let mut setup = Setup { router: &mut router, config: &config, context: &mut context };
        Box::new(QueueDashboard::new(queue).watching(["default", "emails"]))
            .register(&mut setup);

        let context = context.expect("the builder survives setup").build();
        TestClient::new(router).with_context(context)
    }

    #[tokio::test]
    async fn the_dashboard_reports_queue_sizes_and_failures() {
        let queue = Arc::new(MemoryQueue::new());
        queue.push(QueuedJob::new("a", Json::Null)).await.unwrap();
        queue.push(QueuedJob::new("b", Json::Null).on_queue("emails")).await.unwrap();
        queue.push(QueuedJob::new("c", Json::Null).on_queue("emails")).await.unwrap();

        let reserved = queue.pop("default").await.unwrap().unwrap();
        queue.fail(&reserved, "it went wrong").await.unwrap();

        let body = dashboard(queue).await.get("/queue").await.assert_ok().json();

        assert_eq!(body.get("driver").and_then(Json::as_str), Some("memory"));
        assert_eq!(body.get("queues.default").and_then(Json::as_i64), Some(0));
        assert_eq!(body.get("queues.emails").and_then(Json::as_i64), Some(2));
        assert_eq!(body.get("failed.0.name").and_then(Json::as_str), Some("a"));
        assert_eq!(body.get("failed.0.error").and_then(Json::as_str), Some("it went wrong"));
    }

    #[tokio::test]
    async fn the_dashboard_is_empty_but_valid_on_an_idle_system() {
        let body = dashboard(Arc::new(MemoryQueue::new())).await.get("/queue").await.assert_ok().json();

        assert_eq!(body.get("queues.default").and_then(Json::as_i64), Some(0));
        assert_eq!(body.get("failed").and_then(Json::as_array).map(<[Json]>::len), Some(0));
    }

    #[tokio::test]
    async fn the_dashboard_can_be_mounted_somewhere_else() {
        let mut router = Router::new();
        let config = Config::with_defaults();
        let mut context = Some(Context::builder());
        let mut setup = Setup { router: &mut router, config: &config, context: &mut context };

        Box::new(QueueDashboard::new(Arc::new(MemoryQueue::new())).at("/admin/jobs"))
            .register(&mut setup);

        let client = TestClient::new(router).with_context(context.unwrap().build());

        client.get("/admin/jobs").await.assert_ok();
        client.get("/queue").await.assert_not_found();
    }
}
