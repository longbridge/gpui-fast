//! Transport rounded clip groups through the existing renderer layer graph.
//! No public primitive or ContentMask layout changes. Texture ids in
//! 0xF1000000.. identify clip groups, not atlas sprites or scroll tiles. Such a
//! group's LayerFrame has tile_size=0 and contains its drawable scene plus a
//! mask LayerFrame (key u32::MAX), whose scene paints the padding-edge shape.
//! Analytic renderers read horizontal radii from the group sprite and vertical
//! radii from that mask frame's background RGBA, in TL/TR/BR/BL order. Those
//! fields are geometry in this private protocol, never composited colour.
//! Renderers draw the group's content under that mask in the current target;
//! the content is not flattened into a colour texture or a shared mask.

use std::{mem, rc::Rc};

use crate::fast::rounded_clip::{ClipOperation, Shape};
use crate::{
    AtlasTextureId, AtlasTextureKind, AtlasTile, ContentMask, LayerContent, LayerFrame, LayerKey,
    PathBuilder, PolychromeSprite, Rgba, Scene, TileId, point, px, scene::PaintOperation,
};

const CLIP_TEXTURE_BASE: u32 = 0xF100_0000;
const MASK_KEY: LayerKey = LayerKey(u32::MAX);

/// Materialize private paint scopes as existing, renderer-readable layer
/// objects. Preserve the original operation stream for retained paint ranges.
pub(crate) fn compile(scene: &mut Scene) {
    if scene.fast_rounded_clip.scopes.is_empty() || scene.fast_rounded_clip.compiled {
        return;
    }
    let operations = mem::take(&mut scene.paint_operations);
    let layers: Vec<LayerFrame> = scene
        .layers
        .frames
        .iter()
        .filter(|frame| frame.tile_size != 0)
        .cloned()
        .collect();
    let new_scene = || {
        let mut scene = Scene::default();
        scene.layers.frames = layers.clone();
        scene
    };
    let mut stack = vec![new_scene()];
    let mut shapes = Vec::new();
    let mut redundant = Vec::new();
    for operation in &operations {
        match operation {
            PaintOperation::FastRoundedClip(ClipOperation::Begin(shape)) => {
                let already_active = shapes.contains(shape);
                redundant.push(already_active);
                shapes.push(*shape);
                if !already_active {
                    stack.push(new_scene());
                }
            }
            PaintOperation::FastRoundedClip(ClipOperation::End) => {
                let shape = shapes.pop().expect("balanced rounded clips");
                if redundant.pop().expect("rounded clip ownership") {
                    continue;
                }
                let mut child = stack.pop().expect("rounded clip scene");
                if child.is_empty() {
                    continue;
                }
                child.layers.frames.push(mask(shape));
                child.finish();
                let parent = stack.last_mut().expect("parent rounded clip scene");
                let key = LayerKey(0x0100_0000 + parent.layers.frames.len() as u32);
                parent.layers.frames.push(LayerFrame {
                    key,
                    generation: 0,
                    background: Rgba::default(),
                    tile_size: 0,
                    content: LayerContent::from(child),
                    dirty_tiles: Vec::new(),
                });
                parent.insert_primitive(PolychromeSprite {
                    order: 0,
                    pad: 0,
                    grayscale: false.into(),
                    opacity: 1.,
                    bounds: shape.bounds,
                    content_mask: ContentMask {
                        bounds: shape.bounds,
                    },
                    corner_radii: shape.radii.map(|radius| radius.width),
                    tile: AtlasTile {
                        texture_id: AtlasTextureId {
                            index: CLIP_TEXTURE_BASE + key.0 - 0x0100_0000,
                            kind: AtlasTextureKind::Polychrome,
                        },
                        tile_id: TileId(0),
                        padding: 0,
                        bounds: Default::default(),
                    },
                });
            }
            PaintOperation::Primitive(primitive) => stack
                .last_mut()
                .unwrap()
                .insert_primitive(primitive.clone()),
            PaintOperation::StartLayer(bounds) => stack.last_mut().unwrap().push_layer(*bounds),
            PaintOperation::EndLayer => stack.last_mut().unwrap().pop_layer(),
        }
    }
    assert!(
        shapes.is_empty() && stack.len() == 1,
        "balanced rounded clips"
    );
    let mut compiled = stack.pop().unwrap();
    compiled.paint_operations = operations;
    compiled.fast_rounded_clip = mem::take(&mut scene.fast_rounded_clip);
    compiled.fast_rounded_clip.compiled = true;
    compiled.sort_scratch = mem::take(&mut scene.sort_scratch);
    *scene = compiled;
}

fn mask(shape: Shape) -> LayerFrame {
    let b = shape.bounds;
    let x = b.origin.x.0;
    let y = b.origin.y.0;
    let right = b.right().0;
    let bottom = b.bottom().0;
    let r = shape.radii;
    let p = |x, y| point(px(x), px(y));
    let mut builder = PathBuilder::fill();
    builder.move_to(p(x + r.top_left.width.0, y));
    builder.line_to(p(right - r.top_right.width.0, y));
    arc(
        &mut builder,
        r.top_right,
        p(right, y + r.top_right.height.0),
    );
    builder.line_to(p(right, bottom - r.bottom_right.height.0));
    arc(
        &mut builder,
        r.bottom_right,
        p(right - r.bottom_right.width.0, bottom),
    );
    builder.line_to(p(x + r.bottom_left.width.0, bottom));
    arc(
        &mut builder,
        r.bottom_left,
        p(x, bottom - r.bottom_left.height.0),
    );
    builder.line_to(p(x, y + r.top_left.height.0));
    arc(&mut builder, r.top_left, p(x + r.top_left.width.0, y));
    builder.close();
    let mut path = builder.build().expect("finite rounded clip path").scale(1.);
    path.content_mask = ContentMask { bounds: b };
    path.color = crate::white().into();
    let mut scene = Scene::default();
    scene.insert_primitive(path);
    scene.finish();
    LayerFrame {
        key: MASK_KEY,
        generation: 0,
        // Private analytic geometry: horizontal radii are on the group sprite;
        // this non-composited frame carries the corresponding vertical radii.
        background: Rgba {
            r: r.top_left.height.0,
            g: r.top_right.height.0,
            b: r.bottom_right.height.0,
            a: r.bottom_left.height.0,
        },
        tile_size: 0,
        content: Rc::new(scene).into(),
        dirty_tiles: Vec::new(),
    }
}

fn arc(
    builder: &mut PathBuilder,
    radius: crate::Size<crate::ScaledPixels>,
    to: crate::Point<crate::Pixels>,
) {
    if radius.width.0 <= 0. || radius.height.0 <= 0. {
        builder.line_to(to);
    } else {
        builder.arc_to(
            point(px(radius.width.0), px(radius.height.0)),
            px(0.),
            false,
            true,
            to,
        );
    }
}
