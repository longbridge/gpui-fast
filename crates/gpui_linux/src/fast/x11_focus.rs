//! Keeps the window active while keyboard focus moves into or out of one of
//! its child windows, such as a native WebView embedded in it.
//!
//! X11 reports such moves as `FocusOut`/`FocusIn` with detail
//! `NotifyInferior`; the keyboard never leaves the window, so treating them
//! as deactivation (as upstream does) blurs the window while the child has
//! focus.

use x11rb::protocol::xproto::NotifyDetail;

/// Whether a focus event only moves focus between the window and a child.
pub(crate) fn is_inferior(detail: NotifyDetail) -> bool {
    detail == NotifyDetail::INFERIOR
}
