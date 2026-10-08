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

use collections::FxHashMap;

use crate::fast::layers::COMPILED;
use crate::{App, EntityId, PlatformInput, Window};

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
        && matches!(event, PlatformInput::ScrollWheel(_))
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
