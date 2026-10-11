// Modified by Longbridge for gpui-fast.
//! Tests of `ScrollHandle::scroll_to_bottom` asked of a handle already at its
//! bottom (`fast::layers::scroll_to_bottom`).

use std::{cell::Cell, rc::Rc};

use crate::{
    AnyWindowHandle, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, ScrollHandle, StatefulInteractiveElement as _, Styled as _,
    TestAppContext, Window, div, point, px, rgb,
};

/// A 100 px tall scroll container of forty 20 px rows at the top left of the
/// window, and a view beside it that reads its offset.
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

/// A view that shows the offset of a scroll handle, counting its renders.
struct Reader {
    handle: ScrollHandle,
    renders: Rc<Cell<usize>>,
}

impl Render for Reader {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.renders.set(self.renders.get() + 1);
        let height = 10. - f32::from(self.handle.offset().y) / 100.;
        div().w(px(10.)).h(px(height))
    }
}

fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
}

#[crate::test]
fn scrolling_a_handle_to_its_bottom_where_it_is_changes_nothing(cx: &mut TestAppContext) {
    if !crate::fast::layers::COMPILED {
        return;
    }
    let handle = ScrollHandle::new();
    let renders = Rc::new(Cell::new(0));
    let window: AnyWindowHandle = cx
        .add_window({
            let handle = handle.clone();
            let renders = renders.clone();
            move |_, cx| Page {
                handle: handle.clone(),
                reader: cx.new(|_| Reader { handle, renders }),
            }
        })
        .into();
    draw(cx, window);
    draw(cx, window);
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
