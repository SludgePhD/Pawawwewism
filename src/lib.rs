//! A simple library providing modern concurrency primitives.
//!
//! The goal of this library is to explore the design space of easy-to-use higher-level concurrency
//! primitives that implement the principles of *structured concurrency*, and also allow bridging
//! thread-based concurrency and `async` concurrency (via primitives that feature both a blocking
//! and an `async` API).
//!
//! # Why structured concurrency?
//!
//! Similar to how `goto` performs unstructured control flow, mechanisms like Go's `go` statement,
//! or threads/tasks that detach from the code that spawned them, perform *unstructured
//! concurrency*.
//! As it turns out, both `goto` and unstructured concurrency share very similar issues, which have
//! been detailed at length in [this blog post][notes-on-structured-concurrency].
//!
//! Modern languages generally eschew `goto` due to its many issues, instead relying on structured
//! control flow primitives like `if`, loops, `break`, `continue`, and `try ... catch`. However,
//! they do *not* generally eschew unstructured concurrency, presumably because that problem is
//! usually considered out-of-scope, or because structured concurrency is unfamiliar to most
//! programmers.
//!
//! While Rust does provide some tools to make concurrency easier, it still does *not*
//! provide any tools for structured concurrency (beyond [`thread::scope`]).
//! The wider Rust ecosystem is, unfortunately, no exception here: both `async_std` and `tokio`
//! allow cheaply spawning *unstructured* async tasks, which will simply continue running in the
//! background when the corresponding handle is dropped.
//!
//! [notes-on-structured-concurrency]: https://vorpus.org/blog/notes-on-structured-concurrency-or-go-statement-considered-harmful/
//! [`thread::scope`]: std::thread::scope
//!
//! # What is structured concurrency (in Rust)?
//!
//! In Rust specifically, my interpretation of *structured concurrency* means that:
//!
//! - Every background operation (whether thread or async task) is represented by an owned handle.
//! - No background operation outlasts its handle. If the handle is dropped, the operation is either
//!   canceled or joined (if it is a thread).
//!
//! This prevents resource leaks by joining or aborting the background operation when the value
//! representing it is dropped. We no longer have to remember to shut down background threads when
//! some component is shut down. The drawback: the automatic join can potentially hang forever, if
//! the thread doesn't react to the shutdown request, but this is a lot less subtle than never
//! stopping a background thread.
//!
//! This also brings some immediate code clarity benefits: now every background computation is
//! *required* to be represented as an in-scope value (frequently a field of a `struct`), constantly
//! reminding us of its presence.
//!
//! Due to Rust's ownership system, the background operations started by a program form a tree, just
//! like any other owned Rust value, and so have a unique owner (which can be sidestepped via
//! [`Arc`] and [`Rc`], but that's beside the point). This property actually allows us to add one
//! bonus feature with relative ease:
//!
//! - Panics occurring in background operations will be propagated to its owner.
//!
//! This is not normally the case when using [`std::thread`] or async tasks in most popular async
//! runtimes: those typically surface panics happening in the background thread or task as a
//! [`Result`].
//! If structured concurrency is implemented properly, the only way to catch a panic is to do so
//! explicitly with [`catch_unwind`].
//! All panics happening inside concurrent operations are handled in a reasonable way automatically,
//! and will (if the unwinding runtime is used) eventually unwind and reach the program's entry
//! point, just like panics that happen in sequential code. No additional panics will be raised, and
//! the piece of code that will be blamed for the panic is always predictable: it's the code owning
//! or interfacing with the background operation.
//!
//! Of course, structured concurrency is not magic. As soon as code stops being sequential, there is
//! the possibility that *multiple* panics happen at once. Since panics are only propagated when
//! "interacting" with the background operation in some way (eg. by dropping it, joining it, sending
//! it more work to do, or checking its status), panics will generally be forwarded to the owning
//! thread *opportunistically*, when they are noticed, rather than in the order they happened (and
//! regardless, Rust provides no reliable mechanism for determining this order).
//! This is why programs utilizing structured concurrency should generally avoid causing any
//! knock-on panics, like those caused by unwrapping a poisoned mutex, since they might be
//! propagated before the panic representing the actual root cause.
//!
//! [`catch_unwind`]: std::panic::catch_unwind
//! [`forget`]: std::mem::forget
//! [`Arc`]: std::sync::Arc
//! [`Rc`]: std::rc::Rc
//!
//! # Overview
//!
//! This library features several thread-based structured concurrency primitives:
//!
//! - [`background`][background()], which is a simple method to run a closure to completion on a
//!   [`Background`] thread.
//! - [`Worker`]/[`WorkerSet`], which is a background thread that processes packets of work fed to
//!   it from the owning thread.
//! - [`reader::Reader`], a background thread that reads from a cancelable stream and processes or
//!   forwards the results.
//!
//! Additionally, this library features communication primitives that can be used to exchange data
//! between background and foreground threads or tasks:
//!
//! - [`Promise`] and [`PromiseHandle`] provide a mechanism for communicating the result of
//!   computations (like those performed by a [`Worker`]).
//! - [`reactive::Value`] is a value that can be changed from one place, and notifies every
//!   associated [`reactive::Reader`] of that change, so that consumers can react to those changes.

mod background;
mod drop;
mod promise;
mod worker;

#[cfg(test)]
mod test;

pub mod isochronous;
pub mod reactive;
pub mod reader;

pub use background::*;
pub use promise::*;
pub use worker::*;
