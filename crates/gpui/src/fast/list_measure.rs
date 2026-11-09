//! Measuring a `measure_all` list over several frames after its width changes.
//!
//! Upstream's `list` forgets every item's height when the width inside its
//! padding and border changes, and a list that measures all its items then lays
//! out every one of them in that frame. A text view that lists its blocks with
//! `measure_all` and is resized over a long document re-wraps all of it in each
//! frame the width changes: tens to hundreds of milliseconds a frame.
//!
//! Here a list that measures all its items, and had measured them, keeps each
//! item's height as an estimate, and the frame showing the new width lays out
//! only the items it shows and its overdraw, as a list of visible items does.
//! The estimates of the others are scaled by how much the items measured in
//! that frame grew or shrank together ([`rescale_estimates`]), so the list's
//! height, and the scrollbar, follow the reflow. Later frames measure the rest
//! in order, [`DEFERRED_MEASURE_BUDGET`] of layout per frame and at least one
//! item, requesting a frame until none is left, then one more, so that the frame
//! after the last measurement shows the exact height to whatever read it
//! before. The item at the scroll top keeps the same fraction of itself above
//! the viewport, as `remeasure` keeps it.
//!
//! The scroll position is relative to the item at the top, so measuring the
//! items above it moves the scrollbar, not the content, and `scroll_to` lands
//! on its item at once. `scroll_to_reveal_item` of an item below the viewport
//! would place it from estimated heights; while a measurement is deferred the
//! reveal waits for the next prepaint, which lays out the item and those before
//! it, back from it, until they fill the viewport, and puts the item at the
//! bottom edge from those exact heights ([`resolve_pending_reveal`]).
//!
//! Unchanged: the first measurement, `reset`, `remeasure` and
//! `remeasure_items` still measure every item in one frame (a full measurement
//! due also ends a deferred one), and a list of visible items still forgets its
//! heights on a width change. Measurements that are not ready yet are retried
//! by a full measurement, as before.
//!
//! The list calls in here from hooks in `elements/list.rs`:
//! [`defers_width_change`] where a width change would forget every height,
//! [`begin_prepaint`] and [`end_prepaint`] around the layout of its items,
//! [`defer_reveal`] in `scroll_to_reveal_item`, [`forget_reveal`] where its
//! items or its scroll position change, and [`end_deferred`] when every item
//! has been measured.

use std::time::Duration;

use scheduler::Instant;
use sum_tree::{Bias, SumTree};

use crate::geometry::Along as _;
use crate::{
    App, AvailableSpace, Axis, Bounds, Count, Edges, ListItem, ListItemSummary,
    ListMeasuringBehavior, ListOffset, PendingScroll, Pixels, RenderItemFn, StateInner, Window, px,
    size,
};

/// How long a frame may spend measuring the items of a `measure_all` list that
/// a width change left to measure, after the frame that shows the new width.
pub(crate) const DEFERRED_MEASURE_BUDGET: Duration = Duration::from_millis(4);

/// A list's measurement deferred by a width change, held by the list's state.
pub(crate) struct DeferredMeasure {
    /// The index from which the items not yet measured at the new width are
    /// measured, a frame's budget at a time; `None` when no such pass is under
    /// way.
    pub(crate) next: Option<usize>,
    /// How long a frame may measure; tests set it to zero, for one item a
    /// frame.
    pub(crate) budget: Duration,
    /// An item `scroll_to_reveal_item` is to reveal while estimated heights
    /// lie before it: the next prepaint measures back from it and places it.
    pending_reveal: Option<usize>,
    /// The items as they were before the width change of this frame, whose
    /// heights the others' estimates are scaled from once this frame's items
    /// are measured.
    estimated_from: Option<SumTree<ListItem>>,
    /// Whether this frame measured deferred items, so that one more frame
    /// shows what they changed.
    measured: bool,
}

impl Default for DeferredMeasure {
    fn default() -> Self {
        Self {
            next: None,
            budget: DEFERRED_MEASURE_BUDGET,
            pending_reveal: None,
            estimated_from: None,
            measured: false,
        }
    }
}

