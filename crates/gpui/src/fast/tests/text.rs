//! Tests of how text is measured and kept across frames. See
//! [`crate::fast::text`].

use crate::{
    AnyWindowHandle, AppContext as _, Context, IntoElement, LayoutStats, ParentElement as _,
    Render, SharedString, Styled as _, TestAppContext, Window, div, px,
};

/// Rows that each show text of their own, matched to their nodes by
/// position.
struct ShiftingRows {
    row_ids: Vec<u64>,
}

impl Render for ShiftingRows {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .children(self.row_ids.iter().map(|id| {
                div()
                    .h(px(20.))
                    .child(SharedString::from(format!("row {id}")))
            }))
    }
}

fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) -> LayoutStats {
    cx.update_window(window, |_, window, cx| {
        window.draw(cx).clear(cx);
        window.layout_stats()
    })
    .unwrap()
}

fn change_and_draw(
    cx: &mut TestAppContext,
    window: crate::WindowHandle<ShiftingRows>,
    change: impl FnOnce(&mut ShiftingRows),
) -> LayoutStats {
    // Counted from before the change, which can draw a frame of its own.
    cx.update_window(window.into(), |_, window, _| window.reset_layout_stats())
        .unwrap();
    window
        .update(cx, |view, _, cx| {
            change(view);
            cx.notify();
        })
        .unwrap();
    draw(cx, window.into())
}

/// A text node that takes over last frame's measurement answers from the
/// lines it already holds and never asks the line layout cache for them.
/// Those lines still have to stay in the cache: when unidentified rows shift
/// by one, every row lands on a neighbour's node, and the text it brings was
/// on screen all along.
#[test]
fn text_kept_by_its_node_is_not_reshaped_when_rows_shift_onto_other_nodes() {
    let mut cx = TestAppContext::single();
    let window = cx.add_window(|_, _| ShiftingRows {
        row_ids: (0..8).collect(),
    });
    // Enough frames for anything only the first frame asked the cache for
    // to have been forgotten, had nobody asked since.
    for _ in 0..3 {
        window.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
        draw(&mut cx, window.into());
    }

    let shifted = change_and_draw(&mut cx, window, |view| {
        view.row_ids.remove(0);
    });
    assert!(
        shifted.measure_calls > 0,
        "rows should have moved onto other nodes for this to test anything: {shifted:?}"
    );
    assert_eq!(
        shifted.lines_shaped, 0,
        "every row's text was on screen the frame before and should come from the cache: {shifted:?}"
    );
}

/// Lines outlive the frames that asked for them only while something holds
/// them. Once the text is gone from the tree, and with it the nodes that held
/// its lines, the cache has to let them go too.
#[test]
fn text_nothing_holds_any_more_leaves_the_line_layout_cache() {
    let mut cx = TestAppContext::single();
    let window = cx.add_window(|_, _| ShiftingRows {
        row_ids: (0..8).collect(),
    });
    draw(&mut cx, window.into());
    change_and_draw(&mut cx, window, |view| view.row_ids.clear());
    for _ in 0..3 {
        window.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
        draw(&mut cx, window.into());
    }

    let shown_again = change_and_draw(&mut cx, window, |view| {
        view.row_ids = (0..8).collect();
    });
    assert_eq!(
        shown_again.lines_shaped, 8,
        "text removed frames ago should have left the cache: {shown_again:?}"
    );
}
