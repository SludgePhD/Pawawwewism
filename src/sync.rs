//! A mirror of [`std::sync`] without lock poisoning.
//!
//! The [`std::sync::Mutex`] and [`std::sync::RwLock`] types "poison" themselves when they are
//! locked while the thread holding the lock panics. Attempting to lock a poisoned `Mutex` or
//! `RwLock` would result in an error that is typically handled by unwrapping it. This was meant to
//! result in all interacting threads subsequently propagating the error by panicking.
//!
//! Lock poisoning is regarded by many as a misfeature. Those people fall into two camps: those that
//! argue that locking a poisoning mutex should panic *immediately*, rather than requiring the user
//! to `unwrap` to get a panic, and those that argue that poisoning should be removed entirely and
//! panics should not be propagated using this mechanism.
//!
//! This library's structured concurrency primitives already provide their own ways of propagating
//! panics to the owner of a concurrent operation, so the poisoning mechanism is redundant. It can
//! even interfere with panic propagation: if multiple panics happen, it is generally not possible
//! to guarantee that the *first* one will be propagated to the owner, since there is no global
//! ordering between them. Since the first panic is generally the root cause, while the subsequent
//! knock-on panics are `unwrap`s of poisoned locks, this can result in the root cause *not* being
//! propagated to the owner.
//!
//! (unstructured programs are susceptible to the same problem, they just generally don't bother
//! with cleanly propagating panics)
//!
//! Hence, this module provides locking primitives that do not poison themselves in that way.
//!
//! Note that this module contains *low-level* primitives like [`Mutex`] and [`RwLock`] that can
//! *still* be easily misused, leading to deadlocks and race conditions. Prefer using some of the
//! higher-level constructs in this crate, such as [`reactive::Value`][crate::reactive::Value] and
//! [`Promise`][crate::Promise].

use std::{
    error::Error,
    fmt,
    ops::{Deref, DerefMut},
    sync,
    time::Duration,
};

pub use std::sync::WaitTimeoutResult;

pub type TryLockResult<Guard> = Result<Guard, TryLockError>;

#[derive(Default)]
pub struct Mutex<T: ?Sized> {
    inner: sync::Mutex<T>,
}

impl<T> Mutex<T> {
    pub const fn new(t: T) -> Mutex<T> {
        Self {
            inner: sync::Mutex::new(t),
        }
    }
}

impl<T: ?Sized> Mutex<T> {
    pub fn lock(&self) -> MutexGuard<'_, T> {
        let guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        };

        MutexGuard { inner: guard }
    }

    pub fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
        let guard = match self.inner.try_lock() {
            Ok(guard) => guard,
            Err(sync::TryLockError::Poisoned(poison)) => poison.into_inner(),
            Err(sync::TryLockError::WouldBlock) => return Err(TryLockError),
        };

        Ok(MutexGuard { inner: guard })
    }

    pub fn into_inner(self) -> T
    where
        T: Sized,
    {
        match self.inner.into_inner() {
            Ok(inner) => inner,
            Err(poison) => poison.into_inner(),
        }
    }

    pub fn get_mut(&mut self) -> &mut T {
        match self.inner.get_mut() {
            Ok(t) => t,
            Err(poison) => poison.into_inner(),
        }
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("Mutex");
        match self.try_lock() {
            Ok(val) => s.field("data", &&*val),
            Err(TryLockError) => s.field("data", &"<locked>"),
        }
        .finish_non_exhaustive()
    }
}

impl<T> From<T> for Mutex<T> {
    fn from(value: T) -> Self {
        Self {
            inner: value.into(),
        }
    }
}

#[derive(Debug)]
pub struct MutexGuard<'a, T: ?Sized + 'a> {
    inner: sync::MutexGuard<'a, T>,
}

impl<'a, T: ?Sized + 'a> Deref for MutexGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'a, T: ?Sized + 'a> DerefMut for MutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<'a, T: ?Sized + fmt::Display + 'a> fmt::Display for MutexGuard<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct TryLockError;

impl Error for TryLockError {}

impl fmt::Display for TryLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("`try_lock` failed because the operation would block")
    }
}

#[derive(Debug, Default)]
pub struct Condvar {
    inner: sync::Condvar,
}

