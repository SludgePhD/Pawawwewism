//! Utilities related to destructors and drop.

/// Drop guard returned by [`defer`].
#[must_use = "`Defer` should be assigned to a variable, or it will be dropped immediately"]
pub struct Defer<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for Defer<F> {
    fn drop(&mut self) {
        (self.0.take().unwrap())();
    }
}

/// Returns a value that runs `cb` when dropped.
pub fn defer<F: FnOnce()>(cb: F) -> Defer<F> {
    Defer(Some(cb))
}
