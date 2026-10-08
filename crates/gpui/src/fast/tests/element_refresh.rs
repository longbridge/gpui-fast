//! Tests of drawing again only the view an element is in when its own
//! interaction state changes. See [`crate::fast::element_refresh`].

use crate::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, Modifiers, MouseButton,
    MouseDownEvent, MouseUpEvent, ParentElement as _, PlatformInput, Render,
    StatefulInteractiveElement as _, Styled as _, TestAppContext, Window, WindowHandle, div, point,
    px, rgb,
};
use std::{cell::Cell, rc::Rc};

/// A view counting its builds, showing `shared` as it renders.
struct Sibling {
    builds: Rc<Cell<usize>>,
    shared: Rc<Cell<u32>>,
}

impl Render for Sibling {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.builds.set(self.builds.get() + 1);
        div()
            .h(px(20.))
            .bg(rgb(0x202020 + self.shared.get()))
            .child(format!("sibling {}", self.shared.get()))
    }
}

/// A view with a button whose `active` style shows while it is pressed. Its
/// click either changes nothing, or sets `shared`, which the sibling reads
/// outside entities, and refreshes the window, as an application does to
/// say such state changed.
struct Button {
    builds: Rc<Cell<usize>>,
    clicks: Rc<Cell<u32>>,
    shared: Option<Rc<Cell<u32>>>,
}

impl Render for Button {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.builds.set(self.builds.get() + 1);
        let clicks = self.clicks.clone();
        let shared = self.shared.clone();
        div()
            .id("button")
            .h(px(20.))
            .w(px(100.))
            .bg(rgb(0x0000ff))
            .active(|style| style.bg(rgb(0xff0000)))
            .on_click(move |_, window, _| {
                clicks.set(clicks.get() + 1);
                if let Some(shared) = &shared {
                    shared.set(shared.get() + 1);
                    window.refresh();
                }
            })
            .child("button")
    }
}

struct Page {
    button: Entity<Button>,
    sibling: Entity<Sibling>,
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .child(self.button.clone())
            .child(self.sibling.clone())
    }
}

struct Counts {
    button_builds: Rc<Cell<usize>>,
    sibling_builds: Rc<Cell<usize>>,
    clicks: Rc<Cell<u32>>,
}

fn open(cx: &mut TestAppContext, refreshes: bool, retention: bool) -> (WindowHandle<Page>, Counts) {
    let counts = Counts {
        button_builds: Rc::new(Cell::new(0)),
        sibling_builds: Rc::new(Cell::new(0)),
        clicks: Rc::new(Cell::new(0)),
    };
    let shared = Rc::new(Cell::new(0));
    let window = cx.add_window({
        let button_builds = counts.button_builds.clone();
        let sibling_builds = counts.sibling_builds.clone();
        let clicks = counts.clicks.clone();
        move |_, cx| Page {
            button: cx.new(|_| Button {
                builds: button_builds,
                clicks,
                shared: refreshes.then(|| shared.clone()),
            }),
            sibling: cx.new(|_| Sibling {
                builds: sibling_builds,
                shared,
            }),
        }
    });
    cx.update_window(window.into(), |_, window, cx| {
        window.set_view_retention(retention);
        window.draw(cx).clear(cx);
    })
    .unwrap();
    (window, counts)
}

/// Dispatches `event` and draws the frame that follows, returning what it
/// painted.
fn dispatch(
    cx: &mut TestAppContext,
    window: WindowHandle<Page>,
    event: PlatformInput,
) -> Vec<String> {
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_event(event, cx);
        window.draw(cx).clear(cx);
        window.painted_primitives()
    })
    .unwrap()
}

fn press() -> PlatformInput {
    PlatformInput::MouseDown(MouseDownEvent {
        button: MouseButton::Left,
        position: point(px(10.), px(10.)),
        modifiers: Modifiers::default(),
        click_count: 1,
        first_mouse: false,
    })
}

fn release() -> PlatformInput {
    PlatformInput::MouseUp(MouseUpEvent {
        button: MouseButton::Left,
        position: point(px(10.), px(10.)),
        modifiers: Modifiers::default(),
        click_count: 1,
    })
}

/// Pressing and releasing a button changes only its own state, its `active`
/// style: its view is built again, and its sibling is drawn from the last
/// frame, as nothing it depends on changed. Both frames draw what a window
/// without retention draws.
#[test]
fn a_press_builds_only_the_view_of_the_pressed_element() {
    let mut cx = TestAppContext::single();
    let (window, counts) = open(&mut cx, false, true);
    let (plain, _) = open(&mut cx, false, false);
    assert_eq!(
        (counts.button_builds.get(), counts.sibling_builds.get()),
        (1, 1)
    );

    let pressed = dispatch(&mut cx, window, press());
    assert_eq!(pressed, dispatch(&mut cx, plain, press()));
    assert_eq!(
        (counts.button_builds.get(), counts.sibling_builds.get()),
        (2, 1),
        "the press builds the button's view again, not its sibling"
    );

    let released = dispatch(&mut cx, window, release());
    assert_eq!(released, dispatch(&mut cx, plain, release()));
    assert_ne!(pressed, released, "the active style shows while pressed");
    assert_eq!(counts.clicks.get(), 1);
    assert_eq!(
        (counts.button_builds.get(), counts.sibling_builds.get()),
        (3, 1),
        "the release builds the button's view again, not its sibling"
    );
    let stats = cx
        .update_window(window.into(), |_, window, _| window.layout_stats())
        .unwrap();
    assert!(stats.views_reused > 0);
}

/// A refresh the application asks for still draws every view again: the
/// click changes state outside entities that the sibling reads, and only
/// the refresh says so.
#[test]
fn a_refresh_the_application_asks_for_builds_every_view() {
    let mut cx = TestAppContext::single();
    let (window, counts) = open(&mut cx, true, true);
    let (plain, _) = open(&mut cx, true, false);

    assert_eq!(
        dispatch(&mut cx, window, press()),
        dispatch(&mut cx, plain, press())
    );
    assert_eq!(counts.sibling_builds.get(), 1);
    let released = dispatch(&mut cx, window, release());
    assert_eq!(released, dispatch(&mut cx, plain, release()));
    assert_eq!(counts.clicks.get(), 1);
    assert_eq!(
        counts.sibling_builds.get(),
        2,
        "the sibling reads what the click changed"
    );
}
