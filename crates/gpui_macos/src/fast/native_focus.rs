//! Key events while a native subview owns keyboard focus.
//!
//! A native view embedded in a GPUI window (a `WKWebView`, say) is a subview of
//! `GPUIView`. While it is the window's first responder, AppKit still hands
//! keys to `GPUIView` twice:
//!
//! - `performKeyEquivalent:` goes down the view tree before the first responder
//!   sees the key, and `GPUIView` is first in that tree. AppKit sends it for
//!   Command / Control combinations and for keys carrying the function-key flag
//!   (arrows, Home / End, Page Up / Down, forward delete, F1…, Help).
//! - `keyDown:` reaches `GPUIView` up the responder chain when the native view
//!   leaves a key unhandled. WebKit does this for every key the page did not
//!   handle (`WebViewImpl::doneWithKeyEvent` re-sends it through
//!   `-[NSApplication sendEvent:]`), e.g. Left with the caret already at the
//!   start of a text field.
//!
//! `handle_key_event` passes such keys to `GPUIView`'s own `inputContext`. With
//! a composing input method active (Chinese Pinyin, Japanese, Korean…), the
//! input method commits the key's characters to the active text input client,
//! which is the focused native view: a text field in a web view gets a U+F702
//! for Left, drawn as a box.
//!
//! So while a subview owns keyboard focus, `GPUIView` leaves keys without
//! Command or Control alone: AppKit goes on to deliver them to that view, and a
//! key the view sends back up has been declined by it already. Command and
//! Control combinations still reach GPUI, so application key bindings and menu
//! shortcuts keep working.

use cocoa::{
    appkit::{NSEvent, NSEventModifierFlags},
    base::{BOOL, NO, id, nil},
};
use objc::{class, msg_send, runtime::Object, sel, sel_impl};

/// Whether `GPUIView` (`view`) should ignore `event` because a subview of it owns
/// keyboard focus. See the module documentation.
pub(crate) fn leaves_key_to_native_view(view: &Object, event: id) -> bool {
    let (native_focused, modifiers) = unsafe {
        let window: id = msg_send![view, window];
        let responder: id = if window == nil {
            nil
        } else {
            msg_send![window, firstResponder]
        };
        (is_native_subview(view, responder), event.modifierFlags())
    };
    leaves_key(
        native_focused,
        modifiers.contains(NSEventModifierFlags::NSCommandKeyMask),
        modifiers.contains(NSEventModifierFlags::NSControlKeyMask),
    )
}

/// `responder` is a view inside `view`, other than `view` itself.
unsafe fn is_native_subview(view: &Object, responder: id) -> bool {
    let view = view as *const Object as id;
    if responder == nil || responder == view {
        return false;
    }
    unsafe {
        let is_view: BOOL = msg_send![responder, isKindOfClass: class!(NSView)];
        if is_view == NO {
            return false;
        }
        let inside: BOOL = msg_send![responder, isDescendantOf: view];
        inside != NO
    }
}

fn leaves_key(native_focused: bool, command: bool, control: bool) -> bool {
    native_focused && !command && !control
}

#[cfg(test)]
mod tests {
    use super::leaves_key;

    #[test]
    fn a_focused_native_view_keeps_unmodified_keys_from_gpui() {
        // Arrows, Home / End, Escape, Backspace, with or without Shift / Option.
        assert!(leaves_key(true, false, false));
        // Command / Control combinations stay with GPUI: key bindings, menus.
        assert!(!leaves_key(true, true, false));
        assert!(!leaves_key(true, false, true));
        // GPUI itself focused: every key is GPUI's.
        assert!(!leaves_key(false, false, false));
    }
}
