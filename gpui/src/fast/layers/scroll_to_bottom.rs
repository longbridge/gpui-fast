// Modified by Longbridge for gpui-fast.
//! `ScrollHandle::scroll_to_bottom` asked of a handle already at its bottom,
//! as a view keeping a growing box at its bottom asks it on every render.

use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

use crate::{ScrollHandle, ScrollHandleState};

thread_local! {
    /// The scroll handles asked to scroll to the bottom where they were, with
    /// the version of their state then. See [`note_scroll_to_bottom`].
    #[allow(clippy::type_complexity)]
    static QUIET_SCROLLS_TO_BOTTOM: RefCell<Vec<(Weak<RefCell<ScrollHandleState>>, u64)>> =
        const { RefCell::new(Vec::new()) };
}

/// Marks the state of `handle` changed if scrolling it to its bottom may move
/// it: not when it is there already, nor when it was asked to already.
///
/// The request is kept all the same, for content that grew since to be laid
/// out at the bottom, which lays the container out again. A container laid
/// out as it was is not prepainted, and the request would wait for the next
/// frame that prepaints it, a wheel scrolling it away from the bottom, say:
/// it is dropped as the frame ends ([`drop_quiet_scrolls_to_bottom`]), as
/// prepainting the container would have done, leaving it where it is.
pub(crate) fn note_scroll_to_bottom(handle: &ScrollHandle, state: &ScrollHandleState) {
    if state.scroll_to_bottom {
        return;
    }
    let there = state
        .offset
        .try_borrow()
        .is_ok_and(|offset| offset.y <= -state.max_offset.y);
    if there {
        QUIET_SCROLLS_TO_BOTTOM.with_borrow_mut(|quiet| {
            quiet.push((Rc::downgrade(&handle.0), state.version.get()));
        });
    } else {
        state.version.bump();
    }
}

/// Drops the requests to scroll to the bottom made where a scroll handle
/// was already, that no prepaint took since: the handle has not changed
/// since, and is there still. See [`note_scroll_to_bottom`].
pub(crate) fn drop_quiet_scrolls_to_bottom() {
    if QUIET_SCROLLS_TO_BOTTOM.with_borrow(|quiet| quiet.is_empty()) {
        return;
    }
    let quiet = QUIET_SCROLLS_TO_BOTTOM.with_borrow_mut(std::mem::take);
    for (handle, version) in quiet {
        if let Some(handle) = handle.upgrade()
            && let Ok(mut state) = handle.try_borrow_mut()
            && state.version.get() == version
        {
            state.scroll_to_bottom = false;
        }
    }
}
