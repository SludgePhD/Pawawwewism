//! Primitives for reactive programming.
//!
//! [`Value`] encapsulates some value and adds the ability to listen for changes
//! of that value. It supports both blocking and `async` listeners.
//!
//! # Analogues
//!
//! A [`Value`] and its associated [`Reader`]s are comparable to a *broadcast
//! channel*, since any change to the [`Value`] will notify *every* [`Reader`].
//! However, there is an important difference: changes to the [`Value`] are
//! generally *coalesced*, meaning that [`Reader`]s are not guaranteed to see
//! intermediate values if the value is changed again before the [`Reader`] has
//! time to witness the old value.
//!
//! A closer analogue to [`Value`] and [`Reader`] would be tokio's `sync::watch`
//! module and its `Sender` and `Receiver`. The difference is that this
//! implementation supports both async and sync usage and is completely
//! independent of any async runtime.
//!
//! # Examples
//!
//! ```
//! use pawawwewism::reactive::Value;
//!
//! let mut value = Value::new(0);
//! let reader = value.reader();
//!
//! // A background thread performs calculations and publishes the results.
//! let bg = pawawwewism::background(move || for i in 1..=10 {
//!     value.set(i);
//! });
//!
//! // `Reader` can be iterated over, yielding changes to the value. Unlike a channel, `Reader` is
//! // not guaranteed to yield *every* value, only the most recent one.
//! let mut last = 0;
//! for value in reader {
//!     last = value;
//! }
//! assert_eq!(last, 10);
//! ```

use std::{
    future::Future,
    mem,
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    task::{Context, Poll, Waker},
};

/// A reactive value.
///
/// [`Value`] allows synchronous and asynchronous code to exchange a value of type `T` and notify
/// each other of changes of that data (via [`Condvar`]s and [`Waker`]s, respectively).
pub struct Value<T>(Arc<Shared<T>>);

/// A read-only handle to a reactive [`Value`].
///
/// [`Reader`] allows fetching the current value, checking whether the value has changed, and also
/// allows waiting for changes to the underlying value (either synchronously or asynchronously).
///
/// A [`Reader`] can be obtained from a [`Value`] by calling [`Value::reader`], or by cloning an
/// existing [`Reader`]. Many [`Reader`]s can listen for changes of the same [`Value`].
pub struct Reader<T> {
    shared: Arc<Shared<T>>,
    read_gen: u64,
    /// Set to `true` once this reader has produced a [`Disconnected`] error.
    read_disconnected: bool,
}
// FIXME: is `Listener` or `Subscriber` a better name?

struct Shared<T> {
    inner: Mutex<ValueInner<T>>,
    /// Condition variable to wake up all threads waiting for changes of this value.
    // FIXME: `Condvar::notify` is expensive (always a syscall), can we add an atomic flag indicating whether any threads are waiting?
    condvar: Condvar,

    reader_count: AtomicUsize,
}

struct ValueInner<T> {
    /// The actual value.
    value: T,

    /// Write generation counter. Incremented every time a new value is written.
    ///
    /// Value 0 is special-cased and means that the [`Value`] has been dropped.
    // FIXME: this could be an AtomicU64 outside the mutex instead (but we can only write to it while holding the mutex, to avoid races with readers)
    generation: u64,

    disconnected: bool,

    /// List of [`Waker`]s tied to tasks that are waiting for changes to this value.
    wakers: Vec<Waker>,
}

impl<T> Value<T> {
    /// Creates a reactive [`Value`] with an initial underlying value.
    pub fn new(value: T) -> Self {
        Self(Arc::new(Shared {
            inner: Mutex::new(ValueInner {
                value,
                generation: 0,
                disconnected: false,
                wakers: Vec::new(),
            }),
            condvar: Condvar::new(),
            reader_count: AtomicUsize::new(0),
        }))
    }

    /// Creates a [`Reader`] that will be notified of all future changes to the [`Value`].
    ///
    /// Any number of [`Reader`]s can be associated with the same [`Value`]. The current number of
    /// [`Reader`]s can be obtained via [`Value::reader_count`].
    pub fn reader(&self) -> Reader<T> {
        self.0.reader_count.fetch_add(1, Ordering::Relaxed);
        let gen = self.0.inner.lock().unwrap().generation;
        Reader {
            shared: self.0.clone(),
            read_gen: gen,
            read_disconnected: false, // we exist, therefore we cannot not exist anymore
        }
    }

    /// Returns the current number of [`Reader`]s that are observing this [`Value`] instance.
    #[inline]
    pub fn reader_count(&self) -> usize {
        self.0.reader_count.load(Ordering::Relaxed)
    }

