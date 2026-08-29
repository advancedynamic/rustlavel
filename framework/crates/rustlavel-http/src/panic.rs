//! Catching panics inside a handler so one bad request cannot take down the
//! connection — and so the dev error page can show where it happened.

use std::any::Any;
use std::cell::RefCell;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Once;
use std::task::{Context, Poll};

/// Where the last panic on this thread happened, captured by our hook because
/// the payload alone does not carry a location.
#[derive(Debug, Clone)]
pub struct PanicLocation {
    pub file: String,
    pub line: u32,
    pub column: u32,
}

thread_local! {
    static LAST_LOCATION: RefCell<Option<PanicLocation>> = const { RefCell::new(None) };
}

/// Install a panic hook that records the location and stays quiet.
///
/// Without this the default hook prints a backtrace to stderr for every caught
/// panic, which is noise when the error page is about to show the same thing.
pub fn install_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some(location) = info.location() {
                LAST_LOCATION.with(|slot| {
                    *slot.borrow_mut() = Some(PanicLocation {
                        file: location.file().to_string(),
                        line: location.line(),
                        column: location.column(),
                    });
                });
            }
            // Keep the default reporting for panics outside a request.
            if !in_request() {
                previous(info);
            }
        }));
    });
}

thread_local! {
    static IN_REQUEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn in_request() -> bool {
    IN_REQUEST.with(std::cell::Cell::get)
}

pub fn take_location() -> Option<PanicLocation> {
    LAST_LOCATION.with(|slot| slot.borrow_mut().take())
}

/// Turn a panic payload into a readable message.
pub fn message_of(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "the handler panicked".to_string()
    }
}

/// A future that catches a panic raised while polling the inner future.
pub struct CatchUnwind<F> {
    inner: F,
}

impl<F> CatchUnwind<F> {
    pub fn new(inner: F) -> Self {
        CatchUnwind { inner }
    }
}

impl<F: Future> Future for CatchUnwind<F> {
    type Output = Result<F::Output, Box<dyn Any + Send>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `inner` is never moved out of the pinned projection, and the
        // future is dropped in place along with this wrapper.
        let inner = unsafe { self.map_unchecked_mut(|this| &mut this.inner) };

        let was_in_request = IN_REQUEST.with(|flag| flag.replace(true));
        let result = catch_unwind(AssertUnwindSafe(|| inner.poll(cx)));
        IN_REQUEST.with(|flag| flag.set(was_in_request));

        match result {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

/// Run a future, converting a panic into an `Err`.
pub async fn catch<F: Future>(future: F) -> Result<F::Output, String> {
    CatchUnwind::new(future).await.map_err(|payload| message_of(payload.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panicking_future_is_caught() {
        install_hook();
        let caught = catch(async { panic!("boom") }).await;

        assert_eq!(caught.unwrap_err(), "boom");
        assert!(take_location().is_some());
    }

    #[tokio::test]
    async fn a_healthy_future_passes_through() {
        install_hook();
        let value = catch(async { 42 }).await;

        assert_eq!(value.unwrap(), 42);
    }

    #[tokio::test]
    async fn panics_across_await_points_are_caught() {
        install_hook();
        let caught = catch(async {
            tokio::task::yield_now().await;
            panic!("after yielding");
        })
        .await;

        assert_eq!(caught.unwrap_err(), "after yielding");
    }
}
