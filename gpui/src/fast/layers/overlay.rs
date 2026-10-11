// Modified by Longbridge for gpui-fast.
//! Content a layer's tiles cannot hold, drawn over them in the frame.
//!
//! Paths are never rasterized into tiles: the path shaders antialias with
//! `dpdx`/`dpdy` (`dfdx`/`dfdy`, `ddx`/`ddy`), differences within the 2×2
//! pixel quads the rasterizer shades together, and a tile composited at an
//! odd translation pairs other pixels than the window does, so an edge
//! pixel can come out one level apart (spec §5.6).
//!
//! Instead a layer's content is split in two. The *overlay* is every path,
//! and every primitive drawn after something in the overlay that it
//! overlaps; the tiles are rasterized from the rest. Each composited frame
//! draws the overlay over the tiles, in drawing order, at the layer's
//! translation: that is how the window draws it without a layer, path
//! rasterization included.
//!
//! The pixels come out as without a layer: on any pixel, the primitives
//! drawing over it are drawn in the same order. One left in the tiles was
//! drawn before every overlay primitive over the same pixel, or it would
//! overlap one drawn before it and be in the overlay. Tiles hold exactly the
//! pixels the window holds after the primitives they were rasterized from,
//! and the overlay is drawn over them by the frame's own pipelines.

use std::rc::Rc;

use crate::{
    Bounds, ScaledPixels, Scene,
    fast::layers::{background::draw_order, scene::visible_bounds},
    point,
    scene::{PaintOperation, Primitive},
};

/// Where `primitive` can change pixels: its visible bounds, and for a path
/// the pixels around them its antialiased edge reaches.
pub(crate) fn reach(primitive: &Primitive) -> Bounds<ScaledPixels> {
    let visible = visible_bounds(primitive);
    match primitive {
        Primitive::Path(_) => {
            let edge = ScaledPixels(1.);
            Bounds::from_corners(
                visible.origin - point(edge, edge),
                visible.bottom_right() + point(edge, edge),
            )
        }
        _ => visible,
    }
}

/// The rank of a primitive's kind among the batches of one draw order, as
/// `BatchIterator` draws them.
fn rank(primitive: &Primitive) -> u8 {
    match primitive {
        Primitive::Shadow(_) => 0,
        Primitive::Quad(_) => 1,
        Primitive::Path(_) => 2,
        Primitive::Underline(_) => 3,
        Primitive::MonochromeSprite(_) => 4,
        Primitive::SubpixelSprite(_) => 5,
        Primitive::PolychromeSprite(_) => 6,
        Primitive::Surface(_) => 7,
    }
}

/// `operations`, as a scene recorded them (each primitive holding the draw
/// order the scene gave it), split into the operations tiles are rasterized
/// from and the overlay, in drawing order. `None` when they paint no path,
/// and the tiles hold them all.
pub(crate) fn split(
    operations: &[PaintOperation],
) -> Option<(Vec<PaintOperation>, Vec<Primitive>)> {
    let painted_path = operations
        .iter()
        .any(|operation| matches!(operation, PaintOperation::Primitive(Primitive::Path(_))));
    if !painted_path {
        return None;
    }
    // Every primitive in the order the renderer draws them: by draw order,
    // then by kind, then as inserted.
    let mut sequence: Vec<((u32, u8, usize), &Primitive)> = operations
        .iter()
        .enumerate()
        .filter_map(|(ix, operation)| match operation {
            PaintOperation::Primitive(primitive) => {
                Some(((draw_order(primitive), rank(primitive), ix), primitive))
            }
            _ => None,
        })
        .collect();
    sequence.sort_unstable_by_key(|(key, _)| *key);
    let mut in_overlay = vec![false; operations.len()];
    let mut reaches: Vec<Bounds<ScaledPixels>> = Vec::new();
    let mut union: Option<Bounds<ScaledPixels>> = None;
    let mut overlay = Vec::new();
    for ((_, _, ix), primitive) in sequence {
        let reach = reach(primitive);
        if matches!(primitive, Primitive::Path(_))
            || union.is_some_and(|union| union.intersects(&reach))
                && reaches.iter().any(|earlier| earlier.intersects(&reach))
        {
            in_overlay[ix] = true;
            union = Some(union.map_or(reach, |union| union.union(&reach)));
            reaches.push(reach);
            overlay.push(primitive.clone());
        }
    }
    let tiles = operations
        .iter()
        .zip(&in_overlay)
        .filter(|(_, in_overlay)| !**in_overlay)
        .map(|(operation, _)| match operation {
            PaintOperation::Primitive(primitive) => PaintOperation::Primitive(primitive.clone()),
            PaintOperation::StartLayer(bounds) => PaintOperation::StartLayer(*bounds),
            PaintOperation::EndLayer => PaintOperation::EndLayer,
        })
        .collect();
    Some((tiles, overlay))
}

/// `content`, a layer's finished content scene, split into the finished
/// scene its tiles are rasterized from and its overlay (see [`split`]).
pub(crate) fn lift(content: Scene) -> (Scene, Rc<[Primitive]>) {
    let Some((tiles, overlay)) = split(&content.paint_operations) else {
        return (content, Rc::from([]));
    };
    let mut rest = Scene::default();
    for operation in tiles {
        match operation {
            PaintOperation::Primitive(primitive) => rest.insert_primitive(primitive),
            PaintOperation::StartLayer(bounds) => rest.push_layer(bounds),
            PaintOperation::EndLayer => rest.pop_layer(),
        }
    }
    rest.finish();
    (rest, overlay.into())
}
