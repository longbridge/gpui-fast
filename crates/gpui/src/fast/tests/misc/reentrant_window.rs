//! Tests of [`crate::fast::misc::reentrant_window`]: window callbacks the
//! platform delivers while the app is borrowed, as the Windows window
//! procedure does when it is re-entered during a draw.

use std::{cell::RefCell, rc::Rc};

use crate::{
    Context, DisplayId, InteractiveElement as _, IntoElement, Modifiers, MouseButton,
    MouseDownEvent, PlatformInput, Render, Styled as _, Subscription, TestAppContext, Window,
    WindowHandle, div, point, px, size,
};

struct Recorder {
    events: Rc<RefCell<Vec<String>>>,
    _bounds: Option<Subscription>,
}

impl Render for Recorder {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self._bounds.is_none() {
            let events = self.events.clone();
            self._bounds = Some(cx.observe_window_bounds(window, move |_, window, _| {
                events
                    .borrow_mut()
                    .push(format!("bounds {}", window.bounds().size.width));
            }));
        }
        let events = self.events.clone();
        div()
            .size_full()
            .on_mouse_down(MouseButton::Left, move |event, _, _| {
                events
                    .borrow_mut()
                    .push(format!("click {}", event.position.x));
            })
    }
}

fn open(cx: &mut TestAppContext) -> (WindowHandle<Recorder>, Rc<RefCell<Vec<String>>>) {
    let events = Rc::new(RefCell::new(Vec::new()));
    let window = cx.add_window({
        let events = events.clone();
        move |_, _| Recorder {
            events,
            _bounds: None,
        }
    });
    cx.run_until_parked();
    events.borrow_mut().clear();
    (window, events)
}

fn click(x: f32) -> PlatformInput {
    PlatformInput::MouseDown(MouseDownEvent {
        button: MouseButton::Left,
        position: point(px(x), px(10.)),
        modifiers: Modifiers::default(),
        click_count: 1,
        first_mouse: false,
    })
}

#[crate::test]
fn input_while_the_app_is_free_dispatches_at_once(cx: &mut TestAppContext) {
    let (window, events) = open(cx);
    let mut platform = cx.test_window(window.into());
    platform.simulate_input(click(5.));
    assert_eq!(*events.borrow(), ["click 5px"]);
}

#[crate::test]
fn input_during_a_borrow_is_dispatched_in_order_after_it(cx: &mut TestAppContext) {
    let (window, events) = open(cx);
    let mut platform = cx.test_window(window.into());
    {
        let _held = cx.app.borrow_mut();
        platform.simulate_input(click(1.));
        platform.simulate_input(click(2.));
    }
    assert!(events.borrow().is_empty());
    // Input arriving after the borrow ends but before the retry waits for the
    // queued input, so the order is kept.
    platform.simulate_input(click(3.));
    assert_eq!(*events.borrow(), ["click 1px", "click 2px", "click 3px"]);
    cx.run_until_parked();
    assert_eq!(events.borrow().len(), 3);
}

#[crate::test]
fn queued_input_is_delivered_on_a_later_turn(cx: &mut TestAppContext) {
    let (window, events) = open(cx);
    let mut platform = cx.test_window(window.into());
    {
        let _held = cx.app.borrow_mut();
        platform.simulate_input(click(1.));
    }
    cx.run_until_parked();
    assert_eq!(*events.borrow(), ["click 1px"]);
}

#[crate::test]
fn resizes_during_a_borrow_are_coalesced(cx: &mut TestAppContext) {
    let (window, events) = open(cx);
    let mut platform = cx.test_window(window.into());
    {
        let _held = cx.app.borrow_mut();
        platform.simulate_resize(size(px(300.), px(200.)));
        platform.simulate_resize(size(px(320.), px(200.)));
        platform.simulate_resize(size(px(340.), px(200.)));
    }
    assert!(events.borrow().is_empty());
    cx.run_until_parked();
    assert_eq!(*events.borrow(), ["bounds 340px"]);
}

#[crate::test]
fn display_change_during_a_borrow_does_not_panic(cx: &mut TestAppContext) {
    let (window, _events) = open(cx);
    let platform = cx.test_window(window.into());
    {
        let _held = cx.app.borrow_mut();
        platform.simulate_move_to_display(DisplayId(7));
    }
    cx.run_until_parked();
    let display = window
        .update(cx, |_, window, _| {
            window.platform_window.display().map(|display| display.id())
        })
        .unwrap();
    assert_eq!(display, Some(DisplayId(7)));
}
