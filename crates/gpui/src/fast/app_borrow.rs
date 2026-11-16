//! Native Windows APIs can dispatch foreground task messages from a nested loop
//! while their caller still borrows App. Park those futures until the last app
//! borrow ends, rather than polling application code against an occupied AppCell.

#[cfg(any(target_os = "windows", test))]
mod guarded {
    use std::{
        cell::{Cell, RefCell},
        future::{Future, poll_fn},
        pin::pin,
        task::{Poll, Waker},
    };

    thread_local! {
        static BORROWS: Cell<usize> = const { Cell::new(0) };
        static WAITING: RefCell<Vec<Waker>> = const { RefCell::new(Vec::new()) };
    }

    /// Must be stored after the actual Ref/RefMut, so the RefCell is released
    /// before deferred tasks are woken by this field's destructor.
    pub(crate) struct Guard;

    impl Guard {
        pub(crate) fn new() -> Self {
            BORROWS.with(|borrows| borrows.set(borrows.get() + 1));
            Self
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let last = BORROWS.with(|borrows| {
                borrows.set(borrows.get() - 1);
                borrows.get() == 0
            });
            if last {
                let waiting = WAITING.with(|waiting| std::mem::take(&mut *waiting.borrow_mut()));
                for waker in waiting {
                    waker.wake();
                }
            }
        }
    }

    pub(crate) async fn foreground<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        poll_fn(|cx| {
            if BORROWS.with(|borrows| borrows.get() != 0) {
                WAITING.with(|waiting| {
                    let mut waiting = waiting.borrow_mut();
                    if !waiting.iter().any(|waker| waker.will_wake(cx.waker())) {
                        waiting.push(cx.waker().clone());
                    }
                });
                Poll::Pending
            } else {
                future.as_mut().poll(cx)
            }
        })
        .await
    }
}

#[cfg(target_os = "windows")]
pub(crate) use guarded::{Guard, foreground};

#[cfg(not(target_os = "windows"))]
pub(crate) fn foreground<F: std::future::Future>(future: F) -> F {
    future
}

#[cfg(test)]
mod tests {
    use super::guarded::{Guard, foreground};
    use std::{
        cell::RefCell,
        future::Future,
        pin::pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll, Wake, Waker},
    };

    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn shared_app_borrows_park_and_wake_foreground_work_once() {
        let state = RefCell::new(0);
        // Field order matches AppRef and AppRefMut: release the RefCell first.
        let outer = (state.borrow(), Guard::new());
        let inner = (state.borrow(), Guard::new());
        let counter = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(foreground(async {
            *state.borrow_mut() += 1;
        }));

        assert!(future.as_mut().poll(&mut cx).is_pending());
        assert!(future.as_mut().poll(&mut cx).is_pending());
        drop(inner);
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);
        drop(outer);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(()));
        assert_eq!(*state.borrow(), 1);
    }

    #[test]
    fn a_started_future_parks_again_during_a_mutable_app_borrow() {
        let polls = RefCell::new(0);
        let counter = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(foreground(std::future::poll_fn(|_| {
            let mut polls = polls.borrow_mut();
            *polls += 1;
            if *polls == 1 {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })));
        assert!(future.as_mut().poll(&mut cx).is_pending());
        let borrowed = (polls.borrow_mut(), Guard::new());
        assert!(future.as_mut().poll(&mut cx).is_pending());
        assert_eq!(*borrowed.0, 1);
        drop(borrowed);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(()));
        assert_eq!(*polls.borrow(), 2);
    }
}
