//! Tests of `fast::list_measure`: a `measure_all` list measures the items it
//! does not show over later frames after a width change.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use crate::fast::list_measure::DEFERRED_MEASURE_BUDGET;
use crate::{
    AnyElement, AppContext as _, Context, Element as _, IntoElement, ListOffset, ListState, Pixels,
    Render, ScrollDelta, ScrollEvent, Styled as _, TestAppContext, VisualTestContext, Window,
    canvas, div, list, point, px, size,
};

fn painted_item(
    index: usize,
    height: f32,
    painted: &Rc<RefCell<Vec<(usize, Pixels, Pixels)>>>,
) -> AnyElement {
    let painted = painted.clone();
    canvas(
        |_, _, _| {},
        move |bounds, _, _, _| {
            painted
                .borrow_mut()
                .push((index, bounds.top(), bounds.bottom()));
        },
    )
    .h(px(height))
    .w_full()
    .into_any()
}

fn list_offset(item_ix: usize, offset: f32) -> ListOffset {
    ListOffset {
        item_ix,
        offset_in_item: px(offset),
    }
}

struct TestView {
    state: ListState,
    render_item: Rc<dyn Fn(usize) -> AnyElement>,
}

impl Render for TestView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let render_item = self.render_item.clone();
        list(self.state.clone(), move |index, _, _| render_item(index))
            .w_full()
            .h_full()
    }
}

/// The height of item `ix` of a list laid out `width` wide: every third
/// item is a fixed-height row, the rest wrap like text, growing as the
/// width shrinks.
fn reflowing_height(ix: usize, width: f32) -> f32 {
    if ix.is_multiple_of(3) {
        30.
    } else {
        ((20 + (ix % 7) * 10) as f32 * 100. / width).round()
    }
}

/// A `measure_all` list of `count` reflowing items, the width they are laid
/// out at, and the indices of the items rendered so far.
fn reflowing_list(
    count: usize,
    overdraw: f32,
) -> (ListState, Rc<Cell<f32>>, Rc<RefCell<Vec<usize>>>) {
    let state = ListState::new(count, crate::ListAlignment::Top, px(overdraw)).measure_all();
    (
        state,
        Rc::new(Cell::new(100.)),
        Rc::new(RefCell::new(Vec::new())),
    )
}

/// Draws `state` `width` wide and 200px tall, recording each item it renders
/// and where it paints.
fn draw_reflowing(
    state: &ListState,
    width: f32,
    layout_width: &Rc<Cell<f32>>,
    rendered: &Rc<RefCell<Vec<usize>>>,
    painted: &Rc<RefCell<Vec<(usize, Pixels, Pixels)>>>,
    cx: &mut VisualTestContext,
) {
    layout_width.set(width);
    let layout_width = layout_width.clone();
    let rendered = rendered.clone();
    let painted = painted.clone();
    cx.draw(point(px(0.), px(0.)), size(px(width), px(200.)), |_, cx| {
        cx.new(|_| TestView {
            state: state.clone(),
            render_item: Rc::new(move |ix| {
                rendered.borrow_mut().push(ix);
                painted_item(ix, reflowing_height(ix, layout_width.get()), &painted)
            }),
        })
        .into_any_element()
    });
}

fn exact_height(range: std::ops::Range<usize>, width: f32) -> f32 {
    range.map(|ix| reflowing_height(ix, width)).sum()
}

fn total_height(state: &ListState) -> f32 {
    f32::from(state.0.borrow().items.summary().height)
}

fn unmeasured(state: &ListState) -> usize {
    state.0.borrow().items.summary().unrendered_count
}

#[gpui::test]
fn test_width_change_measures_only_around_the_viewport_first(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let (state, width, rendered) = reflowing_list(1000, 100.);
    let painted = Rc::new(RefCell::new(Vec::new()));

    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    assert_eq!(unmeasured(&state), 0);
    assert!(
        rendered.borrow().len() >= 1000,
        "every item is measured first"
    );

    state.scroll_to(list_offset(500, 0.));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    rendered.borrow_mut().clear();

    // The frame with the new width lays out what it shows and its overdraw.
    draw_reflowing(&state, 160., &width, &rendered, &painted, cx);
    let rendered = rendered.borrow();
    assert!(
        rendered.iter().all(|ix| (490..530).contains(ix)),
        "only items around the viewport are laid out: {rendered:?}"
    );
    assert!(unmeasured(&state) > 950);
    assert_eq!(state.logical_scroll_top(), list_offset(500, 0.));

    // The others keep estimates scaled by how the items measured reflowed,
    // so the scrollbar stays near the exact height.
    let exact = exact_height(0..1000, 160.);
    let total = total_height(&state);
    assert!(
        (total - exact).abs() < exact * 0.2,
        "estimated total {total} is far from the exact {exact}"
    );
}

