//! Notifications that a wheel event's other listeners send for its scroll.
//!
//! A scroll container's wheel listener moves its offset and notifies the
//! view that painted it, and [`super::invalidate`] counts those
//! notifications, to tell a view dirty only because a container it holds
//! scrolled from a view that changed: one notified more often than the wheel
//! scrolled what it holds was notified for something else. Other listeners
//! react to the same scroll, though. GPUI Kit's scrollbar listens to the
//! wheel too, and notifies the view again when the offset moved since it
//! last saw it, to show itself; with several wheel events a frame, as
//! trackpads and fast displays send them, its notifications outnumber the
//! scrolls, and the layer is never composited.
//!
//! So a notification of a view sent while a wheel event is dispatched, when
//! the event scrolled a container that view painted, counts as one more
//! notification for the scroll, as long as nothing else changed while the
//! event was dispatched: no entity was updated or written and no global
//! changed. A wheel listener that updates an entity and notifies changed
//! something, and the frame is not taken for a scroll.
//!
//! What such a notification can say that the state retained views track
//! does not is that state outside entities changed. The view is built again
//! for it, as for any notification, and what is drawn outside its scroll
//! containers' content — the scrollbar — is drawn afresh. What is assumed
//! is that state outside entities that a listener changes in reaction to a
//! scroll does not change what the view renders inside the content it
//! scrolled; state that does belongs in an entity the listener updates.
//!
//! Nor does a container's own listener notify for a wheel that cannot move
//! its content; see [`moves`].

use collections::FxHashMap;

use crate::fast::layers::COMPILED;
use crate::{
    App, Bounds, EntityId, Interactivity, IsZero, Pixels, PlatformInput, Point, Style, Window, px,
    size,
};

/// Where the counts stood when the wheel event being dispatched began to be.
#[derive(Default)]
pub(crate) struct WheelDispatch(Option<Before>);

struct Before {
    changes: u64,
    notifies: FxHashMap<EntityId, u64>,
    scroll_notifies: FxHashMap<EntityId, u64>,
}

/// Notes where the counts stand as `event` begins to be dispatched, if it is
/// a wheel event and the window has a layer whose view's notifications are
/// counted. Called before the event's listeners run.
pub(crate) fn begin_dispatch(window: &mut Window, cx: &App, event: &PlatformInput) {
    let before = (COMPILED
        && matches!(event, PlatformInput::Scroll(_))
        && !window.fast_layers.layers.is_empty())
    .then(|| Before {
        changes: crate::fast::dependencies::change_count(cx),
        notifies: crate::fast::layers::invalidate::watched_notifies(),
        scroll_notifies: window.fast_layers.scrolls.scroll_notifies.clone(),
    });
    window.fast_layers.wheel = WheelDispatch(before);
}

/// Counts, once the event [`begin_dispatch`] saw has been dispatched, the
/// notifications its listeners sent each view a container of which it
/// scrolled as notifications for the scroll, if nothing else changed.
pub(crate) fn end_dispatch(window: &mut Window, cx: &App) {
    let Some(before) = window.fast_layers.wheel.0.take() else {
        return;
    };
    if crate::fast::dependencies::change_count(cx) != before.changes {
        return;
    }
    let scroll_notifies = &mut window.fast_layers.scrolls.scroll_notifies;
    for (view, notifies) in crate::fast::layers::invalidate::watched_notifies() {
        let Some(notified_before) = before.notifies.get(&view) else {
            continue;
        };
        let scrolled_before = before.scroll_notifies.get(&view).copied().unwrap_or(0);
        let scrolls = scroll_notifies.get(&view).copied().unwrap_or(0);
        let scrolled = scrolls.saturating_sub(scrolled_before);
        let notified = notifies.saturating_sub(*notified_before);
        if scrolled > 0 && notified > scrolled {
            scroll_notifies.insert(view, scrolls + notified - scrolled);
        }
    }
}

/// How far the content of `interactivity`'s scroll container at `bounds`
/// scrolls on each axis: the bound `Interactivity::clamp_scroll_position`
/// clamps its offset to, computed the same way.
pub(crate) fn scroll_max(
    interactivity: &Interactivity,
    bounds: Bounds<Pixels>,
    style: &Style,
    window: &Window,
) -> Point<Pixels> {
    let padding = style
        .padding
        .to_pixels(bounds.size.into(), window.rem_size())
        .map(|edge| window.pixel_snap(*edge));
    let padding_size = size(padding.left + padding.right, padding.top + padding.bottom);
    let padded_content_size = interactivity.content_size + padding_size;
    Point::from(padded_content_size - bounds.size)
        .map(|pixels| (pixels * 100.).round() / 100.)
        .max(&Default::default())
}

/// Whether a wheel moving a scroll container's `offset` by `delta` moves its
/// content, which can scroll up to `max`. One that cannot, because the
/// content is already at the end the wheel points to, changes nothing.
///
/// The container's listener does not notify its view for such a wheel, or
/// invalidate the retained subtrees holding it, or note it scrolled: each
/// would draw again what did not move, and the last repaints every scroll
/// layer holding the container. A vertical wheel over a container that
/// scrolls only horizontally is mapped onto its one axis, so a page scrolled
/// past a carousel at its start sends one on every event.
pub(crate) fn moves(offset: Point<Pixels>, delta: Point<Pixels>, max: Point<Pixels>) -> bool {
    let moves = |offset: Pixels, delta: Pixels, max: Pixels| {
        !delta.is_zero() && (offset + delta).clamp(-max, px(0.)) != offset
    };
    moves(offset.x, delta.x, max.x) || moves(offset.y, delta.y, max.y)
}
