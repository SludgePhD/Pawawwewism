//! Isochronous data processing on thread pools.
//!
//! This module provides the [`IsochronousProcessor`], a thread pool that performs processing jobs
//! while ensuring that the wall-clock pacing of output messages matches that of the input messages.
//!
//! The primary use cases that [`IsochronousProcessor`] was built for was for image processing on
//! video streams.

use std::{
    collections::VecDeque,
    io, thread,
    time::{Duration, Instant},
};

use crate::{promise, Promise, PromiseHandle, WorkerSet};

/// Processes messages on a thread pool, while retaining order and pacing in the stream of results.
///
/// This can be used for real-time (in the "synchronized with wall-clock time" meaning of the word,
/// not "kills people when it goes wrong") multi-threaded data processing (for when a single thread
/// has insufficient throughput to match requirements).
pub struct IsochronousProcessor<I: Send + 'static, O: Send + 'static> {
    workers: WorkerSet<(I, Promise<O>)>,
    in_progress: VecDeque<Job<O>>,
    /// When was the last value submitted to the processor?
    last_submit: Option<Instant>,
    /// When was the last value yielded to the owner?
    last_yield: Option<Instant>,
}

impl IsochronousProcessor<(), ()> {
    #[inline]
    pub fn builder() -> IsochronousProcessorBuilder {
        IsochronousProcessorBuilder { name: None }
    }
}

impl<I: Send + 'static, O: Send + 'static> IsochronousProcessor<I, O> {
    /// Submits a value to be processed, and returns a processed result if one is available.
    ///
    /// This will block until a thread is available to process the value.
    ///
    /// If no processed value is available, no further blocking occurs and [`None`] is returned.
    ///
    /// If a value *is* available, this method may block, so that two subsequent values are always
    /// returned with at least as much time between them as the inputs from which they were
    /// computed.
    ///
    /// Note that `exchange` can potentially yield the processed result of the `value` submitted in
    /// the *same call*, if the processing is simple enough.
    pub fn exchange(&mut self, value: I) -> Option<O> {
        self.submit(value);

        // Now check if the oldest job is finished, and yield it if so.
        let oldest = self.in_progress.front().unwrap();
        if oldest.handle.will_block() {
            // Not done yet.
            return None;
        }

        self.block()
    }

    /// Submits a new value for processing, blocking until a thread is available to accept it.
    ///
    /// [`Self::block`] can then be used to retrieve the results of the processing operation.
    ///
    /// Note that calling `submit` without also calling [`Self::block`] (or another method that
    /// retrieves the results of the processing) approximately the same number of times will queue
    /// up jobs indefinitely and leak memory. [`Self::exchange`] may be used as a convenience
    /// wrapper that always attempts to retrieve a processed value.
    pub fn submit(&mut self, value: I) {
        let now = Instant::now();

        // Submit job.
        let (promise, handle) = promise();
        self.workers.send((value, promise));

        let pace = match self.last_submit {
            Some(last) => now - last,
            None => Duration::ZERO,
        };
        self.last_submit = Some(now);
        self.in_progress.push_back(Job {
            handle,
            block: pace,
        });
    }

    /// Tries to submit a new value for processing, returning an error when that would block.
    ///
    /// Also see [`Self::block`].
    pub fn try_submit(&mut self, value: I) -> Result<(), I> {
        let now = Instant::now();

        // Try to submit job.
        let (promise, handle) = promise();
        self.workers
            .try_send((value, promise))
            .map_err(|(value, _)| value)?;

        let pace = match self.last_submit {
            Some(last) => now - last,
            None => Duration::ZERO,
        };
        self.last_submit = Some(now);
        self.in_progress.push_back(Job {
            handle,
            block: pace,
        });
        Ok(())
    }

    /// Blocks until the oldest submitted value has been processed, and returns it.
    ///
    /// Returns [`None`] if no processing jobs remain, or when the user-provided processing closure
    /// has panicked.
    pub fn block(&mut self) -> Option<O> {
        let job = self.in_progress.pop_front()?;
        let result = job.handle.block().ok()?;
        // (this will only return `None` when a thread has panicked; in that case the next
        // `workers.send` call will propagate the panic)

        // Compute the intended release time of the value, based on the time we yielded the last
        // value and the input message pacing.
        if let Some(time) = self.last_yield {
            let target_time = time + job.block;
            let delay = target_time.saturating_duration_since(Instant::now());
            thread::sleep(delay);
        }

        self.last_yield = Some(Instant::now());
        Some(result)
    }