/// Called where a width change would forget every item's height. A list that
/// measures all its items, all of which are measured or under way to be at
/// the width it last had, keeps their heights as estimates instead, and
/// returns true; any other list forgets them, as upstream's does, and ends a
/// deferred measurement.
pub(crate) fn defers_width_change(state: &mut StateInner) -> bool {
    if state.last_layout_bounds.is_none()
        || state.measuring_behavior != ListMeasuringBehavior::Measure(true)
    {
        state.fast_measure.next = None;
        return false;
    }
    let old_items = state.items.clone();

    // The item at the scroll top reflows too: keep the same fraction of it
    // above the viewport, as `remeasure` does.
    if let Some(anchor) = state.scroll_anchor.as_mut()
        && anchor.pending.is_none()
        && !anchor.provisional
        && !anchor.pending_end
    {
        let mut cursor = old_items.cursor::<Count>(());
        cursor.seek(&Count(anchor.position.item_ix), Bias::Right);
        if let Some(item_size) = cursor
            .item()
            .and_then(|item| item.size_hint())
            .filter(|item_size| item_size.height > px(0.))
        {
            anchor.pending = Some(PendingScroll::Proportional(
                (anchor.position.offset_in_item / item_size.height).clamp(0., 1.),
            ));
        }
    }

    state.items = SumTree::from_iter(
        old_items.iter().map(|item| ListItem::Unmeasured {
            size_hint: item.size_hint(),
            focus_handle: item.focus_handle(),
        }),
        (),
    );
    state.fast_measure.next = Some(0);
    state.fast_measure.estimated_from = Some(old_items);
    true
}

/// Called before the list lays out the items it shows: measures some of the
/// items a width change in an earlier frame left unmeasured, and places an
/// item a reveal left to place.
pub(crate) fn begin_prepaint(
    state: &mut StateInner,
    bounds: Bounds<Pixels>,
    padding: Edges<Pixels>,
    render_item: &mut RenderItemFn,
    window: &mut Window,
    cx: &mut App,
) {
    state.fast_measure.measured = state.fast_measure.estimated_from.is_none()
        && measure_deferred_items(
            state,
            bounds.size.width - padding.along(Axis::Horizontal),
            render_item,
            window,
            cx,
        );
    resolve_pending_reveal(state, bounds, &padding, render_item, window, cx);
}

/// Called once the list has laid out the items it shows: scales the estimates
/// a width change in this frame left, and requests a frame while there is
/// more to measure, and one more after the last measurement.
pub(crate) fn end_prepaint(state: &mut StateInner, window: &mut Window) {
    if let Some(old_items) = state.fast_measure.estimated_from.take() {
        rescale_estimates(state, &old_items);
    }
    if state.fast_measure.next.is_some() || state.fast_measure.measured {
        window.request_animation_frame();
    }
}

/// Called when the list has measured every item: nothing is left to a
/// deferred measurement.
pub(crate) fn end_deferred(state: &mut StateInner) {
    state.fast_measure.next = None;
}

/// Called where the list's items or its scroll position change: a reveal still
/// to be placed is superseded, or its item may have moved or gone.
pub(crate) fn forget_reveal(state: &mut StateInner) {
    state.fast_measure.pending_reveal = None;
}

/// Called by `scroll_to_reveal_item`: while a deferred measurement leaves
/// estimated heights before an item below the scroll top, the reveal is left
/// to the next prepaint, which measures back from the item, and true is
/// returned.
pub(crate) fn defer_reveal(state: &mut StateInner, ix: usize) -> bool {
    if state.fast_measure.next.is_none() || ix <= state.logical_scroll_top().item_ix {
        return false;
    }
    state.fast_measure.pending_reveal = Some(ix);
    true
}

/// Scales the estimates of the items still unmeasured after a width change by
/// how much the items measured at the new width this frame grew or shrank
/// together, so that the list's height, and with it the scrollbar, follows the
/// reflow until every item is measured.
///
/// The items shown are a sample of the content; wrapped text grows about as
/// the width shrinks, while fixed-height rows keep their height, which the
/// width alone cannot tell.
fn rescale_estimates(state: &mut StateInner, old_items: &SumTree<ListItem>) {
    if old_items.summary().count != state.items.summary().count {
        return;
    }
    let mut before = px(0.);
    let mut after = px(0.);
    for (old, new) in old_items.iter().zip(state.items.iter()) {
        if let (Some(old), Some(new)) = (old.size_hint(), new.size()) {
            before += old.height;
            after += new.height;
        }
    }
    if before <= px(0.) || after <= px(0.) {
        return;
    }
    let ratio = (after / before).clamp(0.25, 4.);
    if (ratio - 1.).abs() < 1e-3 {
        return;
    }
    let items = old_items
        .iter()
        .zip(state.items.iter())
        .map(|(old, new)| match new {
            // Still the estimate it was given: no layout this frame changed it.
            ListItem::Unmeasured {
                size_hint: Some(hint),
                focus_handle,
            } if old.size_hint() == Some(*hint) => ListItem::Unmeasured {
                size_hint: Some(size(hint.width, hint.height * ratio)),
                focus_handle: focus_handle.clone(),
            },
            _ => new.clone(),
        })
        .collect::<Vec<_>>();
    state.items = SumTree::from_iter(items, ());
}

