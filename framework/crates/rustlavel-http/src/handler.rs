use crate::request::Request;
use crate::response::{IntoResponse, Response};
use std::future::Future;
use std::pin::Pin;

/// A boxed, owned future — the shape every handler and middleware returns.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Anything that can answer a request.
///
/// Implemented for every `async fn(Request) -> impl IntoResponse`, so an
/// application never names this trait: it just writes a function.
pub trait Handler: Send + Sync + 'static {
    fn call(&self, request: Request) -> BoxFuture<Response>;
}

impl<F, Fut, R> Handler for F
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: IntoResponse + 'static,
{
    fn call(&self, request: Request) -> BoxFuture<Response> {
        let future = self(request);
        Box::pin(async move { future.await.into_response() })
    }
}

/// A handler that always answers with the same response, for redirects and
/// static pages registered straight on the router.
pub struct Fixed(pub Response);

impl Handler for Fixed {
    fn call(&self, _request: Request) -> BoxFuture<Response> {
        let response = self.0.clone();
        Box::pin(async move { response })
    }
}
