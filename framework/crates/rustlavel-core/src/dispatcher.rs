//! Application events and listeners.
//!
//! Distinct from [`crate::events`], which is the framework's own
//! instrumentation bus: that one is untyped, so a debugging tool can observe
//! anything. This one is typed, so `dispatch(OrderShipped { .. })` reaches
//! exactly the listeners written for `OrderShipped`, and a listener that reads
//! the wrong field does not compile.
//!
//! ```ignore
//! struct OrderShipped { id: i64 }
//! impl AppEvent for OrderShipped {}
//!
//! dispatcher.listen(|event: &OrderShipped| async move {
//!     send_tracking_email(event.id).await
//! });
//!
//! dispatcher.dispatch(&OrderShipped { id: 7 }).await;
//! ```

use crate::Result;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

/// Anything that can be dispatched. A marker: the type itself is the identity.
pub trait AppEvent: Send + Sync + 'static {}

type BoxFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// A registered listener, erased so listeners for different events can share
/// one map, and downcast back on dispatch.
type ErasedListener = Arc<dyn Fn(&(dyn Any + Send + Sync)) -> BoxFuture<'_> + Send + Sync>;

/// Routes events to the listeners registered for their type.
#[derive(Clone, Default)]
pub struct Dispatcher {
    listeners: Arc<RwLock<HashMap<TypeId, Vec<ErasedListener>>>>,
}

impl Dispatcher {
    pub fn new() -> Self {
        Dispatcher::default()
    }

    /// Register a listener for one event type.
    ///
    /// Listeners run in registration order, and each is awaited before the next
    /// starts. A listener that should not block the dispatcher belongs on the
    /// queue instead — the framework will not decide that for you.
    pub fn listen<E, F, Fut>(&self, listener: F)
    where
        E: AppEvent,
        F: Fn(&E) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let erased: ErasedListener = Arc::new(move |event: &(dyn Any + Send + Sync)| {
            let typed = event
                .downcast_ref::<E>()
                .expect("the dispatcher only calls a listener with its own event type");
            Box::pin(listener(typed)) as BoxFuture<'_>
        });

        self.listeners
            .write()
            .expect("dispatcher lock poisoned")
            .entry(TypeId::of::<E>())
            .or_default()
            .push(erased);
    }

    /// Send an event to its listeners.
    ///
    /// The first failure stops the chain and is returned, because a listener
    /// that fails usually means the ones after it should not run either.
    pub async fn dispatch<E: AppEvent>(&self, event: &E) -> Result<()> {
        for listener in self.listeners_for::<E>() {
            listener(event as &(dyn Any + Send + Sync)).await?;
        }
        Ok(())
    }

    /// Send an event, running every listener even if one fails.
    ///
    /// Returns the errors that occurred. Use this when listeners are
    /// independent — one failing mailer should not stop an audit log.
    pub async fn dispatch_all<E: AppEvent>(&self, event: &E) -> Vec<crate::Error> {
        let mut failures = Vec::new();

        for listener in self.listeners_for::<E>() {
            if let Err(error) = listener(event as &(dyn Any + Send + Sync)).await {
                failures.push(error);
            }
        }
        failures
    }

    pub fn listener_count<E: AppEvent>(&self) -> usize {
        self.listeners_for::<E>().len()
    }

    /// Remove every listener. Intended for tests.
    pub fn clear(&self) {
        self.listeners.write().expect("dispatcher lock poisoned").clear();
    }

    /// Take a snapshot of the listeners, so dispatching does not hold the lock
    /// while awaiting — a listener that dispatches would otherwise deadlock.
    fn listeners_for<E: AppEvent>(&self) -> Vec<ErasedListener> {
        self.listeners
            .read()
            .expect("dispatcher lock poisoned")
            .get(&TypeId::of::<E>())
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct OrderShipped {
        id: i64,
    }
    impl AppEvent for OrderShipped {}

    struct UserRegistered;
    impl AppEvent for UserRegistered {}

    /// A minimal executor, so core stays free of an async runtime dependency.
    fn block_on<F: Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        fn noop(_: *const ()) {}
        fn clone(data: *const ()) -> RawWaker {
            RawWaker::new(data, &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);

        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut context = Context::from_waker(&waker);
        let mut future = Box::pin(future);

        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                // Every listener in these tests is ready immediately.
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn a_listener_receives_its_own_event() {
        let dispatcher = Dispatcher::new();
        let seen = Arc::new(Mutex::new(Vec::new()));

        let sink = Arc::clone(&seen);
        dispatcher.listen(move |event: &OrderShipped| {
            let sink = Arc::clone(&sink);
            let id = event.id;
            async move {
                sink.lock().unwrap().push(id);
                Ok(())
            }
        });

        block_on(dispatcher.dispatch(&OrderShipped { id: 7 })).unwrap();

        assert_eq!(*seen.lock().unwrap(), vec![7]);
    }

    #[test]
    fn events_reach_only_their_own_listeners() {
        let dispatcher = Dispatcher::new();
        let orders = Arc::new(AtomicUsize::new(0));
        let users = Arc::new(AtomicUsize::new(0));

        let counter = Arc::clone(&orders);
        dispatcher.listen(move |_: &OrderShipped| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });

        let counter = Arc::clone(&users);
        dispatcher.listen(move |_: &UserRegistered| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });

        block_on(dispatcher.dispatch(&OrderShipped { id: 1 })).unwrap();

        assert_eq!(orders.load(Ordering::SeqCst), 1);
        assert_eq!(users.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn listeners_run_in_registration_order() {
        let dispatcher = Dispatcher::new();
        let order = Arc::new(Mutex::new(Vec::new()));

        for label in ["first", "second", "third"] {
            let sink = Arc::clone(&order);
            dispatcher.listen(move |_: &UserRegistered| {
                let sink = Arc::clone(&sink);
                async move {
                    sink.lock().unwrap().push(label);
                    Ok(())
                }
            });
        }

        block_on(dispatcher.dispatch(&UserRegistered)).unwrap();

        assert_eq!(*order.lock().unwrap(), ["first", "second", "third"]);
    }

    #[test]
    fn dispatch_stops_at_the_first_failure() {
        let dispatcher = Dispatcher::new();
        let reached = Arc::new(AtomicUsize::new(0));

        dispatcher.listen(|_: &UserRegistered| async { Err(crate::Error::msg("no")) });

        let counter = Arc::clone(&reached);
        dispatcher.listen(move |_: &UserRegistered| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });

        assert!(block_on(dispatcher.dispatch(&UserRegistered)).is_err());
        assert_eq!(reached.load(Ordering::SeqCst), 0, "the second listener should not have run");
    }

    #[test]
    fn dispatch_all_runs_everything_and_collects_failures() {
        let dispatcher = Dispatcher::new();
        let reached = Arc::new(AtomicUsize::new(0));

        dispatcher.listen(|_: &UserRegistered| async { Err(crate::Error::msg("mailer down")) });

        let counter = Arc::clone(&reached);
        dispatcher.listen(move |_: &UserRegistered| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });

        let failures = block_on(dispatcher.dispatch_all(&UserRegistered));

        assert_eq!(failures.len(), 1);
        assert_eq!(reached.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_event_with_no_listeners_is_not_an_error() {
        let dispatcher = Dispatcher::new();

        assert_eq!(dispatcher.listener_count::<OrderShipped>(), 0);
        assert!(block_on(dispatcher.dispatch(&OrderShipped { id: 1 })).is_ok());
    }
}
