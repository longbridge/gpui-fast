//! Drawing again the view an element is in when the element's own
//! interaction state changes, rather than the whole window.
//!
//! GPUI's own elements keep part of what they draw in their element state:
//! whether a `div` is pressed (its `active` style), the press a click or a
//! drag starts from, the character an `InteractiveText` was pressed on.
//! Their listeners change that state and call `window.refresh()`, which
//! upstream uses to say "draw again" and gpui-fast takes to mean "draw
//! everything again from scratch": nothing is drawn from the last frame
//! while the window is refreshed, so every click on a clickable `div`
//! rebuilt the whole window, on the press and again on the release.
//!
//! That state is the element's own. It lives behind the element's id,
//! inside the view that painted it, and GPUI hands it to no one else: only
//! the element reads it, as it is laid out, prepainted and painted, and the
//! listeners it registers. Nothing outside that view can look different for
//! it, so drawing that view again is enough, as for a hover style: the view
//! is marked dirty as a notification marks it, without telling its
//! observers, as a refresh does not. What an application's listener changes
//! alongside is its own to notify, as anywhere else (see
//! `docs/retained-mode.md`).
//!
//! A refresh the application asks for (`window.refresh()`,
//! `cx.refresh_windows()`) still draws everything again: it is how an
//! application says that state outside entities changed, and nothing tells
//! which views read that state. The view of the listener asking is not
//! enough: an `Rc<Cell<bool>>` that a button in one view sets before
//! refreshing may be read by another view, to show a dialog.

use crate::{EntityId, Window};

/// The view an element was painted in, for its listeners to draw it again,
/// if it was painted in one.
#[derive(Clone, Copy)]
pub(crate) struct ElementView(Option<EntityId>);

/// The view the element being painted is painted in.
#[inline]
pub(crate) fn capture(window: &Window) -> ElementView {
    ElementView(window.rendered_entity_stack.last().copied())
}

/// Draws `view` again on the next frame, after the interaction state of an
/// element painted in it changed, in place of `window.refresh()`. Without
/// retention every view is drawn anyway, and the window is refreshed as
/// upstream refreshes it.
pub(crate) fn refresh(view: ElementView, window: &mut Window) {
    let Some(view) = view.0 else {
        window.refresh();
        return;
    };
    if !window.retained_state.view_retention || !window.invalidator.not_drawing() {
        window.refresh();
        return;
    }
    // Counted as a notification of the view by a layer whose container it
    // holds, which then does not take the frame for a scroll only.
    crate::fast::layers::invalidate::note_notify(view);
    let mut views = window.invalidator.take_views();
    views.insert(view);
    window.invalidator.replace_views(views);
    window.invalidator.set_dirty(true);
}
