//! Background threads that perform reading I/O.
//!
//! This module provides the [`Reader`] type, an owned background thread reading from some stream
//! that implements [`Shutdown`], which allows remotely stopping the blocking operation and exiting
//! the thread.

use std::{
    io::{self, Read},
    net::{self, TcpStream},
    panic::resume_unwind,
    sync::Arc,
    thread::{self, JoinHandle},
};

use crate::drop::defer;

/// A builder for [`Reader`]s.
#[derive(Default)]
pub struct ReaderBuilder {
    name: Option<String>,
}

impl ReaderBuilder {
    /// Creates a new [`ReaderBuilder`] with default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the name of the spawned thread.
    pub fn name<N: Into<String>>(self, name: N) -> Self {
        Self {
            name: Some(name.into()),
            ..self
        }
    }

    /// Spawns a [`Reader`] that will run `handler` in a separate thread.
    ///
    /// The contract `reader` has to uphold is as follows: no blocking I/O must be performed, except
    /// for reading from `read` (which is passed to `handler` in a [`ReadWrapper`]). If this
    /// contract is not upheld, the owning thread might block while waiting on a [`Reader`] thread
    /// to exit.
    pub fn spawn<F, R, O>(self, read: R, handler: F) -> io::Result<Reader<R, O>>
    where
        F: FnOnce(ReadWrapper<&R>) -> io::Result<O> + Send + 'static,
        R: Shutdown + Send + Sync + 'static,
        O: Send + 'static,
    {
        let reader = Arc::new(read);
        let reader2 = reader.clone();
        let mut builder = thread::Builder::new();
        if let Some(name) = self.name.clone() {
            builder = builder.name(name);
        }
        let handle = builder.spawn(move || {
            let _guard;
            if let Some(name) = self.name {
                log::trace!("reader '{name}' starting");
                _guard = defer(move || log::trace!("reader '{name}' exiting"));
            }
            handler(ReadWrapper(&reader2))
        })?;
        Ok(Reader {
            reader,
            thread: Some(handle),
        })
    }
}

/// An owned thread that reads from a stream.
///
/// This type works with streams that can be shut down from another thread (via the [`Shutdown`]
/// trait). That way, the worker thread can perform a blocking read on the stream, while the owning
/// thread can still make the worker thread exit by shutting down the stream.
///
/// When a [`Reader`] is dropped, or [`Reader::stop`] is called, the stream will be shut down and
/// the thread will be joined. If the thread has panicked, the panic will be propagated to the
/// owner.
pub struct Reader<R: Shutdown, O> {
    reader: Arc<R>,
    thread: Option<JoinHandle<io::Result<O>>>,
}

impl<R: Shutdown, O> Drop for Reader<R, O> {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            // We ignore errors during shutdown, since those will also make the worker thread exit.
            self.reader.shutdown().ok();

            if let Err(payload) = thread.join() {
                if !thread::panicking() {
                    resume_unwind(payload);
                }
            }
        }
    }
}

impl<R: Shutdown, O> Reader<R, O> {
    /// Spawns a [`Reader`] that will run `handler` in a separate thread.
    ///
    /// The contract `handler` has to uphold is as follows: no blocking I/O must be performed,
    /// except for reading from the [`ReadWrapper`] passed to it.
    ///
    /// Also see [`ReaderBuilder`].
    pub fn spawn<F>(read: R, handler: F) -> io::Result<Reader<R, O>>
    where
        F: FnOnce(ReadWrapper<&R>) -> io::Result<O> + Send + 'static,
        R: Shutdown + Send + Sync + 'static,
        O: Send + 'static,
    {
        ReaderBuilder::new().spawn(read, handler)
    }

    /// Stops the [`Reader`] by shutting down the stream it is reading from.
    ///
    /// After shutting down the stream, the thread will be joined and its return value will be
    /// returned.
    ///
    /// If the thread has panicked, the panic will be propagated to the caller.
    pub fn stop(mut self) -> io::Result<O> {
        // We ignore errors during shutdown, since those will also make the worker thread exit.
        self.reader.shutdown().ok();

        let thread = self.thread.take().unwrap();
        match thread.join() {
            Err(payload) => resume_unwind(payload),
            Ok(out) => out,
        }
    }

    /// Checks whether the [`Reader`] thread has finished execution.
    ///
    /// [`Reader`]s are allowed to return a value from their main function if they choose to exit.
    /// To access that value, call [`Reader::stop`]. They can also "finish" by panicking. Panics are
    /// propagated to the owner.
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().unwrap().is_finished()
    }
}

