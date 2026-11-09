//! System sleep and wake notifications that arrive while the app is borrowed.
//!
//! Upstream runs the sleep and wake observers straight from the platform
//! callback with `borrow_mut`. On Windows those callbacks come from
//! `WM_POWERBROADCAST`, a sent message, so any nested message loop on the main
//! thread can deliver it in the middle of a handler that still holds the `App`
//! borrow: a cross-process COM call waiting in `CoWaitForMultipleHandles`, a
//! modal dialog. The second borrow then panics with "RefCell already
//! borrowed". Here a notification that finds the app borrowed is run on the
//! next main-thread turn instead.

use std::rc::{Rc, Weak};

use crate::{App, AppCell, ForegroundExecutor};

/// The platform callback for `on_system_sleep`.
pub(crate) fn sleep_callback(app: &Rc<AppCell>, executor: ForegroundExecutor) -> Box<dyn FnMut()> {
    let app = Rc::downgrade(app);
    Box::new(move || {
        run_or_defer(&app, &executor, |cx| {
            cx.system_sleep_observers
                .clone()
                .retain(move |callback| (callback)(cx));
        })
    })
}

/// The platform callback for `on_system_wake`.
pub(crate) fn wake_callback(app: &Rc<AppCell>, executor: ForegroundExecutor) -> Box<dyn FnMut()> {
    let app = Rc::downgrade(app);
    Box::new(move || {
        run_or_defer(&app, &executor, |cx| {
            cx.system_wake_observers
                .clone()
                .retain(move |callback| (callback)(cx));
        })
    })
}

/// Runs `notify` now if the app can be borrowed, and otherwise retries it on
/// the next main-thread turn, until the borrow is released or the app is gone.
fn run_or_defer(app: &Weak<AppCell>, executor: &ForegroundExecutor, notify: fn(&mut App)) {
    let Some(app_cell) = app.upgrade() else {
        return;
    };
    if let Ok(mut cx) = app_cell.try_borrow_mut() {
        notify(&mut cx);
        return;
    }
    let app = app.clone();
    let retry = executor.clone();
    executor
        .spawn(async move { run_or_defer(&app, &retry, notify) })
        .detach();
}
