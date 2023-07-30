//! Internal unit test utilities.

use std::{
    future::Future,
    pin::pin,
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll, Wake, Waker},
};

/// Polls a future to completion, returning its result.
pub fn block_on<R, F: Future<Output = R>>(fut: F) -> R {
    #[derive(Default)]
    struct RealWaker {
        /// Waiters are signaled by incrementing this number and notifying the `Condvar`.
        mtx: Mutex<u64>,
        condvar: Condvar,
    }
    impl Wake for RealWaker {
        fn wake(self: Arc<Self>) {
            *self.mtx.lock().unwrap() += 1;
            self.condvar.notify_one();
        }
    }

    let arc = Arc::new(RealWaker::default());
    let waker = Waker::from(arc.clone());
    let mut cx = Context::from_waker(&waker);

    let mut fut = pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => {
                let guard = arc.mtx.lock().unwrap();
                let cur = *guard;
                drop(arc.condvar.wait_while(guard, |n| *n == cur).unwrap());
            }
        }
    }
}
