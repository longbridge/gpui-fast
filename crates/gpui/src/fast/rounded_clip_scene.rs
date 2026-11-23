//! Transport rounded clip groups through the existing renderer layer graph.
//! No public primitive or ContentMask layout changes. Texture ids in
//! 0xF1000000.. identify clip groups, not atlas sprites or scroll tiles. Such a
//! group's LayerFrame has tile_size=0 and contains its drawable scene plus
//! a geometry LayerFrame (key u32::MAX). Horizontal radii are on the group
//! sprite and vertical radii occupy that frame's background RGBA, in
//! TL/TR/BR/BL order. This frame is metadata and is never composited.
//! Renderers evaluate the independent padding edges in colour fragments;
//! rounded groups do not allocate intermediate render targets.

use std::mem;

use crate::fast::rounded_clip::{ClipOperation, Shape};
use crate::{
    AtlasTextureId, AtlasTextureKind, AtlasTile, ContentMask, LayerContent, LayerFrame, LayerKey,
    PolychromeSprite, Rgba, ScaledPixels, Scene, TileId, bounds_tree::BoundsTree,
    scene::PaintOperation,
};

const CLIP_TEXTURE_BASE: u32 = 0xF100_0000;
const GEOMETRY_KEY: LayerKey = LayerKey(u32::MAX);

