//! Tests of when an observer hears a change. See [`crate::fast::observers`].

use std::{cell::Cell, rc::Rc};

use crate::{App, Global, TestApp};

struct Watched;

impl Global for Watched {}

struct Watcher;

/// What an observer watches, and which constructor makes it.
#[derive(Clone, Copy, Debug)]
enum Watch {
    /// `App::observe_global`.
    Global,
    /// `Context::observe_global`.
    GlobalFromEntity,
    /// `App::observe`.
    Entity,
}

/// An observer made while a notice of its global or entity waits in the
/// effect queue hears a change made after it was made, which joined that
/// notice, and does not hear one made only before. An observer made before
/// the update hears its changes once.
#[test]
fn an_observer_hears_a_change_made_after_it_was_made_behind_a_waiting_notice() {
    // (what is watched, whether it changes again after the observer is made,
    // the calls the observer hears)
    for (watch, change_after, heard) in [
        (Watch::Global, true, 1),
        (Watch::Global, false, 0),
        (Watch::GlobalFromEntity, true, 1),
        (Watch::GlobalFromEntity, false, 0),
        (Watch::Entity, true, 1),
        (Watch::Entity, false, 0),
    ] {
        let mut app = TestApp::new();
        app.set_global(Watched);
        let watched = app.new_entity(|_| Watched);
        let watcher = app.new_entity(|_| Watcher);

        let earlier_calls = Rc::new(Cell::new(0));
        let counter = earlier_calls.clone();
        let _earlier = app.update(|cx| match watch {
            Watch::Entity => cx.observe(&watched, move |_, _| counter.set(counter.get() + 1)),
            Watch::Global | Watch::GlobalFromEntity => {
                cx.observe_global::<Watched>(move |_| counter.set(counter.get() + 1))
            }
        });

        let calls = Rc::new(Cell::new(0));
        app.update(|cx| {
            let change = |cx: &mut App| match watch {
                Watch::Entity => watched.update(cx, |_, cx| cx.notify()),
                Watch::Global | Watch::GlobalFromEntity => cx.set_global(Watched),
            };
            change(cx);
            let counter = calls.clone();
            match watch {
                Watch::Global => cx
                    .observe_global::<Watched>(move |_| counter.set(counter.get() + 1))
                    .detach(),
                Watch::GlobalFromEntity => watcher.update(cx, |_, cx| {
                    cx.observe_global::<Watched>(move |_, _| counter.set(counter.get() + 1))
                        .detach()
                }),
                Watch::Entity => cx
                    .observe(&watched, move |_, _| counter.set(counter.get() + 1))
                    .detach(),
            }
            if change_after {
                change(cx);
            }
        });

        assert_eq!(
            calls.get(),
            heard,
            "{watch:?}, changed again after the observer was made: {change_after}"
        );
        assert_eq!(
            earlier_calls.get(),
            1,
            "{watch:?}: an observer made before the update hears it once"
        );
    }
}

/// An entity that observes another with `Context::observe`, made while a
/// notice of that entity waits, hears a `Context::notify` made after it was
/// made, and not one made only before.
#[test]
fn an_entity_observer_hears_a_notify_made_after_it_was_made_behind_a_waiting_notice() {
    // (whether the observed entity notifies again after the observer is
    // made, the calls the observer hears)
    for (notify_after, heard) in [(true, 1), (false, 0)] {
        let mut app = TestApp::new();
        let watched = app.new_entity(|_| Watched);
        let watcher = app.new_entity(|_| Watcher);

        let calls = Rc::new(Cell::new(0));
        app.update(|cx| {
            watched.update(cx, |_, cx| cx.notify());
            let counter = calls.clone();
            watcher.update(cx, |_, cx| {
                cx.observe(&watched, move |_, _, _| counter.set(counter.get() + 1))
                    .detach()
            });
            if notify_after {
                watched.update(cx, |_, cx| cx.notify());
            }
        });

        assert_eq!(
            calls.get(),
            heard,
            "notified again after the observer was made: {notify_after}"
        );
    }
}
