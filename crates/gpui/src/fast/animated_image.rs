//! Animated images (GIF, WebP) wake their view when their next frame is due.
//!
//! Upstream's `img` asks for an animation frame on every paint while an
//! animated image shows, so a 10 fps GIF redraws the window at the display's
//! rate, and the scroll container holding it can never keep a layer
//! (`fast::layers::policy` rejects content that asked for an animation frame).
//! Instead, `img` hands its frame timing here, and a timer notifies the view
//! once the current frame's delay has passed.

use crate::{App, Task, Window};
use image::Delay;
use scheduler::Instant;
use std::{cell::Cell, rc::Rc, time::Duration};

/// The shortest wait between frames. GIFs commonly declare a delay of 0,
/// which would otherwise wake the view on every turn of the executor.
const MIN_FRAME_DELAY: Duration = Duration::from_millis(16);

/// The time `img` measures its frames against: the executor's clock, which
/// the timers below run on, so that a timer never fires before `img` sees its
/// frame as due. It is the wall clock, except in tests.
pub(crate) fn now(cx: &App) -> Instant {
    cx.background_executor().now()
}

/// An image's wake-up for its next frame: whether a timer is waiting, and the
/// timer, which notifies the view when it fires. Dropped with the image's
/// element state, which cancels the timer.
#[derive(Default)]
pub(crate) struct NextFrame {
    waiting: Rc<Cell<bool>>,
    _timer: Option<Task<()>>,
}

/// Called by `img` where upstream calls `window.request_animation_frame()`:
/// `state` is the image's wake-up and when its current frame started showing,
/// `delay` how long that frame shows.
pub(crate) fn request_next_frame(
    state: Option<(&mut NextFrame, Option<Instant>)>,
    delay: Delay,
    window: &mut Window,
    cx: &mut App,
) {
    let Some((next, shown_at)) = state else {
        return;
    };
    // Painting again while a timer waits (a scroll, another change to the
    // view) keeps that timer.
    if next.waiting.get() {
        return;
    }
    let now = now(cx);
    let due = shown_at.unwrap_or(now) + Duration::from(delay).max(MIN_FRAME_DELAY);
    // `img` doesn't move a frame's start on when its delay is 0, so such a
    // frame's due time stays in the past.
    let wait = if due > now {
        due - now
    } else {
        MIN_FRAME_DELAY
    };
    let view = window.current_view();
    let waiting = next.waiting.clone();
    waiting.set(true);
    next._timer = Some(window.spawn(cx, async move |cx| {
        cx.background_executor().timer(wait).await;
        waiting.set(false);
        cx.update(move |_, cx| cx.notify(view)).ok();
    }));
}
