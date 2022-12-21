//! A simple library for structured concurrency and heterogeneous thread-based parallel processing.
//!
//! (if you're looking for homogeneous parallel processing using an iterator-like interface, check
//! out [`rayon`] instead; if you're looking for running large numbers of I/O tasks concurrently
//! instead of running a small number of computationally expensive tasks concurrently, you're
//! probably better served by an `async` runtime)
//!
//! # Overview
//!
//! This library features 3 main ways of doing structured concurrency:
//!
//! - [`background`][background()], which is a simple method to run a closure on a background
//!   thread.
//! - [`Worker`] and [`Promise`], which allow constructing arbitrary pipelined computation graphs
//!   that process packets of work fed to them from the owning thread.
//! - [`reader::Reader`], a background thread that reads from a cancelable stream and processes or
//!   forwards the results.
//!
//! # Workers and Promises
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

mod background;
mod drop;
mod promise;
pub mod reader;
mod worker;

pub use background::*;
pub use promise::*;
pub use worker::*;