    /// Returns a [`bool`] indicating whether this [`Value`] is observed by at least 1 [`Reader`]
    /// and can be usefully updated.
    ///
    /// Modifying a [`Value`] which is not being observed is often an undesirable waste of
    /// resources, since no [`Reader`] is there to consume the updated value.
    ///
    /// Note that there is currently no mechanism to be notified when a [`Reader`] for a specific
    /// [`Value`] is created, so an external mechanism to "reactivate" the owner of the [`Value`]
    /// must be used.
    #[inline]
    pub fn has_readers(&self) -> bool {
        self.reader_count() != 0
    }
    // FIXME: something like `pub async fn has_readers_signal` might be useful?

    /// Changes the underlying value and notifies all associated [`Reader`]s of the change.
    ///
    /// Returns the previous value.
    pub fn set(&mut self, new: T) -> T {
        self.mutate(|mut inner| mem::replace(&mut inner.value, new))
    }

    /// Modifies the underlying value using a caller-provided closure, and notifies all [`Reader`]s
    /// if the value is changed.
    ///
    /// The closure should complete quickly, since it blocks access to the contained value.
    ///
    /// The [`Reader`]s associated with this [`Value`] are only notified if the closure mutably
    /// dereferences the [`Mut`] reference. To unconditionally notify all [`Reader`]s, the closure
    /// should *always* assign a new value to the [`Mut`] (or use [`Value::set`] instead). The
    /// [`Mut`] type also allows customizing change detection to avoid unnecessary [`Reader`]
    /// wakeups.
    pub fn modify(&mut self, with: impl FnOnce(Mut<'_, T>)) {
        self.mutate(|inner| with(inner.map(|i| &mut i.value)));
    }

    fn mutate<R>(&mut self, with: impl FnOnce(Mut<'_, ValueInner<T>>) -> R) -> R {
        let mut inner = self.0.inner.lock().unwrap();
        let mut changed = false;
        let m = Mut::new(&mut *inner, &mut changed);
        let r = with(m);
        if changed {
            inner.generation += 1;
            inner.wakers.drain(..).for_each(Waker::wake);
            self.0.condvar.notify_all();
        }
        r
    }
}

impl<T> Drop for Value<T> {
    fn drop(&mut self) {
        // If the `Value` is dropped, we set the generation counter to the sentinel value indicating
        // that the `Value` has been discarded, and then wake up every waiting `Reader` so that they
        // can notify their owner of this.

        // FIXME: if we allow cloneable values this logic has to change to only decrement their number
        // (other code can then check for `disconnected` by comparing the counter with 0)
        let mut inner = self.0.inner.lock().unwrap();
        inner.disconnected = true;
        inner.wakers.drain(..).for_each(Waker::wake);
        self.0.condvar.notify_all();
    }
}

impl<T: Default> Default for Value<T> {
    fn default() -> Self {
        Value::new(T::default())
    }
}

impl<T> Reader<T> {
    /// Calls a closure with a reference to the contained value.
    ///
    /// The closure should complete quickly, since it blocks access to the contained value.
    ///
    /// This method is available for any type `T`. If `T` implements [`Clone`], consider using
    /// [`Reader::get`] instead, which will clone the value.
    pub fn with<R>(&mut self, f: impl FnOnce(&T) -> R) -> Result<R, Disconnected> {
        let guard = self.shared.inner.lock().unwrap();
        if guard.generation != self.read_gen {
            self.read_gen = guard.generation;
            return Ok(f(&guard.value));
        }
        if guard.disconnected {
            self.read_disconnected = true;
            return Err(Disconnected);
        }
        Ok(f(&guard.value))
    }

    /// Returns a [`bool`] indicating whether changes to the [`Value`] have been made that this
    /// [`Reader`] hasn't seen yet.
    ///
    /// The act of dropping the [`Value`] is also treated as a "change" and will make this method
    /// return `true` until any [`Reader`] method is called that accesses the value (which will now
    /// return `Err(Disconnected)`).
    ///
    /// This method is the recommended way to perform polling on a [`Reader`] (for example, to check
    /// for new data on every iteration of a continuous rendering loop).
    pub fn is_changed(&self) -> bool {
        let guard = self.shared.inner.lock().unwrap();
        guard.generation != self.read_gen || guard.disconnected != self.read_disconnected
    }

    /// Returns a [`bool`] indicating whether the underlying [`Value`] has been dropped.
    ///
    /// If this returns `true`, all methods that access the underlying value will fail with a
    /// [`Disconnected`] error.
    pub fn is_disconnected(&self) -> bool {
        self.shared.inner.lock().unwrap().disconnected
    }
}

impl<T: Clone> Reader<T> {
    /// Retrieves the current value.
    ///
    /// If the associated [`Value`] has been dropped, a [`Disconnected`] error is returned instead.
    pub fn get(&mut self) -> Result<T, Disconnected> {
        self.with(T::clone)
    }

    /// Blocks the calling thread until the underlying value changes, and returns the new value.
    ///
    /// If the [`Value`] has been dropped, or is dropped while blocking, this will return a
    /// [`Disconnected`] error instead.
    pub fn block(&mut self) -> Result<T, Disconnected> {
        let mut guard = self.shared.inner.lock().unwrap();
        loop {
            if guard.generation != self.read_gen {
                self.read_gen = guard.generation;
                return Ok(guard.value.clone());
            }
            if guard.disconnected {
                self.read_disconnected = true;
                return Err(Disconnected);
            }
            guard = self.shared.condvar.wait(guard).unwrap();
        }
    }

    /// Asynchronously waits for the [`Value`] to change, and returns the new value.
    ///
    /// If the [`Value`] has been dropped, or is dropped while waiting, this will return a
    /// [`Disconnected`] error instead.
    pub async fn wait(&mut self) -> Result<T, Disconnected> {
        struct Waiter<'a, T>(&'a mut Reader<T>);

        impl<'a, T: Clone> Future for Waiter<'a, T> {
            type Output = Result<T, Disconnected>;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let mut guard = self.0.shared.inner.lock().unwrap();
                if guard.generation != self.0.read_gen {
                    let value = guard.value.clone();
                    let generation = guard.generation;
                    drop(guard);
                    self.0.read_gen = generation;
                    return Poll::Ready(Ok(value));
                }
                if guard.disconnected {
                    drop(guard);
                    self.0.read_disconnected = true;
                    return Poll::Ready(Err(Disconnected));
                }

                guard.wakers.push(cx.waker().clone());
                Poll::Pending
            }
        }

        Waiter(self).await
    }
}

