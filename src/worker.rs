use std::{
    io,
    panic::{self, resume_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
};

use crossbeam_channel::Sender;

use crate::drop::defer;

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
        if self.sender.as_ref().unwrap().send(msg).is_err() {
            // The thread has panicked.
            self.wait_for_exit();
            unreachable!("should have propagated panic");
        }
    }
}

/// A builder object that can be used to configure and spawn a [`WorkerSet`].
pub struct WorkerSetBuilder {
    name: Option<String>,
}

impl WorkerSetBuilder {
    /// Sets the base name of the [`WorkerSet`] threads.
    ///
    /// Each thread spawned will be named according to this base name and its index.
    pub fn name<N: Into<String>>(self, name: N) -> Self {
        Self {
            name: Some(name.into()),
            ..self
        }
    }

    /// Spawns a [`WorkerSet`] that uses `handler` to process incoming messages.
    ///
    /// Unlike [`WorkerBuilder::spawn`], this method requires `handler` to implement [`Fn`] (not
    /// just [`FnMut`]), because it is shared across all threads in the set, and may execute several
    /// times at once.
    pub fn spawn<I, F>(self, count: usize, handler: F) -> io::Result<WorkerSet<I>>
    where
        I: Send + 'static,
        F: Fn(I) + Send + Sync + 'static,
    {
        assert_ne!(count, 0, "count must be at least 1");

        let panic_flag = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(handler);
        let mut handles = Vec::with_capacity(count);
        let (sender, recv) = crossbeam_channel::bounded(0);
        for i in 0..count {
            let mut builder = thread::Builder::new();
            if let Some(name) = &self.name {
                builder = builder.name(format!("{name}-{i}"));
            }
            let recv = recv.clone();
            let handler = handler.clone();
            let panic_flag = panic_flag.clone();
            let handle = builder.spawn(move || {
                let res = panic::catch_unwind(AssertUnwindSafe(|| {
                    for message in recv {
                        handler(message);
                    }
                }));
                match res {
                    Ok(()) => {}
                    Err(payload) => {
                        panic_flag.store(true, Ordering::Relaxed);
                        panic::resume_unwind(payload);
                    }
                }
            })?;
            handles.push(handle);
        }

        Ok(WorkerSet {
            sender: Some(sender),
            handles,
            panic_flag,
        })
    }
}

/// An owned set of [`Worker`] threads that all process the same type of message.
///
/// This can be used to spread identical computations across several threads, when a single thread
/// does not provide enough throughput for the application.
///
/// When [`WorkerSet::send`] is called, or the [`WorkerSet`] is dropped, panics from the worker
/// threads are propagated to the owning thread. If more than one worker thread has panicked, the
/// panic payload of one of the panicked threads will be propagated.
pub struct WorkerSet<I: Send + 'static> {
    sender: Option<Sender<I>>,
    handles: Vec<JoinHandle<()>>,
    /// Set to `true` when any thread panics.
    panic_flag: Arc<AtomicBool>,
}

impl<I: Send + 'static> Drop for WorkerSet<I> {
    fn drop(&mut self) {
        // Close the channel to signal the threads to exit.
        drop(self.sender.take());

        self.wait_for_exit();
    }
}

impl WorkerSet<()> {
    /// Returns a builder that can be used to configure and spawn a [`WorkerSet`].
    #[inline]
    pub fn builder() -> WorkerSetBuilder {
        WorkerSetBuilder { name: None }
    }
}

impl<I: Send + 'static> WorkerSet<I> {
    fn wait_for_exit(&mut self) {
        // Wait for all threads to exit and propagate a panic if one of them panicked.
        let mut payload = None;
        for handle in self.handles.drain(..) {
            if let Err(pl) = handle.join() {
                payload = Some(pl);
            }
        }
        if let Some(payload) = payload {
            if !thread::panicking() {
                resume_unwind(payload);
            }
        }
        // NB: this does not handle panics in the panic payload.
        // If you do that sorta stuff, you're evil.
    }

    /// Sends a message to one of the worker threads in this set.
    ///
    /// If no worker is available to process the message, this will block until one is available.
    ///
    /// If the worker has panicked, this will propagate the panic to the calling thread.
    pub fn send(&mut self, msg: I) {
        if self.panic_flag.load(Ordering::Relaxed) {
            // A thread has panicked. Close the channel to signal all threads to exit.
            drop(self.sender.take());
            self.wait_for_exit();
            unreachable!("should have propagated panic");
        }

        if self.sender.as_ref().unwrap().send(msg).is_err() {
            // All threads have panicked.
            // (we don't strictly need to check this but it makes this method match `Worker::send`'s
            // behavior)
            self.wait_for_exit();
            unreachable!("should have propagated panic");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{catch_unwind, AssertUnwindSafe},
        time::Duration,
    };

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
    fn workerset_singleton_propagates_panic_on_send() {
        let mut worker = WorkerSet::builder()
            .spawn(1, |_| silent_panic("worker panic".into()))
            .unwrap();
        worker.send(());
        catch_unwind(AssertUnwindSafe(|| worker.send(()))).unwrap_err();
        catch_unwind(AssertUnwindSafe(|| drop(worker))).unwrap();
    }

    #[test]
    fn workerset_many_propagates_panic_on_send() {
        let panicked = AtomicBool::new(false);
        let mut worker = WorkerSet::builder()
            .spawn(8, move |_| {
                // Panic on the first message only.
                if !panicked.load(Ordering::Relaxed) {
                    panicked.store(true, Ordering::Relaxed);
                    silent_panic("worker panic".into());
                }
            })
            .unwrap();

        worker.send(());
        while catch_unwind(AssertUnwindSafe(|| worker.send(()))).is_ok() {
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn worker_is_send() {
        assert_send::<Worker<()>>();
    }

    #[test]
    fn workerset_is_send() {
        assert_send::<WorkerSet<()>>();
    }
}
