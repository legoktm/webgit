use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// A one-shot signal that some piece of work has finished, for de-duplicating
/// concurrent reads: the first reader does the work and others wait on this,
/// then look again for what it produced.
///
/// Clones share one signal. Single-threaded, like the rest of the IO here.
#[derive(Clone, Default)]
pub struct Completion(Rc<CompletionState>);

#[derive(Default)]
struct CompletionState {
    done: Cell<bool>,
    wakers: RefCell<Vec<Waker>>,
}

impl Completion {
    /// A signal that has not fired yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fire the signal, waking everything waiting on it. Firing it again does
    /// nothing.
    pub fn complete(&self) {
        self.0.done.set(true);
        for waker in self.0.wakers.take() {
            waker.wake();
        }
    }

    /// Resolve once the signal has fired.
    pub fn wait(&self) -> impl Future<Output = ()> + use<> {
        WaitForCompletion(self.0.clone())
    }

    /// Whether `self` and `other` are the same signal.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

struct WaitForCompletion(Rc<CompletionState>);

impl Future for WaitForCompletion {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0.done.get() {
            return Poll::Ready(());
        }
        self.0.wakers.borrow_mut().push(cx.waker().clone());
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use futures::future::join;

    #[test]
    fn waiters_resume_once_completed() {
        let done = Completion::new();
        let order = RefCell::new(Vec::new());
        block_on(join(
            async {
                done.wait().await;
                order.borrow_mut().push("waiter");
            },
            async {
                order.borrow_mut().push("worker");
                done.complete();
            },
        ));
        assert_eq!(*order.borrow(), ["worker", "waiter"]);
        // Already complete: resolves at once.
        block_on(done.wait());
    }
}