/// A wrapper around a [`Read`] implementor that hides any other functionality of the type.
pub struct ReadWrapper<R>(R);

impl<R: Read> Read for ReadWrapper<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

/// Trait for streams that can be shut down externally.
pub trait Shutdown: Read {
    /// Shuts down the stream, making any in-progress or future attempt to read from it return
    /// `Ok(0)`.
    fn shutdown(&self) -> io::Result<()>;
}

impl Shutdown for TcpStream {
    fn shutdown(&self) -> io::Result<()> {
        self.shutdown(net::Shutdown::Read)
    }
}

#[cfg(unix)]
impl Shutdown for std::os::unix::net::UnixStream {
    fn shutdown(&self) -> io::Result<()> {
        self.shutdown(net::Shutdown::Read)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        net::{Ipv4Addr, TcpListener},
        sync::atomic::{AtomicBool, Ordering},
    };

    use crate::background;

    use super::*;

    fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        let client = background(move || TcpStream::connect((Ipv4Addr::LOCALHOST, port)));
        let (server, _) = listener.accept().unwrap();
        let client = client.join().unwrap();
        (client, server)
    }

    #[test]
    fn tcp_stream_reader_stop() {
        let (read, mut write) = tcp_pair();
        let rdr = Reader::spawn(read, |mut read| {
            let mut buf = Vec::new();
            read.read_to_end(&mut buf)?;
            Ok(buf)
        })
        .unwrap();

        let msg = b"hello world!";
        write.write_all(msg).unwrap();
        let buf = rdr.stop().unwrap();
        assert_eq!(buf, msg);
    }

    #[test]
    fn tcp_stream_reader_drop() {
        let (read, mut write) = tcp_pair();
        let rdr = Reader::spawn(read, |mut read| {
            let mut buf = Vec::new();
            read.read_to_end(&mut buf)?;
            Ok(buf)
        })
        .unwrap();

        let msg = b"hello world!";
        write.write_all(msg).unwrap();
        drop(rdr);
    }

    #[test]
    fn tcp_stream_reader_eof() {
        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();
        let (read, mut write) = tcp_pair();
        let rdr = Reader::spawn(read, move |mut read| {
            let mut buf = Vec::new();
            read.read_to_end(&mut buf)?;
            done2.store(true, Ordering::Relaxed);
            Ok(buf)
        })
        .unwrap();

        let msg = b"hello world!";
        write.write_all(msg).unwrap();
        drop(write);

        // The thread should now exit by itself. Wait for that to happen.
        while !done.load(Ordering::Relaxed) {
            thread::yield_now();
        }

        let buf = rdr.stop().unwrap();
        assert_eq!(buf, msg);
    }

    #[cfg(unix)]
    #[test]
    fn unix_stream_reader_stop() {
        use std::os::unix::net::UnixStream;

        let (read, mut write) = UnixStream::pair().unwrap();
        let rdr = Reader::spawn(read, |mut read| {
            let mut buf = Vec::new();
            read.read_to_end(&mut buf)?;
            Ok(buf)
        })
        .unwrap();

        let msg = b"hello world!";
        write.write_all(msg).unwrap();
        let buf = rdr.stop().unwrap();
        assert_eq!(buf, msg);
    }

    #[cfg(unix)]
    #[test]
    fn unix_stream_reader_drop() {
        use std::os::unix::net::UnixStream;

        let (read, mut write) = UnixStream::pair().unwrap();
        let rdr = Reader::spawn(read, |mut read| {
            let mut buf = Vec::new();
            read.read_to_end(&mut buf)?;
            Ok(buf)
        })
        .unwrap();

        let msg = b"hello world!";
        write.write_all(msg).unwrap();
        drop(rdr);
    }

    #[cfg(unix)]
    #[test]
    fn unix_stream_reader_eof() {
        use std::os::unix::net::UnixStream;

        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();
        let (read, mut write) = UnixStream::pair().unwrap();
        let rdr = Reader::spawn(read, move |mut read| {
            let mut buf = Vec::new();
            read.read_to_end(&mut buf)?;
            done2.store(true, Ordering::Relaxed);
            Ok(buf)
        })
        .unwrap();

        let msg = b"hello world!";
        write.write_all(msg).unwrap();
        drop(write);

        // The thread should now exit by itself. Wait for that to happen.
        while !done.load(Ordering::Relaxed) {
            thread::yield_now();
        }

        let buf = rdr.stop().unwrap();
        assert_eq!(buf, msg);
    }
}