#[gpui::test]
fn test_width_change_converges_to_exact_heights(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let (state, width, rendered) = reflowing_list(60, 0.);
    let painted = Rc::new(RefCell::new(Vec::new()));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    assert_eq!(total_height(&state), exact_height(0..60, 100.));

    // One item per frame, for a count that does not depend on the clock.
    state.0.borrow_mut().fast_measure.budget = Duration::ZERO;
    draw_reflowing(&state, 250., &width, &rendered, &painted, cx);
    let after_resize = unmeasured(&state);
    assert!(after_resize > 40);

    let mut frames = 0;
    while state.0.borrow().fast_measure.next.is_some() {
        let before = unmeasured(&state);
        draw_reflowing(&state, 250., &width, &rendered, &painted, cx);
        assert_eq!(unmeasured(&state), before - 1, "one item each frame");
        frames += 1;
        assert!(frames <= 60);
    }
    assert_eq!(frames, after_resize);
    assert_eq!(unmeasured(&state), 0);
    assert_eq!(total_height(&state), exact_height(0..60, 250.));
    assert_eq!(
        state.max_offset_for_scrollbar().y,
        px(exact_height(0..60, 250.) - 200.)
    );

    // With the default budget, a frame measures as many as fit in it.
    state.0.borrow_mut().fast_measure.budget = DEFERRED_MEASURE_BUDGET;
    draw_reflowing(&state, 120., &width, &rendered, &painted, cx);
    let mut frames = 0;
    while state.0.borrow().fast_measure.next.is_some() {
        draw_reflowing(&state, 120., &width, &rendered, &painted, cx);
        frames += 1;
        assert!(frames <= 60);
    }
    assert_eq!(unmeasured(&state), 0);
    assert!(state.0.borrow().fast_measure.next.is_none());
    assert_eq!(total_height(&state), exact_height(0..60, 120.));
}

#[gpui::test]
fn test_width_change_keeps_the_scroll_anchor(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let (state, width, rendered) = reflowing_list(200, 50.);
    let painted = Rc::new(RefCell::new(Vec::new()));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    state.0.borrow_mut().fast_measure.budget = Duration::ZERO;

    // Halfway into item 100, which reflows.
    assert_ne!(100 % 3, 0);
    let old_height = reflowing_height(100, 100.);
    state.scroll_to(list_offset(100, old_height / 2.));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);

    painted.borrow_mut().clear();
    draw_reflowing(&state, 50., &width, &rendered, &painted, cx);
    let new_height = reflowing_height(100, 50.);
    assert_ne!(new_height, old_height);
    // The same fraction of the item stays above the viewport.
    assert_eq!(
        state.logical_scroll_top(),
        list_offset(100, new_height / 2.)
    );
    assert_eq!(
        painted.borrow().first().copied(),
        Some((100, px(-new_height / 2.), px(new_height / 2.)))
    );

    // Measuring the items above it moves the scrollbar, not the content.
    while state.0.borrow().fast_measure.next.is_some() {
        painted.borrow_mut().clear();
        draw_reflowing(&state, 50., &width, &rendered, &painted, cx);
        assert_eq!(
            state.logical_scroll_top(),
            list_offset(100, new_height / 2.)
        );
        assert_eq!(
            painted.borrow().first().copied(),
            Some((100, px(-new_height / 2.), px(new_height / 2.)))
        );
    }
    assert_eq!(
        state.scroll_px_offset_for_scrollbar().y,
        -px(exact_height(0..100, 50.) + new_height / 2.)
    );
}

#[gpui::test]
fn test_scroll_to_item_after_width_change(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let (state, width, rendered) = reflowing_list(200, 50.);
    let painted = Rc::new(RefCell::new(Vec::new()));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    state.0.borrow_mut().fast_measure.budget = Duration::ZERO;
    draw_reflowing(&state, 180., &width, &rendered, &painted, cx);
    assert!(state.0.borrow().fast_measure.next.is_some());

    // A jump to an item not yet measured shows it at the top at once.
    state.scroll_to(list_offset(150, 0.));
    painted.borrow_mut().clear();
    draw_reflowing(&state, 180., &width, &rendered, &painted, cx);
    let top = |painted: &Rc<RefCell<Vec<(usize, Pixels, Pixels)>>>| {
        painted.borrow().first().map(|&(ix, top, _)| (ix, top))
    };
    assert_eq!(top(&painted), Some((150, px(0.))));

    while state.0.borrow().fast_measure.next.is_some() {
        painted.borrow_mut().clear();
        draw_reflowing(&state, 180., &width, &rendered, &painted, cx);
        assert_eq!(top(&painted), Some((150, px(0.))));
    }
    assert_eq!(
        state.scroll_px_offset_for_scrollbar().y,
        -px(exact_height(0..150, 180.))
    );

    // Once measured, revealing an item below lands it at the bottom edge.
    state.scroll_to_reveal_item(170);
    draw_reflowing(&state, 180., &width, &rendered, &painted, cx);
    let bounds = state.bounds_for_item(170).expect("item 170 is shown");
    assert_eq!(bounds.bottom(), px(200.));
    assert_eq!(
        state.scroll_px_offset_for_scrollbar().y,
        -px(exact_height(0..171, 180.) - 200.)
    );
}