/// Measures the items a width change left unmeasured, in order, until this
/// frame's budget is spent, and at least one, so that the list converges to
/// exact heights. Returns whether it measured any.
///
/// Each item measured keeps the list's scroll position, which is relative to
/// the item at its top; only the scrollbar moves as estimates above it become
/// exact.
fn measure_deferred_items(
    state: &mut StateInner,
    available_width: Pixels,
    render_item: &mut RenderItemFn,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let Some(next) = state.fast_measure.next else {
        return false;
    };
    if state.measuring_behavior != ListMeasuringBehavior::Measure(true) {
        // A full measurement is due, which measures these items too.
        return false;
    }

    let deadline = Instant::now() + state.fast_measure.budget;
    let available_item_space = size(
        AvailableSpace::Definite(available_width),
        AvailableSpace::MinContent,
    );
    let mut measured = Vec::new();
    let mut resume = None;
    let mut cursor = state
        .items
        .filter::<_, Count>((), |summary: &ListItemSummary| summary.unrendered_count > 0);
    cursor.next();
    while let Some(item) = cursor.item() {
        let ix = cursor.start().0;
        // Items before `next` were measured earlier and are not ready yet.
        if ix >= next {
            if !measured.is_empty() && Instant::now() >= deadline {
                resume = Some(ix);
                break;
            }
            let mut element = render_item(ix, window, cx);
            let item_size = crate::fast::layout_key::layout_as_list_item(
                &mut element,
                ix,
                available_item_space,
                window,
                cx,
            );
            measured.push((
                ix,
                item.with_measurement(item_size, state.is_item_ready(ix, cx)),
            ));
        }
        cursor.next();
    }
    drop(cursor);

    let any_measured = !measured.is_empty();
    replace_items(state, measured);

    state.fast_measure.next = resume;
    if resume.is_none() && state.items.summary().unrendered_count > 0 {
        // Measurements not ready yet are retried as a full measurement
        // retries them.
        state.measuring_behavior = ListMeasuringBehavior::Measure(false);
    }
    any_measured
}

/// Replaces the items at the given indices, which ascend, with the items given
/// for them.
fn replace_items(state: &mut StateInner, replacements: Vec<(usize, ListItem)>) {
    if replacements.is_empty() {
        return;
    }
    let mut old_items = state.items.cursor::<Count>(());
    let mut new_items = SumTree::default();
    for (ix, item) in replacements {
        new_items.append(old_items.slice(&Count(ix), Bias::Right), ());
        new_items.push(item, ());
        old_items.seek_forward(&Count(ix + 1), Bias::Right);
    }
    new_items.append(old_items.suffix(), ());
    drop(old_items);
    state.items = new_items;
}

/// Places the item `scroll_to_reveal_item` left to reveal at the bottom of the
/// viewport, as that method does, from heights measured now: the item and
/// those before it are laid out, back from it, until they fill the viewport,
/// rather than trusting the estimates a width change left.
fn resolve_pending_reveal(
    state: &mut StateInner,
    bounds: Bounds<Pixels>,
    padding: &Edges<Pixels>,
    render_item: &mut RenderItemFn,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(ix) = state.fast_measure.pending_reveal.take() else {
        return;
    };
    if ix >= state.items.summary().count {
        return;
    }
    let scroll_top = state.logical_scroll_top();
    if ix <= scroll_top.item_ix {
        state.set_scroll_anchor(Some(ListOffset {
            item_ix: ix,
            offset_in_item: px(0.),
        }));
        return;
    }

    let available_item_space = size(
        AvailableSpace::Definite(bounds.size.width - padding.along(Axis::Horizontal)),
        AvailableSpace::MinContent,
    );
    // How much of the items up to and including `ix` the viewport shows.
    let mut remaining = bounds.size.height - padding.top - padding.bottom;
    let mut measured = Vec::new();
    let mut start = ListOffset::default();
    let mut cursor = state.items.cursor::<Count>(());
    cursor.seek(&Count(ix), Bias::Right);
    while let Some(item) = cursor.item() {
        let item_ix = cursor.start().0;
        let height = match item.size() {
            Some(item_size) => item_size.height,
            None => {
                let mut element = render_item(item_ix, window, cx);
                let item_size = crate::fast::layout_key::layout_as_list_item(
                    &mut element,
                    item_ix,
                    available_item_space,
                    window,
                    cx,
                );
                measured.push((
                    item_ix,
                    item.with_measurement(item_size, state.is_item_ready(item_ix, cx)),
                ));
                item_size.height
            }
        };
        if height >= remaining {
            start = ListOffset {
                item_ix,
                offset_in_item: height - remaining.max(px(0.)),
            };
            break;
        }
        remaining -= height;
        if item_ix == 0 {
            // The viewport reaches above the first item.
            break;
        }
        cursor.prev();
    }
    drop(cursor);
    measured.reverse();
    replace_items(state, measured);

    if start.item_ix >= scroll_top.item_ix {
        state.set_scroll_anchor(Some(start));
    }
}