    /// Returns whether a processed output value is available.
    ///
    /// If this returns `true`, a call to [`Self::block`] will not wait for any background
    /// processing, but it may block to match the pacing of the input submissions.
    pub fn is_output_available(&self) -> bool {
        match self.in_progress.front() {
            Some(front) if !front.handle.will_block() => true,
            _ => false,
        }
    }
}

struct Job<O> {
    handle: PromiseHandle<O>,
    /// Time between the submission of this job, and the preceding job. The processor will only
    /// yield the result of this job when at least this amount of time has passed since the previous
    /// value was yielded.
    block: Duration,
}

/// A builder object that can be used to configure and spawn a [`IsochronousProcessor`].
pub struct IsochronousProcessorBuilder {
    name: Option<String>,
}

impl IsochronousProcessorBuilder {
    /// Sets the base name of the [`IsochronousProcessor`] threads.
    ///
    /// Each thread spawned will be named according to this base name and its index.
    pub fn name<N: Into<String>>(self, name: N) -> Self {
        Self {
            name: Some(name.into()),
            ..self
        }
    }

    /// Spawns a [`IsochronousProcessor`] that uses `handler` to process incoming messages.
    ///
    /// Like [`WorkerSetBuilder::spawn`], this method requires `handler` to implement [`Fn`] (not
    /// just [`FnMut`]), because it is shared across all threads, and may execute several times at
    /// once.
    ///
    /// # Panics
    ///
    /// This method will panic if `count` is 0.
    ///
    /// [`WorkerSetBuilder::spawn`]: crate::WorkerSetBuilder::spawn
    pub fn spawn<I, O, F>(self, count: usize, handler: F) -> io::Result<IsochronousProcessor<I, O>>
    where
        I: Send + 'static,
        O: Send + 'static,
        F: Fn(I) -> O + Send + Sync + 'static,
    {
        let mut builder = WorkerSet::builder();
        if let Some(name) = self.name {
            builder = builder.name(name);
        }
        Ok(IsochronousProcessor {
            workers: builder.spawn(count, move |(value, promise): (I, Promise<O>)| {
                let output = handler(value);
                promise.fulfill(output);
            })?,
            in_progress: VecDeque::new(),
            last_submit: None,
            last_yield: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_isochronous() {
        let mut proc = IsochronousProcessor::builder()
            .name("test_processor")
            .spawn(1, |i| i)
            .unwrap();
        assert!(proc.block().is_none());
        proc.submit(0);
        proc.submit(1);
        proc.submit(2);
        assert_eq!(proc.block(), Some(0));
        assert_eq!(proc.block(), Some(1));
        assert_eq!(proc.block(), Some(2));
        assert_eq!(proc.block(), None);
    }

    #[test]
    fn test_input_pacing() {
        let mut proc = IsochronousProcessor::builder()
            .name("test_processor")
            .spawn(2, |i| i)
            .unwrap();

        let delay = Duration::from_millis(10);
        proc.submit(0);
        thread::sleep(delay * 2); // will take *at least* 20ms
        proc.submit(1);
        thread::sleep(delay); // will take *at least* 10ms
        proc.submit(2);

        let start = Instant::now();
        assert_eq!(proc.block(), Some(0));
        assert_eq!(proc.block(), Some(1));
        let elapsed = start.elapsed();
        assert!(
            elapsed >= delay * 2,
            "{:?} elapsed, expected at least {:?}",
            elapsed,
            delay * 2,
        );

        let start = Instant::now();
        assert_eq!(proc.block(), Some(2));
        let elapsed = start.elapsed();
        assert!(
            elapsed >= delay,
            "{:?} elapsed, expected at least {:?}",
            elapsed,
            delay,
        );
    }

    // TODO: test behavior in more detail, like with varying input and processing timings
}
