//! The middleware pipeline.
//!
//! A middleware receives the request and a [`Next`]; calling `next.run(request)`
//! continues the chain, and not calling it short-circuits — which is how `auth`
//! redirects to a login page without the handler ever running.

use crate::handler::{BoxFuture, Handler};
use crate::request::Request;
use crate::response::Response;
use std::future::Future;
use std::sync::Arc;

pub trait Middleware: Send + Sync + 'static {
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response>;
}

impl<F, Fut> Middleware for F
where
    F: Fn(Request, Next) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response> {
        Box::pin(self(request, next))
    }
}

/// The rest of the pipeline, handed to each middleware in turn.
pub struct Next {
    stack: Arc<Vec<Arc<dyn Middleware>>>,
    index: usize,
    endpoint: Arc<dyn Handler>,
}

impl Next {
    pub(crate) fn new(stack: Arc<Vec<Arc<dyn Middleware>>>, endpoint: Arc<dyn Handler>) -> Self {
        Next { stack, index: 0, endpoint }
    }

    /// Continue to the next middleware, or to the handler when the stack is done.
    pub fn run(self, request: Request) -> BoxFuture<Response> {
        match self.stack.get(self.index).cloned() {
            Some(middleware) => {
                let next = Next {
                    stack: Arc::clone(&self.stack),
                    index: self.index + 1,
                    endpoint: Arc::clone(&self.endpoint),
                };
                middleware.handle(request, next)
            }
            None => self.endpoint.call(request),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;
    use crate::status::Status;

    fn pipeline(stack: Vec<Arc<dyn Middleware>>, endpoint: Arc<dyn Handler>) -> Next {
        Next::new(Arc::new(stack), endpoint)
    }

    #[tokio::test]
    async fn middleware_runs_around_the_handler() {
        let tag: Arc<dyn Middleware> = Arc::new(|request: Request, next: Next| async move {
            let response = next.run(request).await;
            response.with_header("x-tag", "seen")
        });

        let endpoint: Arc<dyn Handler> =
            Arc::new(|_req: Request| async { Response::text("handled") });

        let response = pipeline(vec![tag], endpoint).run(Request::new(Method::Get, "/")).await;

        assert_eq!(response.body_string(), "handled");
        assert_eq!(response.headers.get("x-tag"), Some("seen"));
    }

    #[tokio::test]
    async fn a_middleware_can_short_circuit() {
        let guard: Arc<dyn Middleware> = Arc::new(|_req: Request, _next: Next| async {
            Response::new(Status::UNAUTHORIZED).with_text("denied")
        });

        let endpoint: Arc<dyn Handler> =
            Arc::new(|_req: Request| async {
                panic!("the handler must not run");
                #[allow(unreachable_code)]
                Response::ok()
            });

        let response = pipeline(vec![guard], endpoint).run(Request::new(Method::Get, "/")).await;

        assert_eq!(response.status, Status::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn middleware_runs_in_registration_order() {
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));

        let make = |label: &'static str, log: Arc<std::sync::Mutex<Vec<&'static str>>>| {
            let middleware: Arc<dyn Middleware> = Arc::new(move |request: Request, next: Next| {
                let log = Arc::clone(&log);
                async move {
                    log.lock().unwrap().push(label);
                    next.run(request).await
                }
            });
            middleware
        };

        let stack = vec![make("first", Arc::clone(&order)), make("second", Arc::clone(&order))];
        let endpoint: Arc<dyn Handler> = Arc::new(|_req: Request| async { Response::ok() });

        pipeline(stack, endpoint).run(Request::new(Method::Get, "/")).await;

        assert_eq!(*order.lock().unwrap(), ["first", "second"]);
    }
}
