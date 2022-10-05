//! A simple library for structured concurrency and heterogeneous thread-based parallel processing.
//!
//! (if you're looking for homogeneous parallel processing using an iterator-like interface, check
//! out [`rayon`] instead; if you're looking for running large numbers of I/O tasks concurrently
//! instead of running a small number of computationally expensive tasks concurrently, you're
//! probably better served by an `async` runtime)
//!
//! # Overview
//!
//! This library features two main types: [`Worker`] and [`Promise`]. They can be combined and
//! chained to create arbitrary computation graphs involving different types of data.
//!
//! ## Workers
//!
//! [`Worker`] is a wrapper around an OS-level thread that enforces *structured concurrency*: the
//! idea that concurrent operations should be structured just like other control flow constructs.
//! When the [`Worker`] is dropped, the underlying thread will be signaled to exit and then joined.
//! If the thread has panicked, the panic will be forwarded to the thread dropping the [`Worker`].
//!
//! These "owned threads" ensure that no stale threads will linger around after a concurrent
//! operation is done using them. Forwarding the [`Worker`]s panic ensures that the code that
//! started the computation (by spawning the [`Worker`]) will be torn down properly, as if it had
//! performed the computation directly rather than spawning a [`Worker`] to do it.
//!
//! [`Worker`]s use a message-driven interface, similar to actors. Instead of using a user-written
//! processing loop, they are sent messages of some user-defined type. This encourages thinking of
//! code that uses [`Worker`]s as a data processing pipeline: the code that spawns the [`Worker`]
//! needs to submit input data to it, which can then get transformed and passed somewhere else.
//!
//! ## Promises
//!
//! [`Promise`] provides a mechanism for communicating the result of a computation back to the code
//! that started it, or to the next part of the processing pipeline. Once a computation has
//! finished, its result can be submitted via [`Promise::fulfill`], and the thread holding the
//! corresponding [`PromiseHandle`] can retrieve it.
//!
//! # Usage
//!
//! A single [`Worker`] that communicates its result back using a [`Promise`]:
//!
//! ```
//! use pawawwewism::{Worker, Promise, promise};
//!
//! let mut worker = Worker::builder().spawn(|(input, promise): (i32, Promise<i32>)| {
//!     println!("Doing heavy task...");
//!     let output = input + 1;
//!     promise.fulfill(output);
//! }).unwrap();
//!
//! let (promise, handle) = promise();
//! worker.send((1, promise));
//!
//! // <do other work concurrently>
//!
//! let output = handle.block().expect("worker has dropped the promise; this should be impossible");
//! assert_eq!(output, 2);
//! ```
//!
//! Multiple [`Worker`] threads can be chained to pipeline a computation:
//!
//! ```
//! use std::collections::VecDeque;
//! use pawawwewism::{Worker, Promise, PromiseHandle, promise};
//!
//! // This worker is identical to the one in the first example
//! let mut worker1 = Worker::builder().spawn(|(input, promise): (i32, Promise<i32>)| {
//!     println!("Doing heavy task 1...");
//!     let output = input + 1;
//!     promise.fulfill(output);
//! }).unwrap();
//!
//! // The second worker is passed a `PromiseHandle` instead of a direct value
//! let mut next = 1;
//! let mut worker2 = Worker::builder().spawn(move |handle: PromiseHandle<i32>| {
//!     let input = handle.block().unwrap();
//!     assert_eq!(input, next);
//!     next += 1;
//! }).unwrap();
//!
//! for input in [0,1,2,3] {
//!     let (promise1, handle1) = promise();
//!     worker1.send((input, promise1));
//!     // On the second iteration and later, this `send` will give `worker1` work to do, while
//!     // `worker2` still processes the previous element, achieving pipelining.
//!
//!     worker2.send(handle1);
//! }
//! ```
//!
//! [`rayon`]: https://crates.io/crates/rayon

mod drop;

use std::{
    io, mem,
    panic::resume_unwind,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};

use crossbeam_channel::Sender;

use crate::drop::defer;

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

/// A builder object that can be used to configure and spawn a [`Worker`].
#[derive(Clone)]
pub struct WorkerBuilder {
    name: Option<String>,
    capacity: usize,
}

