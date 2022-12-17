use std::{
    mem,
    sync::{Arc, Condvar, Mutex},
};

/// Creates a connected pair of [`Promise`] and [`PromiseHandle`].
pub fn promise<T>() -> (Promise<T>, PromiseHandle<T>) {
    let inner = Arc::new(PromiseInner {
        state: Mutex::new(PromiseState::Empty),
        condvar: Condvar::new(),
    });
    (
        Promise {
            inner: inner.clone(),
            fulfilled: false,
        },
        PromiseHandle { inner },
    )
}

enum PromiseState<T> {
    Empty,
    Fulfilled(T),
    Dropped,
}

struct PromiseInner<T> {
    state: Mutex<PromiseState<T>>,
    condvar: Condvar,
}

/// An empty slot that can be filled with a `T`, fulfilling the promise.
///
/// Fulfilling a [`Promise`] lets the connected [`PromiseHandle`] retrieve the value. A connected
/// pair of [`Promise`] and [`PromiseHandle`] can be created by calling [`promise`].
pub struct Promise<T> {
    inner: Arc<PromiseInner<T>>,
    fulfilled: bool,
}

impl<T> Drop for Promise<T> {
    fn drop(&mut self) {
        if self.fulfilled {
            // No need to lock or notify again
            return;
        }

        *self.inner.state.lock().unwrap() = PromiseState::Dropped;
        self.inner.condvar.notify_one();
    }
}

impl<T> Promise<T> {
    /// Fulfills the promise with a value, consuming it.
    ///
    /// If a thread is currently waiting at [`PromiseHandle::block`], it will be woken up.
    ///
    /// This method does not block or fail. If the connected [`PromiseHandle`] was dropped, `value`
    /// will be dropped and nothing happens. The calling thread is expected to exit when it attempts
    /// to obtain a new [`Promise`] to fulfill.
    pub fn fulfill(mut self, value: T) {
        // This ignores errors. The assumption is that the thread will exit once it tries to obtain
        // a new `Promise` to fulfill.
        *self.inner.state.lock().unwrap() = PromiseState::Fulfilled(value);
        self.inner.condvar.notify_one();
        self.fulfilled = true;
    }
}

/// A handle connected to a [`Promise`] that will eventually resolve to a value of type `T`.
///
/// A connected pair of [`Promise`] and [`PromiseHandle`] can be created by calling [`promise`].
pub struct PromiseHandle<T> {
    inner: Arc<PromiseInner<T>>,
}

impl<T> PromiseHandle<T> {
    /// Blocks the calling thread until the connected [`Promise`] is fulfilled.
    ///
    /// If the [`Promise`] is dropped without being fulfilled, a [`PromiseDropped`] error is
    /// returned instead. This typically means one of two things:
    ///
    /// - The thread holding the promise has deliberately decided not to fulfill it (for
    ///   example, because it has skipped processing an item).
    /// - The thread holding the promise has panicked.
    ///
    /// Usually, the correct way to handle this is to just skip the item expected from the
    /// [`Promise`]. If the thread has panicked, and it's a [`Worker`] thread, then the next
    /// attempt to send a message to it will propagate the panic to the owning thread, and tear
    /// down the process as usual.
    pub fn block(self) -> Result<T, PromiseDropped> {
        let mut state = self.inner.state.lock().unwrap();
        loop {
            match *state {
                PromiseState::Empty => state = self.inner.condvar.wait(state).unwrap(),
                PromiseState::Fulfilled(_) => {
                    let fulfilled = mem::replace(&mut *state, PromiseState::Empty);
                    match fulfilled {
                        PromiseState::Fulfilled(value) => return Ok(value),
                        PromiseState::Empty | PromiseState::Dropped => unreachable!(),
                    }
                }
                PromiseState::Dropped => return Err(PromiseDropped { _priv: () }),
            }
        }
    }

    /// Tests whether a call to [`PromiseHandle::block`] will block or return immediately.
    ///
    /// If this returns `false`, calling [`PromiseHandle::block`] on `self` will return immediately,
    /// without blocking.
    pub fn will_block(&self) -> bool {
        // If the `Promise` is dropped, it will decrement the refcount, so if that's not 2 we know
        // that we won't block on anything.
        Arc::strong_count(&self.inner) == 2
    }
}

/// An error returned by [`PromiseHandle::block`] indicating that the connected [`Promise`] object
/// was dropped without being fulfilled.
#[derive(Debug, Clone)]
pub struct PromiseDropped {
    _priv: (),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>() {}

    #[test]
    fn promise_fulfillment() {
        let (promise, handle) = promise();
        assert!(handle.will_block());
        promise.fulfill(());
        assert!(!handle.will_block());
        handle.block().unwrap();
    }

    #[test]
    fn promise_drop() {
        let (promise, handle) = promise::<()>();
        assert!(handle.will_block());
        drop(promise);
        assert!(!handle.will_block());
        handle.block().unwrap_err();
    }

    #[test]
    fn promise_is_send() {
        assert_send::<Promise<()>>();
        assert_send::<PromiseHandle<()>>();
    }
}