#[gpui::test]
fn test_remeasure_during_deferred_measurement_measures_all(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let (state, width, rendered) = reflowing_list(100, 0.);
    let painted = Rc::new(RefCell::new(Vec::new()));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    state.0.borrow_mut().fast_measure.budget = Duration::ZERO;
    draw_reflowing(&state, 140., &width, &rendered, &painted, cx);
    assert!(unmeasured(&state) > 0);

    // `remeasure_items` still re-arms the full measurement, which ends the
    // deferred pass with every item exact in one frame.
    state.remeasure_items(0..100);
    draw_reflowing(&state, 140., &width, &rendered, &painted, cx);
    assert_eq!(unmeasured(&state), 0);
    assert!(state.0.borrow().fast_measure.next.is_none());
    assert_eq!(total_height(&state), exact_height(0..100, 140.));
}

fn frames_requested(cx: &mut VisualTestContext) -> usize {
    cx.update(|window, _| window.next_frame_callbacks.borrow_mut().drain(..).count())
}

fn deferred(state: &ListState) -> Option<usize> {
    state.0.borrow().fast_measure.next
}

/// Draws until no frame is requested, returning how many frames ran; fails
/// if it does not settle within `limit` frames.
fn settle(
    state: &ListState,
    width: f32,
    layout_width: &Rc<Cell<f32>>,
    rendered: &Rc<RefCell<Vec<usize>>>,
    painted: &Rc<RefCell<Vec<(usize, Pixels, Pixels)>>>,
    cx: &mut VisualTestContext,
    limit: usize,
    mut each: impl FnMut(&ListState, &Rc<RefCell<Vec<(usize, Pixels, Pixels)>>>),
) -> usize {
    let mut frames = 0;
    loop {
        painted.borrow_mut().clear();
        draw_reflowing(state, width, layout_width, rendered, painted, cx);
        frames += 1;
        each(state, painted);
        if frames_requested(cx) == 0 {
            return frames;
        }
        assert!(frames <= limit, "still requesting frames after {frames}");
    }
}

#[gpui::test]
fn test_width_change_twice_before_convergence(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let (state, width, rendered) = reflowing_list(300, 50.);
    let painted = Rc::new(RefCell::new(Vec::new()));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    state.0.borrow_mut().fast_measure.budget = Duration::ZERO;
    let h100 = reflowing_height(151, 100.);
    state.scroll_to(list_offset(151, h100 / 2.));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    frames_requested(cx);

    draw_reflowing(&state, 160., &width, &rendered, &painted, cx);
    assert!(frames_requested(cx) > 0, "a pass requests frames");
    for _ in 0..3 {
        draw_reflowing(&state, 160., &width, &rendered, &painted, cx);
    }
    assert!(deferred(&state).is_some());

    // Second width change before the pass converged.
    painted.borrow_mut().clear();
    draw_reflowing(&state, 220., &width, &rendered, &painted, cx);
    let h220 = reflowing_height(151, 220.);
    let first = painted.borrow().first().copied().unwrap();
    assert_eq!(first.0, 151);
    assert!(
        (f32::from(first.1) + h220 / 2.).abs() <= 1.,
        "anchor fraction kept across two resizes: {first:?}, h220 {h220}"
    );
    let frames = settle(
        &state,
        220.,
        &width,
        &rendered,
        &painted,
        cx,
        400,
        |_, painted| {
            assert_eq!(painted.borrow().first().map(|p| p.0), Some(151));
        },
    );
    assert!(frames > 1);
    assert_eq!(unmeasured(&state), 0);
    assert!(deferred(&state).is_none());
    assert_eq!(total_height(&state), exact_height(0..300, 220.));
    assert_eq!(
        state.max_offset_for_scrollbar().y,
        px(exact_height(0..300, 220.) - 200.)
    );
}

