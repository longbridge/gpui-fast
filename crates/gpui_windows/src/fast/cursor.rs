//! Hiding the native pointer while a surface draws its own.
//!
//! The core hands [`CursorStyle::Hidden`] to `set_cursor_style` like any
//! other style; this module answers it by posting a null cursor to the
//! window thread. `current_cursor` doubles as the shape-to-restore slot:
//! hiding clears it, so the next real style sees the change and posts a
//! restore, and the window thread applies both through the same
//! `WM_GPUI_CURSOR_STYLE_CHANGED` path it already uses for shapes.

use gpui::CursorStyle;
use windows::Win32::Foundation::{LPARAM, WPARAM};

use crate::{events::WM_GPUI_CURSOR_STYLE_CHANGED, platform::WindowsPlatform};

/// Applies `style`, returning whether it was fully handled here so the
/// caller can skip its own path. Any style but `Hidden` is the caller's
/// to draw.
pub(crate) fn set_style(platform: &WindowsPlatform, style: CursorStyle) -> bool {
    if style != CursorStyle::Hidden {
        return false;
    }
    if platform.inner.state.current_cursor.get().is_some() {
        platform.inner.state.current_cursor.set(None);
        platform.post_message(
            WM_GPUI_CURSOR_STYLE_CHANGED,
            WPARAM(platform.inner.validation_number),
            LPARAM(0),
        );
    }
    true
}
