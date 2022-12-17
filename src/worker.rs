use std::{
    io,
    panic::resume_unwind,
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
    fn worker_is_send() {
        assert_send::<Worker<()>>();
    }
}