#[gpui::test]
fn test_splice_during_deferred_measurement(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let (state, width, rendered) = reflowing_list(300, 50.);
    let painted = Rc::new(RefCell::new(Vec::new()));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    state.0.borrow_mut().fast_measure.budget = Duration::ZERO;
    state.scroll_to(list_offset(100, 0.));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    draw_reflowing(&state, 170., &width, &rendered, &painted, cx);
    for _ in 0..10 {
        draw_reflowing(&state, 170., &width, &rendered, &painted, cx);
    }
    let next = deferred(&state).unwrap();
    assert!(next > 2);

    // Replace an item before `next` (same index), append past it, and
    // drop the last few: indices of the others are unchanged.
    state.splice(1..2, 1);
    state.splice(300..300, 5);
    state.splice(295..305, 3);
    assert_eq!(state.item_count(), 298);
    settle(
        &state,
        170.,
        &width,
        &rendered,
        &painted,
        cx,
        400,
        |_, _| {},
    );
    // One frame more for a full measurement of what the pass skipped.
    draw_reflowing(&state, 170., &width, &rendered, &painted, cx);
    assert_eq!(unmeasured(&state), 0);
    assert_eq!(total_height(&state), exact_height(0..298, 170.));
    assert_eq!(state.logical_scroll_top(), list_offset(100, 0.));

    // Splicing the anchor out, and shrinking below `next`, mid-pass.
    draw_reflowing(&state, 120., &width, &rendered, &painted, cx);
    for _ in 0..50 {
        draw_reflowing(&state, 120., &width, &rendered, &painted, cx);
    }
    assert!(deferred(&state).unwrap() > 30);
    state.splice(20..298, 0);
    settle(
        &state,
        120.,
        &width,
        &rendered,
        &painted,
        cx,
        400,
        |_, _| {},
    );
    draw_reflowing(&state, 120., &width, &rendered, &painted, cx);
    assert_eq!(unmeasured(&state), 0);
    assert_eq!(total_height(&state), exact_height(0..20, 120.));
}

#[gpui::test]
fn test_reveal_into_estimated_heights_after_width_change(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let (state, width, rendered) = reflowing_list(300, 0.);
    let painted = Rc::new(RefCell::new(Vec::new()));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    state.0.borrow_mut().fast_measure.budget = Duration::ZERO;
    draw_reflowing(&state, 45., &width, &rendered, &painted, cx);
    assert!(deferred(&state).is_some());

    state.scroll_to_reveal_item(170);
    painted.borrow_mut().clear();
    draw_reflowing(&state, 45., &width, &rendered, &painted, cx);
    let shown = painted.borrow().iter().find(|p| p.0 == 170).copied();
    let (_, top, bottom) = shown.expect("item 170 painted after reveal");
    // At the bottom edge, as a reveal of an item below places it.
    assert_eq!(bottom, px(200.));
    assert!(
        top >= px(0.),
        "item 170 not fully visible: {top:?}..{bottom:?}"
    );

    // Measuring the items above it moves nothing.
    settle(&state, 45., &width, &rendered, &painted, cx, 400, |_, _| {});
    painted.borrow_mut().clear();
    draw_reflowing(&state, 45., &width, &rendered, &painted, cx);
    let after = painted.borrow().iter().find(|p| p.0 == 170).copied();
    assert_eq!(after, shown);
    assert_eq!(
        state.scroll_px_offset_for_scrollbar().y,
        -px(exact_height(0..171, 45.) - 200.)
    );
}

#[gpui::test]
fn test_width_change_on_bottom_aligned_list(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let state = ListState::new(200, crate::ListAlignment::Bottom, px(50.)).measure_all();
    let width = Rc::new(Cell::new(100.));
    let rendered = Rc::new(RefCell::new(Vec::new()));
    let painted = Rc::new(RefCell::new(Vec::new()));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    state.0.borrow_mut().fast_measure.budget = Duration::ZERO;
    painted.borrow_mut().clear();
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    assert_eq!(
        painted.borrow().last().map(|p| (p.0, p.2)),
        Some((199, px(200.)))
    );

    painted.borrow_mut().clear();
    draw_reflowing(&state, 60., &width, &rendered, &painted, cx);
    assert_eq!(
        painted.borrow().last().map(|p| (p.0, p.2)),
        Some((199, px(200.)))
    );
    settle(
        &state,
        60.,
        &width,
        &rendered,
        &painted,
        cx,
        400,
        |_, painted| {
            assert_eq!(
                painted.borrow().last().map(|p| (p.0, p.2)),
                Some((199, px(200.)))
            );
        },
    );
    assert_eq!(unmeasured(&state), 0);
    assert_eq!(total_height(&state), exact_height(0..200, 60.));
    assert_eq!(
        state.scroll_px_offset_for_scrollbar().y,
        -px(exact_height(0..200, 60.) - 200.)
    );

    // Bottom-aligned, scrolled up: the anchor item stays at the top.
    state.scroll_to(list_offset(120, 0.));
    draw_reflowing(&state, 60., &width, &rendered, &painted, cx);
    draw_reflowing(&state, 90., &width, &rendered, &painted, cx);
    settle(
        &state,
        90.,
        &width,
        &rendered,
        &painted,
        cx,
        400,
        |_, painted| {
            assert_eq!(
                painted.borrow().first().map(|p| (p.0, p.1)),
                Some((120, px(0.)))
            );
        },
    );
    assert_eq!(total_height(&state), exact_height(0..200, 90.));
}

