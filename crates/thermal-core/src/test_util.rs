//! Helpers shared by the host tests.

use core::future::Future;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

/// Drives a future to completion without an async runtime.
///
/// The mocks never suspend, so a future that returns `Pending` means a test
/// awaited something real by mistake.
pub fn block_on<F: Future>(future: F) -> F::Output {
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );

    // SAFETY: clone returns an equivalent waker and wake, wake_by_ref and drop
    // are no-ops, which satisfies the RawWaker contract for a waker that is
    // never used to reschedule anything.
    let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
    let mut context = Context::from_waker(&waker);
    let future = core::pin::pin!(future);

    match future.poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("a mock returned Pending"),
    }
}
