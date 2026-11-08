//! Window composition (zed-industries/zed#62379): native views between
//! GPUI's base content and its overlays, on Wayland and X11.

#[cfg(feature = "wayland")]
pub(crate) mod wayland;
#[cfg(feature = "x11")]
pub(crate) mod x11;

#[cfg(feature = "x11")]
mod occluders;

use gpui::{ComposedSceneLayer, Scene};

/// `present` supplies the current frame's scene after `Frame::finish`.
/// Only a unique layer covering its full stream permits a borrow. Callers
/// still process the other layers, whose range lists must be empty.
fn whole_scene_layer<'a>(
    scene: &'a Scene,
    layers: &[ComposedSceneLayer],
) -> Option<(usize, &'a Scene)> {
    let mut target = None;
    for (index, layer) in layers.iter().enumerate() {
        if !layer.ranges.is_empty() {
            let [range] = layer.ranges.as_slice() else {
                return None;
            };
            if target.is_some() || range.start != 0 || range.end != scene.len() {
                return None;
            }
            target = Some((index, scene));
        }
    }
    target
}

#[cfg(test)]
mod tests;
