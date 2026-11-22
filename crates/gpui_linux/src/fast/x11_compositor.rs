//! Whether an X11 compositing manager is running on the window's screen.
//!
//! Upstream's `check_compositor_present` also accepts `_NET_SUPPORTING_WM_CHECK`
//! on the root window as evidence of compositing. That property only says an
//! EWMH-capable window manager is running — KWin sets it whether its compositor
//! is active or suspended — so on a KDE session with compositing off, or under
//! any plain non-compositing EWMH window manager, upstream reports a compositor
//! that is not there. Client-side decorations then get granted, the client
//! inset reserves a 20-px "shadow" band around the window, and with nobody to
//! blend its alpha the band paints as an opaque black frame.
//!
//! The standard signal is ownership of the `_NET_WM_CM_S{screen}` selection:
//! a compositing manager owns it while it composites and releases it when it
//! stops. Upstream also formatted the atom with the root *window id* instead of
//! the screen number (`_NET_WM_CM_S0x3000057`), a selection that can never
//! exist; both issues are corrected here together because they serve the same
//! one-question predicate. `_NET_WM_CM_OWNER` on the root window stays as a
//! secondary hint some window managers write.

use gpui_util::ResultExt as _;
use log::Level;
use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::xproto::{self, AtomEnum, ConnectionExt as _},
    xcb_ffi::XCBConnection,
};

use crate::linux::get_reply;

/// True when a compositing manager will blend the window's alpha channel.
///
/// `root` is the root window id of the screen the window will be created on,
/// matching upstream's parameter; the screen number is recovered from the
/// connection setup.
pub(crate) fn compositor_present(xcb_connection: &XCBConnection, root: xproto::Window) -> bool {
    // XWayland is always composited: the Wayland compositor blends every X11
    // surface, but may not claim the _NET_WM_CM_S selection. Same test
    // fast::composition::x11 uses for the overlay window.
    if xcb_connection
        .extension_information("XWAYLAND")
        .log_with_level(Level::Debug)
        .flatten()
        .is_some()
    {
        return true;
    }

    let screen_num = xcb_connection
        .setup()
        .roots
        .iter()
        .position(|screen| screen.root == root)
        .unwrap_or(0);

    // Method 1: ownership of the _NET_WM_CM_S{screen} selection.
    let atom_name = format!("_NET_WM_CM_S{screen_num}");
    let atom1 = get_reply(
        || format!("Failed to intern {atom_name}"),
        xcb_connection.intern_atom(false, atom_name.as_bytes()),
    );
    let method1 = match atom1.log_with_level(Level::Debug) {
        Some(reply) if reply.atom != x11rb::NONE => {
            let atom = reply.atom;
            get_reply(
                || format!("Failed to get {atom_name} owner"),
                xcb_connection.get_selection_owner(atom),
            )
            .map(|reply| reply.owner != 0)
            .log_with_level(Level::Debug)
            .unwrap_or(false)
        }
        _ => false,
    };

    // Method 2: a _NET_WM_CM_OWNER property on the root window.
    let atom_name = "_NET_WM_CM_OWNER";
    let atom2 = get_reply(
        || format!("Failed to intern {atom_name}"),
        xcb_connection.intern_atom(false, atom_name.as_bytes()),
    );
    let method2 = match atom2.log_with_level(Level::Debug) {
        Some(reply) if reply.atom != x11rb::NONE => {
            let atom = reply.atom;
            get_reply(
                || format!("Failed to get {atom_name}"),
                xcb_connection.get_property(false, root, atom, AtomEnum::WINDOW, 0, 1),
            )
            .map(|reply| reply.value_len > 0)
            .unwrap_or(false)
        }
        _ => return false,
    };

    log::debug!(
        "Compositor detection: _NET_WM_CM_S?={}, _NET_WM_CM_OWNER={}",
        method1,
        method2
    );

    method1 || method2
}
