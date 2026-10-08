//! Tests of questions about where a scroll handle is scrolled to, recorded
//! with their answers (`fast::layers::answers`), and of scroll requests that
//! leave a handle where it is.

use std::{cell::Cell, rc::Rc};

use crate::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, ScrollDelta, ScrollHandle, ScrollWheelEvent,
    StatefulInteractiveElement as _, Styled as _, TestAppContext, TouchPhase, Window, div, point,
    px, rgb,
};

/// A 100 px tall scroll container of forty 20 px rows at the top left of the
/// window, and a view beside it that reads where it is scrolled.
struct Page {
    handle: ScrollHandle,
    reader: Entity<Reader>,
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(
                div()
                    .id("scroller")
                    .overflow_y_scroll()
                    .track_scroll(&self.handle)
                    .w(px(200.))
                    .h(px(100.))
                    .children((0..40).map(|row| div().h(px(20.)).bg(rgb(0x100000 + row * 0x10)))),
            )
            .child(self.reader.clone())
    }
}

/// What [`Reader`] reads of the scroll handle.
#[derive(Clone, Copy)]
enum Read {
    /// Which row shows at its top.
    TopItem,
    /// Its offset.
    Offset,
}

/// A view that shows what it reads of a scroll handle, counting its renders.
struct Reader {
    handle: ScrollHandle,
    read: Read,
    renders: Rc<Cell<usize>>,
}

impl Render for Reader {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.renders.set(self.renders.get() + 1);
        let height = match self.read {
            Read::TopItem => 10. + self.handle.top_item() as f32,
            Read::Offset => 10. - f32::from(self.handle.offset().y) / 100.,
        };
        div().w(px(10.)).h(px(height))
    }
}

fn with_window<R>(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    f: impl FnOnce(&mut Window, &mut App) -> R,
) -> R {
    cx.update_window(window, |_, window, cx| f(window, cx))
        .unwrap()
}

fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
    with_window(cx, window, |window, cx| window.draw(cx).clear(cx));
}

fn wheel(cx: &mut TestAppContext, window: AnyWindowHandle, dy: f32) {
    with_window(cx, window, |window, cx| {
        window.dispatch_event(
            crate::PlatformInput::ScrollWheel(ScrollWheelEvent {
                position: point(px(20.), px(20.)),
                delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
                modifiers: Default::default(),
                touch_phase: TouchPhase::Moved,
            }),
            cx,
        );
    });
    draw(cx, window);
}

/// A [`Page`] whose reader reads `read`, drawn twice, with its scroll handle
/// and the count of the reader's renders.
fn page(cx: &mut TestAppContext, read: Read) -> (AnyWindowHandle, ScrollHandle, Rc<Cell<usize>>) {
    let handle = ScrollHandle::new();
    let renders = Rc::new(Cell::new(0));
    let window: AnyWindowHandle = cx
        .add_window({
            let handle = handle.clone();
            let renders = renders.clone();
            move |_, cx| Page {
                handle: handle.clone(),
                reader: cx.new(|_| Reader {
                    handle,
                    read,
                    renders,
                }),
            }
        })
        .into();
    draw(cx, window);
    draw(cx, window);
    (window, handle, renders)
}

#[crate::test]
fn a_view_asking_which_row_shows_at_the_top_is_built_again_only_when_another_does(
    cx: &mut TestAppContext,
) {
    if !crate::fast::layers::COMPILED {
        return;
    }
    let (window, handle, renders) = page(cx, Read::TopItem);
    let before = renders.get();
    // Row 0 spans 0 to 20 px.
    wheel(cx, window, -5.);
    wheel(cx, window, -5.);
    assert_eq!(handle.top_item(), 0);
    assert_eq!(renders.get(), before, "row 0 still shows at the top");
    wheel(cx, window, -20.);
    assert_eq!(handle.top_item(), 1);
    assert_eq!(renders.get(), before + 1, "row 1 shows at the top now");
    draw(cx, window);
    assert_eq!(renders.get(), before + 1, "and is reused once it has");
}

#[crate::test]
fn a_view_reading_the_offset_is_built_again_on_every_scroll(cx: &mut TestAppContext) {
    if !crate::fast::layers::COMPILED {
        return;
    }
    let (window, _, renders) = page(cx, Read::Offset);
    let before = renders.get();
    wheel(cx, window, -5.);
    wheel(cx, window, -5.);
    assert_eq!(renders.get(), before + 2);
}

#[crate::test]
fn scrolling_a_handle_to_its_bottom_where_it_is_changes_nothing(cx: &mut TestAppContext) {
    if !crate::fast::layers::COMPILED {
        return;
    }
    let (window, handle, renders) = page(cx, Read::Offset);
    handle.scroll_to_bottom();
    draw(cx, window);
    draw(cx, window);
    assert_eq!(handle.offset().y, -handle.max_offset().y);
    assert!(handle.max_offset().y > px(0.));
    let version = handle.0.borrow().version.get();
    let before = renders.get();
    handle.scroll_to_bottom();
    assert_eq!(handle.0.borrow().version.get(), version);
    draw(cx, window);
    assert_eq!(
        renders.get(),
        before,
        "nothing that read the offset is built again"
    );
    assert_eq!(
        handle.offset().y,
        -handle.max_offset().y,
        "it stays at its bottom"
    );

    handle.set_offset(point(px(0.), px(-20.)));
    let version = handle.0.borrow().version.get();
    handle.scroll_to_bottom();
    assert_ne!(handle.0.borrow().version.get(), version, "it moves");
    draw(cx, window);
    assert_eq!(handle.offset().y, -handle.max_offset().y);
}