#[gpui::test]
fn test_width_change_to_one_pixel_and_back(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let (state, width, rendered) = reflowing_list(100, 0.);
    let painted = Rc::new(RefCell::new(Vec::new()));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    draw_reflowing(&state, 1., &width, &rendered, &painted, cx);
    settle(&state, 1., &width, &rendered, &painted, cx, 400, |_, _| {});
    assert_eq!(total_height(&state), exact_height(0..100, 1.));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    settle(
        &state,
        100.,
        &width,
        &rendered,
        &painted,
        cx,
        400,
        |_, _| {},
    );
    assert_eq!(total_height(&state), exact_height(0..100, 100.));
}

#[gpui::test]
fn test_width_change_to_zero_with_fixed_rows(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let state = ListState::new(500, crate::ListAlignment::Top, px(0.)).measure_all();
    let draw = |w: f32, cx: &mut VisualTestContext| {
        cx.draw(point(px(0.), px(0.)), size(px(w), px(200.)), |_, cx| {
            cx.new(|_| TestView {
                state: state.clone(),
                render_item: Rc::new(|_| div().h(px(20.)).w_full().into_any()),
            })
            .into_any_element()
        });
    };
    draw(100., cx);
    draw(0., cx);
    let mut frames = 0;
    while frames_requested(cx) > 0 {
        draw(0., cx);
        frames += 1;
        assert!(frames < 1000);
    }
    assert_eq!(unmeasured(&state), 0);
    assert_eq!(total_height(&state), 500. * 20.);
    draw(100., cx);
    while frames_requested(cx) > 0 {
        draw(100., cx);
        frames += 1;
        assert!(frames < 2000);
    }
    assert_eq!(total_height(&state), 500. * 20.);
}

#[gpui::test]
fn test_deferred_measurement_ends_with_unready_items(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let (state, width, rendered) = reflowing_list(80, 0.);
    let painted = Rc::new(RefCell::new(Vec::new()));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    state.0.borrow_mut().fast_measure.budget = Duration::ZERO;
    state.set_item_measurement_readiness(|ix, _| ix != 40);
    draw_reflowing(&state, 150., &width, &rendered, &painted, cx);
    settle(
        &state,
        150.,
        &width,
        &rendered,
        &painted,
        cx,
        400,
        |_, _| {},
    );
    assert_eq!(unmeasured(&state), 1);
    assert!(deferred(&state).is_none());
    // Further frames do not request more.
    draw_reflowing(&state, 150., &width, &rendered, &painted, cx);
    assert_eq!(frames_requested(cx), 0);
}

#[gpui::test]
fn test_wheel_during_deferred_measurement(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let (state, width, rendered) = reflowing_list(300, 50.);
    let painted = Rc::new(RefCell::new(Vec::new()));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    state.0.borrow_mut().fast_measure.budget = Duration::ZERO;
    state.scroll_to(list_offset(150, 0.));
    draw_reflowing(&state, 100., &width, &rendered, &painted, cx);
    draw_reflowing(&state, 130., &width, &rendered, &painted, cx);
    for _ in 0..5 {
        cx.simulate_event(ScrollEvent {
            position: point(px(1.), px(1.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-37.))),
            ..Default::default()
        });
        draw_reflowing(&state, 130., &width, &rendered, &painted, cx);
    }
    let top = state.logical_scroll_top();
    assert!(top.item_ix >= 150);
    settle(
        &state,
        130.,
        &width,
        &rendered,
        &painted,
        cx,
        400,
        |_, _| {},
    );
    assert_eq!(state.logical_scroll_top(), top);
    assert_eq!(total_height(&state), exact_height(0..300, 130.));
}
