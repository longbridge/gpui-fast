//! Tests of [`crate::fast::misc::system_power`].

use std::{cell::RefCell, rc::Rc};

use crate::{AppCell, TestAppContext, fast::misc::system_power::PowerNotifications};

/// What the `on_system_sleep` platform callback does.
fn sleep(power: &Rc<PowerNotifications>, app: &Rc<AppCell>) {
    if let Some(cx) = &mut PowerNotifications::borrow_for_sleep(power, app) {
        cx.system_sleep_observers
            .clone()
            .retain(move |callback| (callback)(cx));
    }
}

/// What the `on_system_wake` platform callback does.
fn wake(power: &Rc<PowerNotifications>, app: &Rc<AppCell>) {
    if let Some(cx) = &mut PowerNotifications::borrow_for_wake(power, app) {
        cx.system_wake_observers
            .clone()
            .retain(move |callback| (callback)(cx));
    }
}

fn record_power_events(cx: &mut TestAppContext) -> Rc<RefCell<Vec<&'static str>>> {
    let events = Rc::new(RefCell::new(Vec::new()));
    cx.update(|cx| {
        let sleeps = events.clone();
        cx.on_system_sleep(move |_| sleeps.borrow_mut().push("sleep"))
            .detach();
        let wakes = events.clone();
        cx.on_system_wake(move |_| wakes.borrow_mut().push("wake"))
            .detach();
    });
    events
}

#[crate::test]
fn wake_runs_its_observers_at_once_when_the_app_is_free(cx: &mut TestAppContext) {
    let events = record_power_events(cx);
    let power = PowerNotifications::new(cx.foreground_executor().clone());
    wake(&power, &cx.app);
    assert_eq!(*events.borrow(), ["wake"]);
}

#[crate::test]
fn wake_during_a_borrow_waits_for_the_borrow_to_end(cx: &mut TestAppContext) {
    let events = record_power_events(cx);
    let power = PowerNotifications::new(cx.foreground_executor().clone());
    {
        let _held = cx.app.borrow_mut();
        wake(&power, &cx.app);
    }
    assert!(events.borrow().is_empty());
    cx.run_until_parked();
    assert_eq!(*events.borrow(), ["wake"]);
}

#[crate::test]
fn sleep_during_a_borrow_waits_for_the_borrow_to_end(cx: &mut TestAppContext) {
    let events = record_power_events(cx);
    let power = PowerNotifications::new(cx.foreground_executor().clone());
    {
        let _held = cx.app.borrow_mut();
        sleep(&power, &cx.app);
    }
    assert!(events.borrow().is_empty());
    cx.run_until_parked();
    assert_eq!(*events.borrow(), ["sleep"]);
}

#[crate::test]
fn wake_on_a_free_app_still_follows_a_deferred_sleep(cx: &mut TestAppContext) {
    let events = record_power_events(cx);
    let power = PowerNotifications::new(cx.foreground_executor().clone());
    {
        let _held = cx.app.borrow_mut();
        sleep(&power, &cx.app);
    }
    wake(&power, &cx.app);
    assert_eq!(*events.borrow(), ["sleep", "wake"]);
    cx.run_until_parked();
    assert_eq!(*events.borrow(), ["sleep", "wake"]);
}

#[crate::test]
fn sleep_delivered_by_an_observer_waits_behind_the_arriving_wake(cx: &mut TestAppContext) {
    let events = Rc::new(RefCell::new(Vec::new()));
    let power = PowerNotifications::new(cx.foreground_executor().clone());
    let app = Rc::downgrade(&cx.app);
    cx.update(|cx| {
        let sleeps = events.clone();
        let power = power.clone();
        cx.on_system_sleep(move |_| {
            sleeps.borrow_mut().push("sleep");
            if sleeps.borrow().len() == 1
                && let Some(app) = app.upgrade()
            {
                sleep(&power, &app);
            }
        })
        .detach();
        let wakes = events.clone();
        cx.on_system_wake(move |_| wakes.borrow_mut().push("wake"))
            .detach();
    });
    {
        let _held = cx.app.borrow_mut();
        sleep(&power, &cx.app);
    }
    wake(&power, &cx.app);
    cx.run_until_parked();
    assert_eq!(*events.borrow(), ["sleep", "wake", "sleep"]);
}
