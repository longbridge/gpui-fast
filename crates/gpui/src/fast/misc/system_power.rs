//! System sleep and wake notifications that arrive while the app is borrowed.
//!
//! Upstream runs the sleep and wake observers straight from the platform
//! callback with `borrow_mut`. On Windows those callbacks come from
//! `WM_POWERBROADCAST`, a sent message, so any nested message loop on the main
//! thread can deliver it in the middle of a handler that still holds the `App`
//! borrow: a cross-process COM call waiting in `CoWaitForMultipleHandles`, a
//! modal dialog. The second borrow then panics with "RefCell already
//! borrowed". Here a notification that finds the app borrowed is queued and
//! run on a later main-thread turn instead.

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::{Rc, Weak},
};

use crate::{App, AppCell, AppRefMut, ForegroundExecutor};

/// Sleep and wake notifications not yet delivered, shared by the two platform
/// callbacks so they are delivered in the order they arrived: a wake that
/// finds the app free still waits for a sleep deferred before it.
pub(crate) struct PowerNotifications {
    executor: ForegroundExecutor,
    pending: RefCell<VecDeque<fn(&mut App)>>,
    retry_scheduled: Cell<bool>,
}

impl PowerNotifications {
    pub(crate) fn new(executor: ForegroundExecutor) -> Rc<Self> {
        Rc::new(Self {
            executor,
            pending: RefCell::default(),
            retry_scheduled: Cell::new(false),
        })
    }

    /// Replaces the `borrow_mut` of the `on_system_sleep` platform callback.
    /// `None` means the app is borrowed and the sleep observers will run on a
    /// later turn.
    pub(crate) fn borrow_for_sleep<'a>(
        this: &Rc<Self>,
        app: &'a Rc<AppCell>,
    ) -> Option<AppRefMut<'a>> {
        Self::borrow_for(this, app, |cx| {
            cx.system_sleep_observers
                .clone()
                .retain(move |callback| (callback)(cx));
        })
    }

    /// Replaces the `borrow_mut` of the `on_system_wake` platform callback.
    /// `None` means the app is borrowed and the wake observers will run on a
    /// later turn.
    pub(crate) fn borrow_for_wake<'a>(
        this: &Rc<Self>,
        app: &'a Rc<AppCell>,
    ) -> Option<AppRefMut<'a>> {
        Self::borrow_for(this, app, |cx| {
            cx.system_wake_observers
                .clone()
                .retain(move |callback| (callback)(cx));
        })
    }

    /// Borrows the app for the caller to notify its observers. Returns `None`
    /// when this notification is handled here instead, so the caller must not
    /// run its observers:
    ///
    /// - the app is already borrowed: `notify` is queued for a later turn;
    /// - earlier notifications are still queued: `notify` joins the end of
    ///   the queue and the whole queue is delivered now, in arrival order.
    fn borrow_for<'a>(
        this: &Rc<Self>,
        app: &'a Rc<AppCell>,
        notify: fn(&mut App),
    ) -> Option<AppRefMut<'a>> {
        let Ok(mut cx) = app.try_borrow_mut() else {
            this.pending.borrow_mut().push_back(notify);
            Self::schedule_retry(this, Rc::downgrade(app));
            return None;
        };
        if this.pending.borrow().is_empty() {
            return Some(cx);
        }
        this.pending.borrow_mut().push_back(notify);
        Self::deliver_pending(this, &mut cx);
        None
    }

    /// Runs the queued notifications in order. One that an observer delivers
    /// while this runs finds the app borrowed, joins the end of the queue and
    /// is run by this same loop.
    fn deliver_pending(this: &Rc<Self>, cx: &mut App) {
        loop {
            let Some(notify) = this.pending.borrow_mut().pop_front() else {
                break;
            };
            notify(cx);
        }
    }

    /// Delivers the queued notifications on the next main-thread turn.
    ///
    /// The retry is deliberately unbounded: while a nested message loop keeps
    /// the outer borrow alive, every turn finds the app borrowed and schedules
    /// one more cheap attempt, until the outer handler returns or the app is
    /// gone.
    fn schedule_retry(this: &Rc<Self>, app: Weak<AppCell>) {
        if this.retry_scheduled.replace(true) {
            return;
        }
        let retry = this.clone();
        this.executor
            .spawn(async move {
                retry.retry_scheduled.set(false);
                let Some(app) = app.upgrade() else {
                    return;
                };
                match app.try_borrow_mut() {
                    Ok(mut cx) => Self::deliver_pending(&retry, &mut cx),
                    Err(_) => Self::schedule_retry(&retry, Rc::downgrade(&app)),
                }
            })
            .detach();
    }
}
