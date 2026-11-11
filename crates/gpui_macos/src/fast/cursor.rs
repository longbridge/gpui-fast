//! A transparent native pointer for [`CursorStyle::Hidden`].
//!
//! AppKit's global hide APIs are undone by mouse motion and span the whole
//! process, so a hidden style is answered with a 1x1 transparent cursor
//! instead: `reset_cursor_rects` registers it over the view that asked, and
//! the next real style replaces it again.

use objc2::{class, msg_send, runtime::AnyObject};
use objc2_foundation::{NSPoint, NSSize};
use std::{cell::Cell, ptr};

/// A 1x1 transparent cursor, built once per thread.
///
/// # Safety
///
/// Must be called on the AppKit main thread, like the cursor-rect update
/// that consumes the returned pointer.
pub(crate) unsafe fn hidden_cursor() -> *mut AnyObject {
    thread_local! {
        static HIDDEN: Cell<*mut AnyObject> = const { Cell::new(ptr::null_mut()) };
    }
    // SAFETY: The caller guarantees main-thread access; the messages below
    // are AppKit's documented NSImage/NSCursor initializers, and the image is
    // released right after the cursor retains it.
    unsafe {
        HIDDEN.with(|slot| {
            let mut cursor = slot.get();
            if cursor.is_null() {
                let image: *mut AnyObject = msg_send![class!(NSImage), alloc];
                let image: *mut AnyObject = msg_send![image, initWithSize: NSSize::new(1., 1.)];
                let allocated: *mut AnyObject = msg_send![class!(NSCursor), alloc];
                cursor = msg_send![
                    allocated,
                    initWithImage: image,
                    hotSpot: NSPoint::new(0., 0.)
                ];
                let _: () = msg_send![image, release];
                slot.set(cursor);
            }
            cursor
        })
    }
}