impl Condvar {
    pub const fn new() -> Condvar {
        Self {
            inner: sync::Condvar::new(),
        }
    }

    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        let guard = match self.inner.wait(guard.inner) {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        };
        MutexGuard { inner: guard }
    }

    pub fn wait_while<'a, T, F>(&self, guard: MutexGuard<'a, T>, condition: F) -> MutexGuard<'a, T>
    where
        F: FnMut(&mut T) -> bool,
    {
        let guard = match self.inner.wait_while(guard.inner, condition) {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        };
        MutexGuard { inner: guard }
    }

    pub fn wait_timeout<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        dur: Duration,
    ) -> (MutexGuard<'a, T>, WaitTimeoutResult) {
        let (guard, timeout) = match self.inner.wait_timeout(guard.inner, dur) {
            Ok(out) => out,
            Err(poison) => poison.into_inner(),
        };
        (MutexGuard { inner: guard }, timeout)
    }

    pub fn wait_timeout_while<'a, T, F>(
        &self,
        guard: MutexGuard<'a, T>,
        dur: Duration,
        condition: F,
    ) -> (MutexGuard<'a, T>, WaitTimeoutResult)
    where
        F: FnMut(&mut T) -> bool,
    {
        let (guard, timeout) = match self.inner.wait_timeout_while(guard.inner, dur, condition) {
            Ok(out) => out,
            Err(poison) => poison.into_inner(),
        };
        (MutexGuard { inner: guard }, timeout)
    }

    pub fn notify_one(&self) {
        self.inner.notify_one();
    }

    pub fn notify_all(&self) {
        self.inner.notify_all();
    }
}

#[derive(Default)]
pub struct RwLock<T: ?Sized> {
    inner: sync::RwLock<T>,
}

impl<T> RwLock<T> {
    pub const fn new(t: T) -> RwLock<T> {
        Self {
            inner: sync::RwLock::new(t),
        }
    }
}

impl<T: ?Sized> RwLock<T> {
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        let guard = match self.inner.read() {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        };
        RwLockReadGuard { inner: guard }
    }

    pub fn try_read(&self) -> TryLockResult<RwLockReadGuard<'_, T>> {
        let guard = match self.inner.try_read() {
            Ok(guard) => guard,
            Err(sync::TryLockError::Poisoned(poison)) => poison.into_inner(),
            Err(sync::TryLockError::WouldBlock) => return Err(TryLockError),
        };
        Ok(RwLockReadGuard { inner: guard })
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        let guard = match self.inner.write() {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        };
        RwLockWriteGuard { inner: guard }
    }

    pub fn try_write(&self) -> TryLockResult<RwLockWriteGuard<'_, T>> {
        let guard = match self.inner.try_write() {
            Ok(guard) => guard,
            Err(sync::TryLockError::Poisoned(poison)) => poison.into_inner(),
            Err(sync::TryLockError::WouldBlock) => return Err(TryLockError),
        };
        Ok(RwLockWriteGuard { inner: guard })
    }

    pub fn into_inner(self) -> T
    where
        T: Sized,
    {
        match self.inner.into_inner() {
            Ok(inner) => inner,
            Err(poison) => poison.into_inner(),
        }
    }

    pub fn get_mut(&mut self) -> &mut T {
        match self.inner.get_mut() {
            Ok(t) => t,
            Err(poison) => poison.into_inner(),
        }
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut fmt = f.debug_struct("RwLock");
        match self.try_read() {
            Ok(guard) => fmt.field("value", &guard),
            Err(_) => fmt.field("value", &"<locked>"),
        }
        .finish_non_exhaustive()
    }
}

impl<T> From<T> for RwLock<T> {
    fn from(value: T) -> Self {
        RwLock::new(value)
    }
}

pub struct RwLockReadGuard<'a, T: ?Sized + 'a> {
    inner: sync::RwLockReadGuard<'a, T>,
}

impl<'a, T: ?Sized> Deref for RwLockReadGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'a, T: ?Sized + fmt::Debug> fmt::Debug for RwLockReadGuard<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl<'a, T: ?Sized + fmt::Display> fmt::Display for RwLockReadGuard<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

pub struct RwLockWriteGuard<'a, T: ?Sized + 'a> {
    inner: sync::RwLockWriteGuard<'a, T>,
}

impl<'a, T: ?Sized> Deref for RwLockWriteGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'a, T: ?Sized> DerefMut for RwLockWriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<'a, T: ?Sized + fmt::Debug> fmt::Debug for RwLockWriteGuard<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl<'a, T: ?Sized + fmt::Display> fmt::Display for RwLockWriteGuard<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}