impl<T> Clone for Reader<T> {
    fn clone(&self) -> Self {
        self.shared.reader_count.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: self.shared.clone(),
            read_gen: self.read_gen,
            read_disconnected: self.read_disconnected,
        }
    }
}

impl<T> Drop for Reader<T> {
    #[inline]
    fn drop(&mut self) {
        self.shared.reader_count.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Just like a channel, a [`Reader`] can be iterated over, yielding the changes made to the
/// underlying [`Value`].
///
/// The iterator stops when the [`Value`] is dropped and the [`Reader`] becomes disconnected.
impl<T: Clone> IntoIterator for Reader<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    #[inline]
    fn into_iter(self) -> IntoIter<T> {
        IntoIter { reader: self }
    }
}

/// A blocking [`Iterator`] over the values visible to a [`Reader`].
///
/// Every call to [`IntoIter::next`] will block until the underlying [`Value`] is changed.
pub struct IntoIter<T: Clone> {
    reader: Reader<T>,
}

impl<T: Clone> Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.reader.block().ok()
    }
}

/// An error value returned by [`Reader`] when the underlying [`Value`] has been dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disconnected;

/// A mutable reference that tracks whether the pointee has been modified.
///
/// This implements [`Deref`] and [`DerefMut`] to allow access to the inner value. If the [`Mut`] is
/// mutably dereferenced, the contained value will be marked as changed. The value can be manually
/// marked or unmarked as changed by calling [`Mut::set_changed`].
pub struct Mut<'a, T> {
    t: &'a mut T,
    changed: &'a mut bool,
}

impl<'a, T> Mut<'a, T> {
    fn new(t: &'a mut T, changed: &'a mut bool) -> Self {
        Self { t, changed }
    }

    fn map<U>(self, f: impl FnOnce(&'a mut T) -> &'a mut U) -> Mut<'a, U> {
        let u = f(self.t);
        Mut {
            t: u,
            changed: self.changed,
        }
    }

    /// Sets whether the value should be considered to have changed.
    ///
    /// This method can be used to override the automatic change tracking. It should be used
    /// sparingly, since incorrectly marking a value as *unchanged* when it *did* change can result
    /// in incorrect and surprising behavior (or lack of behavior).
    #[inline]
    pub fn set_changed(&mut self, changed: bool) {
        *self.changed = changed;
    }
}

impl<'a, T> Deref for Mut<'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.t
    }
}

