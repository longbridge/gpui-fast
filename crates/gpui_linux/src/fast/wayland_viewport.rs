//! Keeps a window's `wp_viewport` destination valid while its size is not yet
//! known.
//!
//! A layer-shell surface that is anchored to opposite edges may ask for a width
//! or height of 0 and leave it to the compositor (`zwlr_layer_surface_v1.set_size`:
//! "If you pass 0 for either value, the compositor will assign it and inform
//! you of the assignment in the configure event."). When the compositor sends
//! the output's fractional scale before that first configure, the window is
//! rescaled while that dimension is still 0, and the viewport destination was
//! set to it. viewporter forbids that (`wp_viewport.set_destination`: "If width
//! is -1 and height is -1, the destination size is unset instead. Any other
//! pair of values for width and height that contains zero or negative values
//! raises the bad_value protocol error."), so the compositor ended the
//! connection. Until both dimensions are positive the destination is unset
//! instead; the configure that brings the real size sets it.

use gpui::{Pixels, Size};
use wayland_protocols::wp::viewporter::client::wp_viewport;

/// Sets `viewport`'s destination to `size`, or unsets it while `size` has a
/// dimension that is not positive.
pub(crate) fn set_destination(viewport: &wp_viewport::WpViewport, size: Size<Pixels>) {
    let (width, height) = destination(size);
    viewport.set_destination(width, height);
}

/// The `wp_viewport.set_destination` arguments for a surface of `size`:
/// `(-1, -1)`, which unsets the destination, unless both are positive.
fn destination(size: Size<Pixels>) -> (i32, i32) {
    let width = f32::from(size.width) as i32;
    let height = f32::from(size.height) as i32;
    if width > 0 && height > 0 {
        (width, height)
    } else {
        (-1, -1)
    }
}

#[cfg(test)]
mod tests {
    use super::destination;
    use gpui::{px, size};

    #[test]
    fn the_destination_is_the_size_when_both_dimensions_are_positive() {
        assert_eq!(destination(size(px(1280.), px(32.))), (1280, 32));
        assert_eq!(destination(size(px(1.), px(1.))), (1, 1));
    }

    #[test]
    fn the_destination_is_unset_while_a_dimension_is_not_positive() {
        // A full-width layer-shell bar before its first configure.
        assert_eq!(destination(size(px(0.), px(32.))), (-1, -1));
        assert_eq!(destination(size(px(32.), px(0.))), (-1, -1));
        assert_eq!(destination(size(px(0.), px(0.))), (-1, -1));
        // Rounds down to 0.
        assert_eq!(destination(size(px(0.5), px(32.))), (-1, -1));
        assert_eq!(destination(size(px(-4.), px(32.))), (-1, -1));
    }
}
