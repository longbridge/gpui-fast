//! A press before a tooltip's show delay ends cancels the pending tooltip
//! (gpui-kit#4073): the press may open a menu over the element, and the
//! delayed tooltip would otherwise be drawn on top of it.

use std::{cell::Cell, rc::Rc, time::Duration};

use crate::prelude::FluentBuilder as _;

use crate::{
    AppContext, Context, InputEvent, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, ParentElement, Pixels, Point, Render, StatefulInteractiveElement, Styled,
    TestAppContext, Window, div, point, px,
};
/// Longer than the default show delay (500 ms).
const PAST_DELAY: Duration = Duration::from_secs(1);

struct Tip;

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child("tip")
    }
}

/// A button with a tooltip that opens an occluding menu on mouse-down.
struct MenuButton {
    builds: Rc<Cell<usize>>,
    menu_open: bool,
}

impl Render for MenuButton {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let builds = self.builds.clone();
        div()
            .size_full()
            .child(
                div()
                    .id("button")
                    .size(px(50.))
                    .tooltip(move |_, cx| {
                        builds.set(builds.get() + 1);
                        cx.new(|_| Tip).into()
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.menu_open = true;
                            cx.notify();
                        }),
                    ),
            )
            .when(self.menu_open, |root| {
                root.child(div().absolute().top_0().left_0().size_full().occlude())
            })
    }
}

fn move_to(window: &mut Window, cx: &mut crate::App, position: Point<Pixels>) {
    window.dispatch_event(
        MouseMoveEvent {
            position,
            ..Default::default()
        }
        .to_platform_input(),
        cx,
    );
    window.draw(cx).clear(cx);
}

fn run(cx: &mut TestAppContext, press: bool) -> usize {
    let builds = Rc::new(Cell::new(0));
    let window = cx.add_window({
        let builds = builds.clone();
        |_, _| MenuButton {
            builds,
            menu_open: false,
        }
    });
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        move_to(window, cx, point(px(10.), px(10.)));
        if press {
            window.dispatch_event(
                MouseDownEvent {
                    button: MouseButton::Left,
                    position: point(px(10.), px(10.)),
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.draw(cx).clear(cx);
        }
    })
    .unwrap();
    cx.executor().advance_clock(PAST_DELAY);
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        // The first move after the press, as in the report.
        move_to(window, cx, point(px(11.), px(10.)));
    })
    .unwrap();
    cx.executor().advance_clock(PAST_DELAY);
    cx.run_until_parked();
    builds.get()
}

#[crate::test]
fn test_press_cancels_pending_tooltip(cx: &mut TestAppContext) {
    assert_eq!(run(cx, true), 0);
}

#[crate::test]
fn test_tooltip_shows_without_press(cx: &mut TestAppContext) {
    assert!(run(cx, false) > 0);
}
