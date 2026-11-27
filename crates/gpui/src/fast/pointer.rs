//! Whether the pointer is in the window, for hit testing.
//!
//! `MouseExited` carries the exit position, which lies on the window edge and
//! so is still inside hitboxes flush with that edge (such as maximized title
//! bar buttons on X11). Hit testing ignores the last position from the exit
//! until the next pointer event, so those hitboxes stop showing as hovered.

use crate::{DispatchPhase, FileDropEvent, Frame, HitTest, MouseExitEvent, PlatformInput, Window};

/// Records whether `event` moves the pointer out of the window or back in.
pub(crate) fn note_input(window: &mut Window, event: &PlatformInput) {
    match event {
        PlatformInput::MouseExited(_) => window.fast_pointer_outside = true,
        PlatformInput::KeyDown(_)
        | PlatformInput::KeyUp(_)
        | PlatformInput::ModifiersChanged(_)
        | PlatformInput::MousePressure(_)
        | PlatformInput::FileDrop(FileDropEvent::Exited | FileDropEvent::Ended) => {}
        _ => window.fast_pointer_outside = false,
    }
}

/// Hit tests `frame` at the pointer, or nothing while the pointer is outside.
pub(crate) fn hit_test(window: &Window, frame: &Frame) -> HitTest {
    if window.fast_pointer_outside {
        HitTest::default()
    } else {
        frame.hit_test(window.mouse_position)
    }
}

/// Records the hover state an element paints with, so the next mouse move compares against
/// what is on screen. An element can become hovered without a mouse move (an occluding overlay
/// closes under a still pointer); if the move that leaves it compared against a stale `false`,
/// nothing would be redrawn and the hover style would stay (gpui-kit#4072).
///
/// The pointer can also leave the window straight from a hovered element, with no mouse move
/// outside it first (macOS only sends `MouseExited`). Upstream only updates hover styles on
/// mouse moves, so `hover` and `group_hover` stayed on; clear them on the exit too
/// (gpui-kit#3552).
pub(crate) fn note_painted_hover(
    state: &Option<std::rc::Rc<std::cell::RefCell<crate::ElementHoverState>>>,
    group: bool,
    hitbox: &crate::HitboxId,
    window: &mut Window,
) {
    let Some(state) = state else {
        return;
    };
    let hovered = !window.last_input_was_touch() && hitbox.is_hovered(window);
    {
        let mut state = state.borrow_mut();
        if group {
            state.group = hovered;
        } else {
            state.element = hovered;
        }
    }

    let state = state.clone();
    let current_view = window.current_view();
    let subtrees = crate::fast::retained::enclosing_retained_subtrees(window);
    window.on_mouse_event(move |_: &MouseExitEvent, phase, window, cx| {
        if phase != DispatchPhase::Capture {
            return;
        }
        let was_hovered = {
            let mut state = state.borrow_mut();
            let hovered = if group {
                &mut state.group
            } else {
                &mut state.element
            };
            std::mem::replace(hovered, false)
        };
        if was_hovered {
            crate::fast::retained::invalidate_retained_subtrees(window, &subtrees);
            cx.notify(current_view);
        }
    });
}
