use std::{
    panic::resume_unwind,
    thread::{self, JoinHandle},
};

/// A simple run-to-completion background thread.
///
/// Created with the freestanding [`background`] function.
///
/// This is an owned thread that runs a closure to completion and returns the result back to the
/// owning thread when it calls [`Background::join`].
///
/// Calling [`Background::join`] or dropping a [`Background`] object will join the thread. If the
/// thread panicked, the panic will be propagated to the owner.
pub struct Background<R> {
    handle: Option<JoinHandle<R>>,
}

impl<R> Drop for Background<R> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            if let Err(payload) = handle.join() {
                if !thread::panicking() {
                    resume_unwind(payload);
                }
            }
        }
    }
}

impl<R: Send + Sync + 'static> Background<R> {
    /// Blocks on the background thread and returns its result.
    ///
    /// If the thread panics, the panic will be propagated to the owner.
    pub fn join(mut self) -> R {
        let handle = self.handle.take().unwrap();
        match handle.join() {
            Ok(r) => r,
            Err(payload) => resume_unwind(payload),
        }
    }
}

/// Spawns a run-to-completion [`Background`] thread.
///
/// # Examples
///
/// Get a pair of connected [`TcpStream`]s by connecting to a [`TcpListener`] in a background
/// thread:
///
/// ```
/// use pawawwewism::background;
/// use std::net::{TcpListener, TcpStream, Ipv4Addr};
///
/// let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
/// let port = listener.local_addr().unwrap().port();
///
/// let client = background(move || TcpStream::connect((Ipv4Addr::LOCALHOST, port)));
/// let (server, _) = listener.accept().unwrap();
/// let client = client.join().unwrap();
/// ```
///
/// Both [`TcpStream::connect`] and [`TcpListener::accept`] block the caller, so this is not
/// possible in a single thread without using `async`.
///
/// [`TcpStream`]: std::net::TcpStream
/// [`TcpStream::connect`]: std::net::TcpStream::connect
/// [`TcpListener`]: std::net::TcpListener
/// [`TcpListener::accept`]: std::net::TcpListener::accept
pub fn background<R, F>(f: F) -> Background<R>
where
    R: Send + Sync + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    Background {
        handle: Some(thread::spawn(f)),
    }
}
