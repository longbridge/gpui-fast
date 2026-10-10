//! Hover held still while a wheel scrolls content under a still pointer.
//!
//! A wheel moves what is under the pointer without the pointer moving: every
//! frame of the scroll finds another row hovered, renders the row it leaves
//! and the row it enters again, and each may start an animation (a hover
//! card fading in, a toolbar showing) that keeps the list off its layer. A
//! frame of a long transcript scrolled by the wheel then costs about what it
//! costs without layers.
//!
//! So for [`HOVER_FREEZE`] after a wheel event, while the pointer stays where
//! the event was, nothing is hovered: the frame's hit test, and the one a
//! list foretells for its rows, are empty. When the wheel stops the window is
//! drawn again, and hover follows the pointer as before. A pointer that moves
//! ends the freeze at once.

use std::time::{Duration, Instant};

use crate::{App, HitTest, Pixels, Point, Window};

/// How long after the last wheel event hover stays held still.
pub(crate) const HOVER_FREEZE: Duration = Duration::from_millis(100);

/// When the window last got a wheel event, and where the pointer was.
pub(crate) struct HoverFreeze {
    last_wheel: Option<(Instant, Point<Pixels>)>,
    /// Off in the crate's own tests, which scroll under a still pointer to
    /// test what hover does as rows move; see [`set_enabled`].
    enabled: bool,
}

impl Default for HoverFreeze {
    fn default() -> Self {
        Self {
            last_wheel: None,
            enabled: !cfg!(test),
        }
    }
}

/// Notes a wheel event at the pointer's position, and draws the window again
/// once hover is no longer held still for it.
pub(crate) fn note_wheel(window: &mut Window, cx: &mut App) {
    let freeze = &mut window.fast_layers.hover_freeze;
    if !freeze.enabled {
        return;
    }
    freeze.last_wheel = Some((Instant::now(), window.mouse_position));
    window
        .spawn(cx, async move |cx| {
            cx.background_executor()
                .timer(HOVER_FREEZE + Duration::from_millis(10))
                .await;
            cx.update(|window, _| {
                if !frozen(window) {
                    window.refresh();
                }
            })
            .ok();
        })
        .detach();
}

/// Whether hover is held still: a wheel event came less than
/// [`HOVER_FREEZE`] ago, and the pointer has not moved since.
pub(crate) fn frozen(window: &Window) -> bool {
    window
        .fast_layers
        .hover_freeze
        .last_wheel
        .is_some_and(|(at, position)| {
            at.elapsed() < HOVER_FREEZE && position == window.mouse_position
        })
}

/// Empties the last frame's hit test as a frame begins to be drawn while
/// hover is held still, so that nothing rendered counts as hovered.
pub(crate) fn begin_draw(window: &mut Window) {
    if frozen(window) {
        window.mouse_hit_test = HitTest::default();
    }
}

/// The hit test of the frame being drawn, taken between its prepaint and its
/// paint: empty while hover is held still.
pub(crate) fn hit_test(window: &Window) -> HitTest {
    if frozen(window) {
        HitTest::default()
    } else {
        window.next_frame.hit_test(window.mouse_position)
    }
}

impl Window {
    /// Turns holding hover still while a wheel scrolls on or off for this
    /// window, for the crate's tests.
    #[cfg(test)]
    pub(crate) fn set_hover_freeze(&mut self, enabled: bool) {
        self.fast_layers.hover_freeze.enabled = enabled;
        self.fast_layers.hover_freeze.last_wheel = None;
    }
}
