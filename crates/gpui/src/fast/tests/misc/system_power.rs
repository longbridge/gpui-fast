//! Tests of [`crate::fast::misc::system_power`].

use std::{cell::Cell, rc::Rc};

use crate::{
    TestAppContext,
    fast::misc::system_power::{sleep_callback, wake_callback},
};

fn count_wakes(cx: &mut TestAppContext) -> Rc<Cell<usize>> {
    let wakes = Rc::new(Cell::new(0));
    cx.update(|cx| {
        let wakes = wakes.clone();
        cx.on_system_wake(move |_| wakes.set(wakes.get() + 1))
            .detach();
    });
    wakes
}

#[crate::test]
fn wake_runs_its_observers_at_once_when_the_app_is_free(cx: &mut TestAppContext) {
    let wakes = count_wakes(cx);
    let mut wake = wake_callback(&cx.app, cx.foreground_executor().clone());
    wake();
    assert_eq!(wakes.get(), 1);
}

#[crate::test]
fn wake_during_a_borrow_waits_for_the_borrow_to_end(cx: &mut TestAppContext) {
    let wakes = count_wakes(cx);
    let mut wake = wake_callback(&cx.app, cx.foreground_executor().clone());
    {
        let _held = cx.app.borrow_mut();
        wake();
        assert_eq!(wakes.get(), 0);
    }
    cx.run_until_parked();
    assert_eq!(wakes.get(), 1);
}

#[crate::test]
fn sleep_during_a_borrow_waits_for_the_borrow_to_end(cx: &mut TestAppContext) {
    let sleeps = Rc::new(Cell::new(0));
    cx.update(|cx| {
        let sleeps = sleeps.clone();
        cx.on_system_sleep(move |_| sleeps.set(sleeps.get() + 1))
            .detach();
    });
    let mut sleep = sleep_callback(&cx.app, cx.foreground_executor().clone());
    {
        let _held = cx.app.borrow_mut();
        sleep();
        assert_eq!(sleeps.get(), 0);
    }
    cx.run_until_parked();
    assert_eq!(sleeps.get(), 1);
}