impl<'a, T> DerefMut for Mut<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        *self.changed = true;
        self.t
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::pin, task::Wake, thread};

    use crate::{background, test::block_on};

    use super::*;

    /// Polls a future exactly once, asserting that the future is ready, and returns the produced
    /// result.
    fn assert_ready<R>(fut: impl Future<Output = R>) -> R {
        struct NoopWaker;
        impl Wake for NoopWaker {
            fn wake(self: Arc<Self>) {}
        }

        let waker = Waker::from(Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);

        let fut = pin!(fut);
        match fut.poll(&mut cx) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("assert_ready: future is not ready"),
        }
    }

    #[test]
    fn simple() {
        let mut value = Value::new(0);
        assert_eq!(value.reader_count(), 0);

        let mut reader = value.reader();
        assert_eq!(value.reader_count(), 1);
        drop(reader.clone());
        assert_eq!(value.reader_count(), 1);

        // As long as `value` is in scope, the `reader` should not be disconnected.
        assert!(!reader.is_disconnected());
        // Since `value` wasn't written to since `reader` was created, `is_changed()` is `false`...
        assert_eq!(reader.is_changed(), false);
        // ...but `get` will return the old value.
        assert_eq!(reader.get(), Ok(0));

        // If `value` is changed, `reader.next()` will succeed.
        value.set(123);
        assert_eq!(reader.is_changed(), true);
        assert_eq!(reader.get(), Ok(123));
        assert_eq!(reader.is_changed(), false);

        // If `value` is changed, `reader.block()` will return immediately.
        value.set(456);
        assert_eq!(reader.is_changed(), true);
        assert_eq!(reader.block(), Ok(456));
        assert_eq!(reader.is_changed(), false);

        // If `value` is changed, `reader.wait()` will be `Ready` immediately.
        value.set(789);
        assert_eq!(reader.is_changed(), true);
        assert_eq!(assert_ready(reader.wait()), Ok(789));
        assert_eq!(reader.is_changed(), false);

        // If `value` is dropped, the value is marked as changed, and all attempts to read it will
        // return a `Disconnected` error.
        assert_eq!(reader.is_disconnected(), false);
        drop(value);
        assert_eq!(reader.is_changed(), true);
        assert_eq!(reader.is_disconnected(), true);
        assert_eq!(reader.get(), Err(Disconnected));
        assert_eq!(reader.is_changed(), false);
        assert_eq!(reader.block(), Err(Disconnected));
        assert_eq!(assert_ready(reader.wait()), Err(Disconnected));
    }

    /// Tests that updating a value in a different thread works, and we can block on the `Reader` in
    /// another thread to obtain the value.
    #[test]
    fn block() {
        let mut value = Value::new(0);
        let mut reader = value.reader();
        let bg = background(move || {
            value.set(123);
            thread::park();
        });

        assert_eq!(reader.block(), Ok(123));
        assert_eq!(reader.is_changed(), false);
        bg.thread().unpark();
        bg.join();
        assert_eq!(reader.is_changed(), true);
        assert_eq!(reader.get(), Err(Disconnected));
        assert_eq!(reader.is_changed(), false);
    }

    #[test]
    fn wait() {
        let mut value = Value::new(0);
        let mut reader = value.reader();
        let bg = background(move || {
            value.set(123);
            thread::park();
        });

        assert_eq!(block_on(reader.wait()), Ok(123));
        assert_eq!(reader.is_changed(), false);
        bg.thread().unpark();
        bg.join();
        assert_eq!(reader.is_changed(), true);
        assert_eq!(reader.get(), Err(Disconnected));
        assert_eq!(reader.is_changed(), false);
    }

    /// Tests that `Reader` can be cloned, and both clones will be notified of subsequent changes.
    #[test]
    fn clone_reader() {
        let mut value = Value::new(0);
        let mut r1 = value.reader();

        assert_eq!(r1.is_changed(), false);
        value.set(123);
        let mut r2 = r1.clone();

        assert_eq!(r1.is_changed(), true);
        assert_eq!(r1.get(), Ok(123));
        assert_eq!(r1.is_changed(), false);

        assert_eq!(r2.is_changed(), true);
        assert_eq!(r2.get(), Ok(123));
        assert_eq!(r2.is_changed(), false);
    }

    /// Tests that [`Reader`]s will yield that last value written through [`Value`] if it hasn't
    /// been seen yet, even if the [`Value`] has been dropped since.
    #[test]
    fn reader_returned_last_value_written() {
        let mut value = Value::new(0);
        let mut reader = value.reader();

        value.set(123);
        drop(value);

        assert_eq!(reader.block(), Ok(123));
        assert_eq!(reader.block(), Err(Disconnected));
    }
}