/// Materialize private paint scopes as existing, renderer-readable layer
/// objects. Preserve the original operation stream for retained paint ranges.
pub(crate) fn compile(scene: &mut Scene) {
    if scene.fast_rounded_clip.scopes.is_empty() || scene.fast_rounded_clip.compiled {
        return;
    }
    // The platform reads the original placements for layout, stacking, and
    // input. Only the renderer traverses holes inside clip groups.
    scene.native_views.sort_by_key(|view| view.order);
    scene.keep_first_painted_native_views();
    let operations = mem::take(&mut scene.paint_operations);
    let layers: Vec<LayerFrame> = scene
        .layers
        .frames
        .iter()
        .filter(|frame| frame.tile_size != 0)
        .cloned()
        .collect();
    // Each clip scene orders its primitives with the tree the clip opened
    // at the same place had last frame, so the tree is replayed.
    let mut trees = mem::take(&mut scene.fast_rounded_clip.trees);
    let new_scene = |tree| {
        let mut scene = Scene::default();
        scene.layers.frames = layers.clone();
        scene.primitive_bounds = tree;
        scene
    };
    let mut opened = 0;
    let mut slots = Vec::new();
    let mut stack = vec![new_scene(trees.take(None))];
    let mut shapes = Vec::new();
    let mut redundant = Vec::new();
    // As `Scene::replay` does: a split quad's pieces are inserted together,
    // and a flat top-level layer in one pass.
    let mut quads = crate::fast::scene::QuadRun::default();
    let mut next = 0;
    while let Some(operation) = operations.get(next) {
        next += 1;
        if let PaintOperation::Primitive(crate::Primitive::Quad(quad)) = operation {
            quads.push(stack.last_mut().unwrap(), *quad);
            continue;
        }
        quads.flush(stack.last_mut().unwrap());
        if let PaintOperation::StartLayer(_) = operation {
            let target = stack.last_mut().unwrap();
            if target.fast_top_level()
                && let Some(len) =
                    crate::fast::scene::replay_flat_layer(target, &operations[next - 1..])
            {
                next += len - 1;
                continue;
            }
        }
        match operation {
            PaintOperation::FastRoundedClip(ClipOperation::Begin(shape)) => {
                let already_active = shapes.contains(shape);
                redundant.push(already_active);
                shapes.push(*shape);
                if !already_active {
                    stack.push(new_scene(trees.take(Some(opened))));
                    slots.push(opened);
                    opened += 1;
                }
            }
            PaintOperation::FastRoundedClip(ClipOperation::End) => {
                let shape = shapes.pop().expect("balanced rounded clips");
                if redundant.pop().expect("rounded clip ownership") {
                    continue;
                }
                let mut child = stack.pop().expect("rounded clip scene");
                let slot = slots.pop().expect("rounded clip tree");
                trees.put(Some(slot), mem::take(&mut child.primitive_bounds));
                if child.is_empty() {
                    continue;
                }
                child.layers.frames.push(geometry(shape));
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
            PaintOperation::Primitive(crate::Primitive::NativeView(view)) => {
                // AppKit hosts one occurrence of each view. Keep its first paint
                // across sibling clip groups as well as within any one scene.
                if scene
                    .native_views
                    .iter()
                    .any(|first| first.id == view.id && first.hitbox_id == view.hitbox_id)
                {
                    stack.last_mut().unwrap().insert_primitive(*view);
                }
            }
            PaintOperation::Primitive(primitive) => stack
                .last_mut()
                .unwrap()
                .insert_primitive(primitive.clone()),
            PaintOperation::StartLayer(bounds) => stack.last_mut().unwrap().push_layer(*bounds),
            PaintOperation::EndLayer => stack.last_mut().unwrap().pop_layer(),
        }
    }
    quads.flush(stack.last_mut().unwrap());
    assert!(
        shapes.is_empty() && stack.len() == 1,
        "balanced rounded clips"
    );
    let mut compiled = stack.pop().unwrap();
    compiled.paint_operations = operations;
    compiled.fast_rounded_clip = mem::take(&mut scene.fast_rounded_clip);
    // The scene keeps the tree it was painted with, for the next paint.
    let root = mem::replace(
        &mut compiled.primitive_bounds,
        mem::take(&mut scene.primitive_bounds),
    );
    trees.put(None, root);
    compiled.fast_rounded_clip.trees = trees;
    if scene.native_views.len() != compiled.native_views.len() {
        let mut direct = mem::replace(
            &mut compiled.native_views,
            mem::take(&mut scene.native_views),
        );
        direct.sort_by_key(|view| view.order);
        let indices = direct
            .iter()
            .map(|view| {
                compiled
                    .native_views
                    .iter()
                    .position(|placement| {
                        placement.id == view.id && placement.hitbox_id == view.hitbox_id
                    })
                    .expect("every directly drawn native view has a placement")
            })
            .collect();
        compiled.fast_rounded_clip.native_batches =
            Some(Box::new(NativeBatches { direct, indices }));
    }
    compiled.fast_rounded_clip.compiled = true;
    compiled.sort_scratch = mem::take(&mut scene.sort_scratch);
    *scene = compiled;
}

/// The bounds trees of the scenes `compile` builds: the root scene's, and
/// each clip's by the order the clips are opened in.
#[derive(Default)]
pub(crate) struct ClipTrees {
    root: BoundsTree<ScaledPixels>,
    clips: Vec<BoundsTree<ScaledPixels>>,
}

impl ClipTrees {
    /// The tree of the root (`None`) or of the clip opened `slot`th, cleared
    /// so that it replays what it was given last time.
    fn take(&mut self, slot: Option<usize>) -> BoundsTree<ScaledPixels> {
        let mut tree = match slot {
            None => mem::take(&mut self.root),
            Some(slot) => self.clips.get_mut(slot).map(mem::take).unwrap_or_default(),
        };
        tree.clear();
        tree
    }

    fn put(&mut self, slot: Option<usize>, tree: BoundsTree<ScaledPixels>) {
        match slot {
            None => self.root = tree,
            Some(slot) => {
                if self.clips.len() <= slot {
                    self.clips.resize_with(slot + 1, Default::default);
                }
                self.clips[slot] = tree;
            }
        }
    }
}

/// A clip's child scene draws its own native view holes. The root retains
/// all placements in upstream draw order for platform layout and input, and
/// emits GPU batches for just the views directly painted into that scene.
pub(crate) struct NativeBatches {
    direct: Vec<crate::PaintNativeView>,
    indices: Vec<usize>,
}

pub(crate) fn native_batches(scene: &Scene) -> (&[crate::PaintNativeView], Option<&[usize]>) {
    match scene.fast_rounded_clip.native_batches.as_deref() {
        Some(batches) => (&batches.direct, Some(&batches.indices)),
        None => (&scene.native_views, None),
    }
}

fn geometry(shape: Shape) -> LayerFrame {
    let r = shape.radii;
    LayerFrame {
        key: GEOMETRY_KEY,
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
        content: Scene::default().into(),
        dirty_tiles: Vec::new(),
    }
}