impl WorkerBuilder {
    /// Sets the name of the [`Worker`] thread.
    pub fn name<N: Into<String>>(self, name: N) -> Self {
        Self {
            name: Some(name.into()),
            ..self
        }
    }

    /// Sets the channel capacity of the [`Worker`].
    ///
    /// By default, a capacity of 0 is used, which means that [`Worker::send`] will block until the
    /// worker has finished processing any preceding message.
    ///
    /// When a pipeline of [`Worker`]s is used, the capacity of later [`Worker`]s may be increased
    /// to allow the processing of multiple input messages at once.
    #[inline]
    pub fn capacity(self, capacity: usize) -> Self {
        Self { capacity, ..self }
    }

    /// Spawns a [`Worker`] thread that uses `handler` to process incoming messages.
    pub fn spawn<I, F>(self, mut handler: F) -> io::Result<Worker<I>>
    where
        I: Send + 'static,
        F: FnMut(I) + Send + 'static,
    {
        let (sender, recv) = crossbeam_channel::bounded(self.capacity);
        let mut builder = thread::Builder::new();
        if let Some(name) = self.name.clone() {
            builder = builder.name(name);
        }
        let handle = builder.spawn(move || {
            let _guard;
            if let Some(name) = self.name {
                log::trace!("worker '{name}' starting");
                _guard = defer(move || log::trace!("worker '{name}' exiting"));
            }
            for message in recv {
                handler(message);
            }
        })?;

        Ok(Worker {
            sender: Some(sender),
            handle: Some(handle),
        })
    }
}

/// A handle to a worker thread that processes messages of type `I`.
///
/// This type enforces structured concurrency: When it's dropped, the thread will be signaled to
/// exit and the thread will be joined. If the thread has panicked, the panic will be forwarded
/// to the thread dropping the [`Worker`].
pub struct Worker<I: Send + 'static> {
    sender: Option<Sender<I>>,
    handle: Option<JoinHandle<()>>,
}

impl<I: Send + 'static> Drop for Worker<I> {
    fn drop(&mut self) {
        // Close the channel to signal the thread to exit.
        drop(self.sender.take());

        self.wait_for_exit();
    }
}

impl Worker<()> {
    /// Returns a builder that can be used to configure and spawn a [`Worker`].
    #[inline]
    pub fn builder() -> WorkerBuilder {
        WorkerBuilder {
            name: None,
            capacity: 0,
        }
    }
}

impl<I: Send + 'static> Worker<I> {
    fn wait_for_exit(&mut self) {
        // Wait for it to exit and propagate its panic if it panicked.
        if let Some(handle) = self.handle.take() {
            match handle.join() {
                Ok(()) => {}
                Err(payload) => {
                    if !thread::panicking() {
                        resume_unwind(payload);
                    }
                }
            }
        }
    }

    /// Sends a message to the worker thread.
    ///
    /// If the worker's channel capacity is exhausted, this will block until the worker is available
    /// to accept the message.
    ///
    /// If the worker has panicked, this will propagate the panic to the calling thread.
    pub fn send(&mut self, msg: I) {
        match self.sender.as_ref().unwrap().send(msg) {
            Ok(()) => {}
            Err(_) => {
                self.wait_for_exit();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use super::*;

    fn assert_send<T: Send>() {}

    fn silent_panic(payload: String) {
        resume_unwind(Box::new(payload));
    }

    #[test]
    fn worker_propagates_panic_on_drop() {
        let mut worker = Worker::builder()
            .spawn(|_: ()| silent_panic("worker panic".into()))
            .unwrap();
        worker.send(());
        catch_unwind(AssertUnwindSafe(|| drop(worker))).unwrap_err();
    }

    #[test]
    fn worker_propagates_panic_on_send() {
        let mut worker = Worker::builder()
            .spawn(|_| silent_panic("worker panic".into()))
            .unwrap();
        worker.send(());
        catch_unwind(AssertUnwindSafe(|| worker.send(()))).unwrap_err();
        catch_unwind(AssertUnwindSafe(|| drop(worker))).unwrap();
    }

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

    #[test]
    fn worker_is_send() {
        assert_send::<Worker<()>>();
    }
}
