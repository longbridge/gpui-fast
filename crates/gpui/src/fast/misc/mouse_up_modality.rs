//! A mouse button released, or a file dragged in, after a key press.
//!
//! The window tracks whether the last input came from the keyboard, and
//! while it did, [`crate::HitboxId::is_hovered`] is false, so that keyboard
//! navigation shows no hover highlight under a still pointer. Upstream sets
//! keyboard modality on a key press and mouse modality again on a move or a
//! press, but not on a release, nor on a file dragged in from the system.
//! The mouse up listeners `div` registers (`on_mouse_up`, `on_click`,
//! `on_drop`) ask whether their hitbox is hovered before they run. So after a
//! key press (other than a modifier) while a button is held, a release that
//! follows without a move is dropped: the click and the drop are lost, and a
//! drag the app ends in `on_mouse_up` without capturing the pointer never
//! ends. A file dragged in after a key press is not dropped either.
//!
//! A release is mouse input like a press, and so is a file drag, which the
//! window dispatches as mouse moves and a release: each sets mouse modality
//! before it is dispatched.

use crate::{FileDropEvent, InputModality, PlatformInput, Window};

/// Sets mouse modality for a mouse button release or a file drag, after
/// upstream has set the modality for the other events.
pub(crate) fn note_input(window: &mut Window, event: &PlatformInput) {
    if let PlatformInput::MouseUp(_)
    | PlatformInput::FileDrop(
        FileDropEvent::Entered { .. }
        | FileDropEvent::Pending { .. }
        | FileDropEvent::Submit { .. },
    ) = event
    {
        window.last_input_modality = InputModality::Mouse;
    }
}
