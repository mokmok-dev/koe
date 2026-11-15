//! A wake from the dictation thread to the shell's frame loop.
//!
//! The dictation thread marks a wake wherever it folds a recognition result
//! into the transcript, so a loop waiting on the wake stops waiting at that
//! fold.
//!
//! A wake marked while the loop is drawing is not lost: it stays pending, so a
//! result folded in during a frame is drawn on the frame after it. It is one
//! flag, not a queue: a hundred results between two waits still cost one
//! wakeup.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

/// The wake shared by the dictation thread and the shell's frame loop.
///
/// One flag and one waiter: the waker is a single slot, so a second wait
/// started while another is parked takes the first's place and leaves it
/// unreleased. [`wake`](Self::wake) hands out clones to mark, not to wait on,
/// and the shell's one frame loop holds the only wait.
#[derive(Clone, Debug, Default)]
pub struct Wake {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    pending: bool,
    waker: Option<Waker>,
}

impl Wake {
    /// Marks a wake pending and releases the waiting loop, if there is one.
    ///
    /// Called from the dictation thread. With no loop waiting this only sets
    /// the flag, so the loop's next wait returns at once.
    pub fn wake(&self) {
        let waker = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.pending = true;
            state.waker.take()
        };
        // The executor may poll the wait from inside `wake`, and that reloads
        // this lock, so the waker is released outside it.
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Waits for one [`wake`](Self::wake), returning at once when one is
    /// already pending and parking until one is marked otherwise.
    ///
    /// The wait has no deadline of its own; a caller that needs one bounds it
    /// from outside.
    pub fn wait(&self) -> impl Future<Output = ()> + '_ {
        Notified {
            state: Arc::clone(&self.state),
        }
    }

    /// Consumes the pending wake, reporting whether one was marked since the
    /// last consume.
    ///
    /// The polling form of [`wait`](Self::wait): it reports whether a wake
    /// landed, for a test that wants to look rather than wait.
    #[cfg(test)]
    pub(crate) fn consume(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut state.pending)
    }
}

/// A wait for one [`Wake`].
#[derive(Debug)]
struct Notified {
    state: Arc<Mutex<State>>,
}

impl Future for Notified {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.pending {
            state.pending = false;
            return Poll::Ready(());
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for Notified {
    /// Drops the waker when the timer won the wait, so the next [`Wake::wake`]
    /// does not reschedule a future that already moved on.
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.waker = None;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::pin::pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    use super::*;

    /// Counts the wakeups the executor would have run.
    #[derive(Default)]
    struct Counts(AtomicUsize);

    impl std::task::Wake for Counts {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            let _ = self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_wake_before_the_wait_ends_it_at_once() {
        let wake = Wake::default();
        wake.wake();
        let mut notified = pin!(wake.wait());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(notified.as_mut().poll(&mut cx).is_ready());
    }

    #[test]
    fn a_wait_parks_until_a_wake_arrives() {
        let wake = Wake::default();
        let counts = Arc::new(Counts::default());
        let waker = Waker::from(Arc::clone(&counts));
        let mut cx = Context::from_waker(&waker);
        let mut notified = pin!(wake.wait());
        assert!(notified.as_mut().poll(&mut cx).is_pending());
        assert_eq!(counts.0.load(Ordering::SeqCst), 0);

        wake.wake();
        assert_eq!(counts.0.load(Ordering::SeqCst), 1);
        assert!(notified.as_mut().poll(&mut cx).is_ready());
    }

    #[test]
    fn many_wakes_inside_one_wait_cost_one_wakeup() {
        let wake = Wake::default();
        let counts = Arc::new(Counts::default());
        let waker = Waker::from(Arc::clone(&counts));
        let mut cx = Context::from_waker(&waker);
        let mut notified = pin!(wake.wait());
        assert!(notified.as_mut().poll(&mut cx).is_pending());
        for _ in 0..5 {
            wake.wake();
        }
        assert_eq!(counts.0.load(Ordering::SeqCst), 1);
        assert!(notified.as_mut().poll(&mut cx).is_ready());
        // The text was drawn; nothing is pending again until the next result.
        assert!(notified.as_mut().poll(&mut cx).is_pending());
        assert_eq!(counts.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_wake_from_another_thread_releases_the_wait() {
        let wake = Wake::default();
        let counts = Arc::new(Counts::default());
        let waker = Waker::from(Arc::clone(&counts));
        let mut cx = Context::from_waker(&waker);
        let worker_wake = wake.clone();
        // The wait parks before the worker exists, so only the worker's mark
        // can release it.
        let mut notified = pin!(wake.wait());
        assert!(notified.as_mut().poll(&mut cx).is_pending());
        assert_eq!(counts.0.load(Ordering::SeqCst), 0);
        let worker = thread::spawn(move || worker_wake.wake());
        worker.join().unwrap();
        assert_eq!(counts.0.load(Ordering::SeqCst), 1);
        assert!(notified.as_mut().poll(&mut cx).is_ready());
    }

    #[test]
    fn a_dropped_wait_leaves_no_waker_to_release() {
        let wake = Wake::default();
        let counts = Arc::new(Counts::default());
        let waker = Waker::from(Arc::clone(&counts));
        let mut cx = Context::from_waker(&waker);
        {
            let mut notified = pin!(wake.wait());
            assert!(notified.as_mut().poll(&mut cx).is_pending());
        }
        // The loop moved on — its timer won the wait — so the waker it
        // registered is gone and a later result must not reschedule that
        // future. The flag below is still set, so the next wait returns at
        // once and the result is not lost.
        wake.wake();
        assert_eq!(counts.0.load(Ordering::SeqCst), 0);
        let mut next = pin!(wake.wait());
        assert!(next.as_mut().poll(&mut cx).is_ready());
    }

    #[test]
    fn consuming_the_pending_wake_clears_it() {
        let wake = Wake::default();
        assert!(!wake.consume());
        wake.wake();
        assert!(wake.consume());
        assert!(!wake.consume());
    }
}
