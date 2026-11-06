//! Where the browser opens its IME candidate panel.
//!
//! A browser anchors the panel of a composition (the candidate list of a CJK
//! input method, the accent picker) to the caret of the focused editable
//! element. For GPUI that element is the hidden `<textarea>` of
//! `ime_mirror`, which sits in the page's top left corner, so every panel
//! opened there rather than at the text being composed. GPUI reports the
//! caret through `PlatformWindow::update_ime_position`, which did nothing on
//! the web; this moves the element to the caret.

use wasm_bindgen::JsCast;

use gpui::{Bounds, Pixels};

use crate::window::WebWindowInner;

/// Moves the IME mirror to the caret at `bounds`, in window coordinates.
///
/// Only the element's style is written. A write to its value or selection
/// restarts the IME's input connection (see `ime_mirror`), a style write
/// does not, so this is safe in the middle of a composition.
pub(crate) fn move_to_caret(inner: &WebWindowInner, bounds: Bounds<Pixels>) {
    let Some(element) = inner
        .ime_mirror
        .event_target()
        .dyn_ref::<web_sys::HtmlElement>()
    else {
        return;
    };
    // Window coordinates are the canvas's, and the mirror is positioned
    // against the viewport: a page may place the canvas anywhere. The mirror's
    // corner goes under the caret's bottom left corner, so the panel, which
    // the browser opens below the mirror's caret, opens just under the
    // composed text, where a native text field opens it.
    let canvas = inner.canvas.get_bounding_client_rect();
    let left = canvas.left() + f64::from(f32::from(bounds.left()));
    let top = canvas.top() + f64::from(f32::from(bounds.bottom()));
    let style = element.style();
    style.set_property("left", &format!("{left}px")).ok();
    style.set_property("top", &format!("{top}px")).ok();
    // Over the canvas, the element must not take the clicks meant for the
    // text under it, and its caret, which the panel is placed against, must
    // not be inset from its corner by a textarea's default padding and border.
    style.set_property("pointer-events", "none").ok();
    style.set_property("padding", "0").ok();
    style.set_property("border", "0").ok();
}
