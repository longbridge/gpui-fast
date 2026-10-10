//! Painting a scroll container's children into its layer: the scene swap,
//! the cull/clip split and inserting the tile quads (M3).
//!
//! A `div` that scrolls prepaints and paints its children through
//! [`begin_children`], [`prepaint_children`], [`end_children`] and
//! [`paint_children`]. They ask [`policy::decide`] what to do and do it:
//!
//! - `Bypass`: today's path.
//! - `Repaint`: the children are prepainted and painted as today, in window
//!   space at the current offset, but culled against the painted region (the
//!   viewport and its overscan) instead of the viewport, and painted into the
//!   layer's own scene, which is then stored in content space, diffed into
//!   tiles, and composited.
//! - `Composite`: the children are neither prepainted nor painted; the layer's
//!   tiles are composited at the new offset.

use crate::{
    App, AtlasTile, Bounds, ContentMask, Corners, DevicePixels, GlobalElementId, Overflow, Pixels,
    Point, PolychromeSprite, PrepaintStateIndex, Rgba, ScaledPixels, Scene, Size, Style, TileCoord,
    Window, WindowBackgroundAppearance,
    fast::{
        dependencies::{DependencyRecording, RenderDependencies},
        layers::{
            COMPILED, Layer, active, background,
            policy::{self, Decision},
            record::LayerRecord,
            scene::{
                LayerContent, LayerFrame, LayerKey, decode_layer_tile, layer_tile_id,
                layer_tile_texture_id, translate_primitive, visible_bounds,
            },
            tiles::{dirty_tiles, tile_hashes},
        },
    },
    point,
    scene::{PaintOperation, Primitive},
    size as size2,
};
use collections::FxHashMap;
use std::{mem, ops::Range, rc::Rc};

/// The side of a tile, in device pixels.
pub(crate) const TILE_SIZE: u32 = 512;

/// The layer whose content is being prepainted or painted.
pub(crate) struct Painting {
    /// The scroll container.
    pub(crate) id: GlobalElementId,
    /// The container's clip rect, in window space.
    pub(crate) viewport: Bounds<Pixels>,
    /// The part of the content painted, in window space: the viewport and
    /// the overscan around it, within the content.
    pub(crate) painted_region: Bounds<Pixels>,
    /// While the content paints, the frame's scene, swapped out for the
    /// layer's; otherwise the layer's scene, in window space.
    pub(crate) scene: Scene,
    /// The offset the content is painted at, snapped.
    pub(crate) scroll_offset: Point<Pixels>,
    /// The layer's translation, `scroll_offset` in whole device pixels.
    pub(crate) translation: Point<ScaledPixels>,
    /// What prepainting the content added to the frame.
    pub(crate) prepaint_range: Range<PrepaintStateIndex>,
    /// The recording of what prepainting the content reads, while it runs.
    pub(crate) recording: Option<DependencyRecording>,
    /// What prepainting the content read.
    pub(crate) dependencies: RenderDependencies,
    /// What routing input into the content takes from painting it.
    pub(crate) input: crate::fast::layers::input::PaintingInput,
    /// How the views prepainted inside the content were laid out.
    pub(crate) view_layouts: FxHashMap<GlobalElementId, crate::fast::layers::reuse::KeptLayout>,
    /// What prepainting the content added to the accessibility tree.
    pub(crate) a11y: Option<Rc<crate::fast::a11y::A11yStretch>>,
}

/// What a container's prepaint decided, for its paint to carry out.
pub(crate) enum Prepainted {
    /// The content was prepainted into the layer, and is painted into it.
    Repaint(Painting),
    /// The content was not prepainted; the layer's tiles are composited.
    #[allow(dead_code, reason = "the input stream reads the viewport")]
    Composite {
        viewport: Bounds<Pixels>,
        scroll_offset: Point<Pixels>,
    },
}

/// What a container decided for its children this frame, from
/// [`begin_children`] to [`end_children`].
#[derive(Clone, Copy)]
pub(crate) struct Children {
    decision: Decision,
    scroll_offset: Point<Pixels>,
}

/// Whether the content of a layer is being prepainted or painted: nested
/// retained views then record nothing and reuse nothing, the layer being
/// their retention (spec §6.3).
#[inline]
pub(crate) fn inside_layer(window: &Window) -> bool {
    window.fast_layers.painting.is_some()
}

/// Decides what the container `id`, prepainting its children at
/// `scroll_offset`, does with them, and sets up their prepaint for it.
/// `bounds` are the container's, `child_min` and `content_size` where its
/// children lie before scrolling.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
pub(crate) fn begin_children(
    window: &mut Window,
    cx: &mut App,
    id: Option<&GlobalElementId>,
    bounds: Bounds<Pixels>,
    child_min: Point<Pixels>,
    content_size: Size<Pixels>,
    scroll_offset: Point<Pixels>,
    style: &Style,
) -> Children {
    // Nearly every div: it does not scroll, and has no offset to snap.
    if !scrolls(style) && scroll_offset == Point::default() {
        return Children {
            decision: Decision::Bypass,
            scroll_offset,
        };
    }
    begin_scrolling_children(
        window,
        cx,
        id,
        bounds,
        child_min,
        content_size,
        scroll_offset,
        style,
    )
}

#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn begin_scrolling_children(
    window: &mut Window,
    cx: &mut App,
    id: Option<&GlobalElementId>,
    bounds: Bounds<Pixels>,
    child_min: Point<Pixels>,
    content_size: Size<Pixels>,
    scroll_offset: Point<Pixels>,
    style: &Style,
) -> Children {
    let scroll_offset = snap_scroll_offset(window, scroll_offset);
    let bypass = Children {
        decision: Decision::Bypass,
        scroll_offset,
    };
    if !COMPILED {
        return bypass;
    }
    let Some(id) = id else {
        return bypass;
    };
    if !scrolls(style) || inside_layer(window) || !active(window, cx) {
        return bypass;
    }
    let viewport = window.content_mask().bounds;
    let decision = policy::decide(window, cx, id, bounds, content_size, scroll_offset);
    let mut decision = crate::fast::layers::input::decide(window, id, decision);
    if decision == Decision::Composite
        && window.fast_layers.layers.get(id).is_none_or(|layer| {
            layer.record.as_ref().is_none_or(|record| {
                crate::fast::a11y::layer_needs_repaint(window, record.a11y.as_deref())
            })
        })
    {
        decision = Decision::Repaint;
    }
    match decision {
        Decision::Bypass => return bypass,
        Decision::Composite => {
            layer_mut(window, id).prepainted = Some(Prepainted::Composite {
                viewport,
                scroll_offset,
            });
            crate::fast::layers::reuse::carry_prepaint(window, id, viewport, scroll_offset);
        }
        Decision::Repaint => {
            let content_origin = if child_min.x == Pixels::MAX {
                bounds.origin
            } else {
                child_min
            };
            let content = Bounds {
                origin: content_origin + scroll_offset,
                size: content_size,
            };
            let painted_region = painted_region(viewport, content, style.overflow);
            let start = window.prepaint_index();
            window.fast_layers.painting = Some(Painting {
                id: id.clone(),
                viewport,
                painted_region,
                scene: Scene::default(),
                scroll_offset,
                translation: translation(window, scroll_offset),
                prepaint_range: start.clone()..start,
                recording: Some(cx.begin_recording_dependencies()),
                dependencies: RenderDependencies::default(),
                input: Default::default(),
                view_layouts: FxHashMap::default(),
                a11y: None,
            });
            // Culling works in the painted region, not in the viewport and
            // whatever clips it; the composite clips to those.
            window.content_mask_stack.push(ContentMask {
                bounds: painted_region,
            });
        }
    }
    Children {
        decision,
        scroll_offset,
    }
}

/// Prepaints a container's children as [`begin_children`] decided: `f` is
/// the children's prepaint, run at the container's scroll offset unless
/// the layer is composited.
#[inline(always)]
pub(crate) fn prepaint_children(
    window: &mut Window,
    children: Children,
    f: impl FnOnce(&mut Window),
) {
    if children.decision != Decision::Composite {
        window.with_element_offset(children.scroll_offset, f);
    }
}

/// Ends what [`begin_children`] began, once the children are prepainted.
#[inline(always)]
pub(crate) fn end_children(window: &mut Window, cx: &mut App, children: Children) {
    if children.decision != Decision::Repaint {
        return;
    }
    end_repainted_children(window, cx);
}

#[inline(never)]
fn end_repainted_children(window: &mut Window, cx: &mut App) {
    window.content_mask_stack.pop();
    let Some(mut painting) = window.fast_layers.painting.take() else {
        debug_assert!(false, "a layer's prepaint ended without beginning");
        return;
    };
    painting.prepaint_range.end = window.prepaint_index();
    painting.a11y = crate::fast::a11y::capture(window, &painting.prepaint_range);
    if let Some(recording) = painting.recording.take() {
        painting.dependencies = cx.finish_recording_dependencies(recording).all;
    }
    let id = painting.id.clone();
    layer_mut(window, &id).prepainted = Some(Prepainted::Repaint(painting));
}

/// Paints a container's children as its prepaint decided: `f` is the
/// children's paint.
#[inline(always)]
pub(crate) fn paint_children(
    window: &mut Window,
    cx: &mut App,
    id: Option<&GlobalElementId>,
    f: impl FnOnce(&mut Window, &mut App),
) {
    if window.fast_layers.layers.is_empty() {
        return f(window, cx);
    }
    paint_children_of_layers(window, cx, id, f)
}

#[inline(never)]
fn paint_children_of_layers(
    window: &mut Window,
    cx: &mut App,
    id: Option<&GlobalElementId>,
    f: impl FnOnce(&mut Window, &mut App),
) {
    let prepainted = match id {
        Some(id) if !window.fast_layers.layers.is_empty() => window
            .fast_layers
            .layers
            .get_mut(id)
            .and_then(|layer| layer.prepainted.take()),
        _ => None,
    };
    match prepainted {
        None => f(window, cx),
        Some(Prepainted::Repaint(painting)) => match bake_background(window) {
            Some(background) => repaint(window, cx, painting, background, f),
            None => paint_unbaked(window, cx, painting, f),
        },
        Some(Prepainted::Composite { scroll_offset, .. }) => {
            if let Some(id) = id {
                crate::fast::layers::reuse::carry_paint(window, id);
                composite(window, id, scroll_offset);
            }
        }
    }
}

/// The opaque colour a layer's tiles are cleared with: what the frame has
/// painted so far under the viewport, the current content mask, if it is
/// one solid quad covering it (spec §5.2).
/// Whether the layer of `id` replays its content into the frame rather than
/// compositing tiles of it: a list's does. Its rows' paint operations are
/// drawn again, moved, over the viewport, costing the frame what drawing
/// the rows it shows costs, without rendering, laying out or prepainting
/// them again, and without tiles to rasterize or to keep within budget.
pub(crate) fn replays(window: &Window, id: &GlobalElementId) -> bool {
    window
        .fast_layers
        .layers
        .get(id)
        .is_some_and(|layer| layer.rows.list)
}

pub(crate) fn bake_background(window: &Window) -> Option<Rgba> {
    let window_opaque =
        window.platform_window.background_appearance() == WindowBackgroundAppearance::Opaque;
    let viewport = window.snapped_content_mask().bounds;
    background::bake(&window.next_frame.scene, viewport, window_opaque)
}

/// Paints content prepainted for a layer straight into the frame, as
/// without one, because the layer's tiles could not be cleared with what is
/// under them. The layer keeps no content, so that it is painted afresh.
fn paint_unbaked(
    window: &mut Window,
    cx: &mut App,
    painting: Painting,
    f: impl FnOnce(&mut Window, &mut App),
) {
    let id = painting.id.clone();
    // Nested views were prepainted inside the layer, and are painted so.
    window.fast_layers.painting = Some(painting);
    f(window, cx);
    window.fast_layers.painting = None;
    crate::fast::layers::policy::defer_unbaked(window, &id);
}

/// Carries out a `Composite` decision for the container `id`, scrolled to
/// `scroll_offset`: the layer's content stands, cleared with the background
/// now under it, and is composited at the offset.
fn composite(window: &mut Window, id: &GlobalElementId, scroll_offset: Point<Pixels>) {
    let translation = translation(window, scroll_offset);
    composite_at(window, id, translation);
}

/// Composites the layer of the container `id` with its content moved by
/// `translation` into window space, its content standing, cleared with the
/// background now under it.
pub(crate) fn composite_at(
    window: &mut Window,
    id: &GlobalElementId,
    translation: Point<ScaledPixels>,
) {
    if replays(window, id) {
        insert_layer(window, id, translation, 0);
        return;
    }
    let background = bake_background(window);
    let layer = layer_mut(window, id);
    let Some(current) = layer.record.as_ref().map(|record| record.background) else {
        return;
    };
    if background.is_some_and(|background| background != current) {
        layer.next_generation();
    }
    let generation = layer.generation;
    let record = layer.record.as_mut().expect("checked above");
    let mut dirtied = 0;
    match background {
        Some(background) if background == record.background => {}
        Some(background) => {
            // Every tile is cleared with another colour: a new generation of
            // the same content, all of it dirty.
            record.background = background;
            record.generation = generation;
            record.dirty_tiles = all_tiles(&record.tile_hashes);
            dirtied = record.dirty_tiles.len();
        }
        None => {
            // The tiles cannot be cleared with what is under them now: the
            // content is drawn into the frame, and painted afresh next time.
            let record = layer.record.take().expect("checked above");
            crate::fast::layers::policy::defer_unbaked(window, id);
            draw_into_frame(window, record.content.operations(), translation);
            let viewports = viewports(window);
            insert_overlay(
                &mut window.next_frame.scene,
                &record.overlay,
                translation,
                viewports,
            );
            return;
        }
    }
    insert_layer(window, id, translation, dirtied);
}

/// Composites the layer of the container `id` into the frame at
/// `translation`, `dirtied` of its tiles new this frame: its tile quads
/// over the viewport, the current content mask, and its [`LayerFrame`].
/// Content whose overlay cannot be drawn over its tiles
/// ([`LayerRecord::has_paths`]) is drawn into the frame instead.
pub(crate) fn insert_layer(
    window: &mut Window,
    id: &GlobalElementId,
    translation: Point<ScaledPixels>,
    dirtied: usize,
) {
    let viewport = window.snapped_content_mask().bounds;
    let Some(layer) = window.fast_layers.layers.get(id) else {
        return;
    };
    let Some(record) = layer.record.as_ref() else {
        return;
    };
    if layer.rows.list {
        let content = record.content.clone();
        let viewport = viewports(window).snapped;
        let in_content = Bounds {
            origin: viewport.origin - translation,
            size: viewport.size,
        };
        draw_into_frame(window, content.operations_over(in_content), translation);
        let stats = &mut window.layout_engine.as_mut().unwrap().retention.stats;
        stats.layer_frames_composited += 1;
        policy::note_work(window, id, 0.);
        return;
    }
    if record.has_paths {
        let content = record.content.clone();
        draw_into_frame(window, content.operations(), translation);
        return;
    }
    let viewports = viewports(window);
    insert_tile_quads(&mut window.next_frame.scene, layer, viewport, translation);
    insert_overlay(
        &mut window.next_frame.scene,
        &record.overlay,
        translation,
        viewports,
    );
    let stats = &mut window.layout_engine.as_mut().unwrap().retention.stats;
    stats.layer_frames_composited += 1;
    stats.tiles_dirtied += dirtied as u64;
    policy::note_work(window, id, 0.);
}

/// Inserts into `scene` the quads of the tiles of `layer` that show in
/// `viewport` with its content moved by `translation`, all at one draw
/// order, and the layer's [`LayerFrame`] they are rasterized from (spec
/// §5.1).
pub(crate) fn insert_tile_quads(
    scene: &mut Scene,
    layer: &Layer,
    viewport: Bounds<ScaledPixels>,
    translation: Point<ScaledPixels>,
) {
    let Some(record) = layer.record.as_ref() else {
        return;
    };
    if viewport.is_empty() {
        return;
    }
    let frame = LayerFrame {
        key: layer.key,
        generation: record.generation,
        background: record.background,
        tile_size: TILE_SIZE,
        content: record.content.clone(),
        dirty_tiles: record.dirty_tiles.clone(),
    };
    let size = TILE_SIZE as f32;
    let content_viewport = Bounds {
        origin: viewport.origin - translation,
        size: viewport.size,
    };
    let min = content_viewport.origin;
    let max = content_viewport.bottom_right();
    let (x0, y0) = (
        (min.x.0 / size).floor() as i32,
        (min.y.0 / size).floor() as i32,
    );
    let (x1, y1) = (
        (max.x.0 / size).ceil() as i32,
        (max.y.0 / size).ceil() as i32,
    );
    scene.push_layer(viewport);
    let texture_id = layer_tile_texture_id(layer.key);
    for y in y0..y1 {
        for x in x0..x1 {
            let coord = TileCoord { x, y };
            let tile = frame.tile_bounds(coord);
            scene.insert_primitive(PolychromeSprite {
                order: 0,
                pad: 0,
                grayscale: false.into(),
                opacity: 1.,
                bounds: Bounds {
                    origin: tile.origin + translation,
                    size: tile.size,
                },
                content_mask: ContentMask { bounds: viewport },
                corner_radii: Corners::default(),
                tile: AtlasTile {
                    texture_id,
                    tile_id: layer_tile_id(coord),
                    padding: 0,
                    bounds: Bounds {
                        origin: Point::default(),
                        size: size2(
                            DevicePixels(TILE_SIZE as i32),
                            DevicePixels(TILE_SIZE as i32),
                        ),
                    },
                },
            });
        }
    }
    scene.pop_layer();
    scene.layers.frames.push(frame);
}

/// Inserts into `scene` the overlay a layer keeps apart from its tiles,
/// moved by `translation` and clipped to `viewport`, after its tile quads,
/// so that it is drawn over them (see `fast::layers::overlay`).
fn insert_overlay(
    scene: &mut Scene,
    overlay: &[Primitive],
    translation: Point<ScaledPixels>,
    viewports: Viewports,
) {
    for primitive in overlay {
        let mut primitive = translate_primitive(primitive, translation);
        clip_primitive(&mut primitive, viewports);
        scene.insert_primitive(primitive);
    }
}

/// The viewport content drawn into the frame is clipped to: the current
/// content mask, snapped to device pixels as the frame snaps every
/// primitive's but a path's, and on [`snap_path`]'s grid, as
/// `Window::paint_path` takes it, for paths.
#[derive(Clone, Copy)]
struct Viewports {
    snapped: Bounds<ScaledPixels>,
    exact: Bounds<ScaledPixels>,
}

fn viewports(window: &Window) -> Viewports {
    Viewports {
        snapped: window.snapped_content_mask().bounds,
        exact: snap_bounds(window.content_mask().bounds.scale(window.scale_factor())),
    }
}

/// How finely [`snap_path`] places path vertices: 256 steps a device pixel,
/// as fine as a GPU rasterizer resolves a vertex's position.
const PATH_GRID: f32 = 256.;

/// `path`, in device pixels, with its vertices and bounds moved to the
/// nearest 1/256 of a device pixel where layers are compiled.
///
/// A path painted into a layer is drawn moved by whole device pixels, while
/// drawn without a layer it is painted where it is now: at a fractional
/// scale factor the two differ by rounding error in the last bits of each
/// position (a snapped scroll offset is not a whole number of logical
/// pixels). On the grid they land on the same positions.
pub(crate) fn snap_path(mut path: crate::Path<ScaledPixels>) -> crate::Path<ScaledPixels> {
    if !COMPILED {
        return path;
    }
    for vertex in &mut path.vertices {
        vertex.xy_position = snap_point(vertex.xy_position);
    }
    path.bounds = snap_bounds(path.bounds);
    // Its mask too: a layer's overlay clips the path to the viewport where
    // it is drawn, which must come out as the mask painted there.
    path.content_mask.bounds = snap_bounds(path.content_mask.bounds);
    path
}

/// `value` on the grid of [`PATH_GRID`] steps a device pixel.
fn snap(value: ScaledPixels) -> ScaledPixels {
    // Adding and taking away 1.5 × 2^23 rounds an `f32` below 2^22 in
    // magnitude to a whole number, ties to even, which a shift by whole
    // pixels (a multiple of 256 steps) keeps; `floor` would call into libm.
    const ROUNDER: f32 = 12_582_912.;
    let steps = value.0 * PATH_GRID;
    if steps.abs() < 4_194_304. {
        ScaledPixels(((steps + ROUNDER) - ROUNDER) / PATH_GRID)
    } else {
        value
    }
}

fn snap_point(point: Point<ScaledPixels>) -> Point<ScaledPixels> {
    Point {
        x: snap(point.x),
        y: snap(point.y),
    }
}

/// `bounds` with its corners on the grid of [`PATH_GRID`] steps a device
/// pixel, as [`snap_path`] puts a path's.
fn snap_bounds(bounds: Bounds<ScaledPixels>) -> Bounds<ScaledPixels> {
    Bounds::from_corners(snap_point(bounds.origin), snap_point(bounds.bottom_right()))
}

/// Carries into `scene` the [`LayerFrame`]s of the tile quads that
/// `scene.replay` copies from the operations `range` of `previous`, so
/// that a retained subtree holding a layer composites it when it is reused.
pub(crate) fn replay_layers(scene: &mut Scene, range: Range<usize>, previous: &Scene) {
    if previous.layers.frames.is_empty() {
        return;
    }
    for operation in &previous.paint_operations[range] {
        let PaintOperation::Primitive(Primitive::PolychromeSprite(sprite)) = operation else {
            continue;
        };
        let Some((key, _)) = decode_layer_tile(sprite.tile.texture_id, sprite.tile.tile_id) else {
            continue;
        };
        if scene.layers.frames.iter().any(|frame| frame.key == key) {
            continue;
        }
        if let Some(frame) = previous.layers.frames.iter().find(|frame| frame.key == key) {
            scene.layers.frames.push(frame.clone());
        }
    }
}

/// Draws the primitives of `content`, moved by `delta` into window space,
/// straight into the frame, clipped to the viewport, the current content
/// mask: what painting the content into the frame would have drawn.
pub(crate) fn draw_into_frame<'a>(
    window: &mut Window,
    operations: impl IntoIterator<Item = &'a PaintOperation>,
    delta: Point<ScaledPixels>,
) {
    let viewports = viewports(window);
    let viewport = viewports.snapped;
    let scene = &mut window.next_frame.scene;
    for operation in operations {
        match operation {
            PaintOperation::Primitive(primitive) => {
                let mut primitive = translate_primitive(primitive, delta);
                clip_primitive(&mut primitive, viewports);
                scene.insert_primitive(primitive);
            }
            PaintOperation::StartLayer(bounds) => {
                let bounds = Bounds {
                    origin: bounds.origin + delta,
                    size: bounds.size,
                };
                scene.push_layer(bounds.intersect(&viewport));
            }
            PaintOperation::EndLayer => scene.pop_layer(),
        }
    }
}

/// Narrows every content mask `primitive` carries to `viewports`.
fn clip_primitive(primitive: &mut Primitive, viewports: Viewports) {
    let mask = match primitive {
        Primitive::Path(_) => &viewports.exact,
        _ => &viewports.snapped,
    };
    let clip = |content_mask: &mut ContentMask<ScaledPixels>| {
        content_mask.bounds = content_mask.bounds.intersect(mask);
    };
    match primitive {
        Primitive::Shadow(p) => clip(&mut p.content_mask),
        Primitive::Quad(p) => clip(&mut p.content_mask),
        Primitive::Path(p) => {
            clip(&mut p.content_mask);
            for vertex in &mut p.vertices {
                if !vertex.content_mask.bounds.is_empty() {
                    clip(&mut vertex.content_mask);
                }
            }
        }
        Primitive::Underline(p) => clip(&mut p.content_mask),
        Primitive::MonochromeSprite(p) => clip(&mut p.content_mask),
        Primitive::SubpixelSprite(p) => clip(&mut p.content_mask),
        Primitive::PolychromeSprite(p) => clip(&mut p.content_mask),
        Primitive::Surface(p) => clip(&mut p.content_mask),
    }
}

/// Every tile of `hashes`, sorted.
pub(crate) fn all_tiles(hashes: &FxHashMap<TileCoord, u64>) -> Vec<TileCoord> {
    let mut all: Vec<_> = hashes.keys().copied().collect();
    all.sort();
    all
}

/// Paints the content into the layer's scene and records it.
fn repaint(
    window: &mut Window,
    cx: &mut App,
    mut painting: Painting,
    background: Rgba,
    f: impl FnOnce(&mut Window, &mut App),
) {
    window.content_mask_stack.push(ContentMask {
        bounds: painting.painted_region,
    });
    mem::swap(&mut window.next_frame.scene, &mut painting.scene);
    let paint_start = window.paint_index();
    window.take_hover_reads();
    let hovers_start = window.retained_state.hover_dependencies.len();
    let recording = cx.begin_recording_dependencies();
    window.fast_layers.painting = Some(painting);

    f(window, cx);

    let mut painting = window
        .fast_layers
        .painting
        .take()
        .expect("the layer being painted");
    let paint_dependencies = cx.finish_recording_dependencies(recording).all;
    window.take_hover_reads();
    let hovers: Rc<[_]> = window.retained_state.hover_dependencies[hovers_start..].into();
    let paint_end = window.paint_index();
    mem::swap(&mut window.next_frame.scene, &mut painting.scene);
    window.content_mask_stack.pop();

    let scale_factor = window.scale_factor();
    let translation = painting.translation;
    let to_content = point(
        ScaledPixels(-translation.x.0),
        ScaledPixels(-translation.y.0),
    );
    let content = translated_scene(&painting.scene, to_content);
    let region = painting.painted_region.scale(scale_factor);
    let region = Bounds {
        origin: region.origin + to_content,
        size: region.size,
    };
    let (content, overlay) = crate::fast::layers::overlay::lift(content);
    let mut visible = painting.viewport.scale(scale_factor);
    visible.origin += to_content;
    let mut rendered_work = 0;
    let mut visible_work = 0;
    for operation in &content.paint_operations {
        if let PaintOperation::Primitive(primitive) = operation {
            rendered_work += 1;
            visible_work += usize::from(visible_bounds(primitive).intersects(&visible));
        }
    }
    policy::note_work(
        window,
        &painting.id,
        rendered_work as f32 / visible_work.max(1) as f32 * crate::fast::layers::work::REPAINT_COST,
    );
    let hashes = tile_hashes(&content, TILE_SIZE, region);

    let views = crate::fast::layers::invalidate::content_views(window, &painting.prepaint_range);
    let layer = layer_mut(window, &painting.id);
    let generation = layer.next_generation();
    let dirty = match &layer.record {
        Some(old) if old.background == background => dirty_tiles(&old.tile_hashes, &hashes),
        _ => all_tiles(&hashes),
    };
    layer.record = Some(LayerRecord {
        content: LayerContent::from(content),
        generation,
        painted_region: painting.painted_region,
        viewport: painting.viewport,
        scroll_offset: painting.scroll_offset,
        translation,
        prepaint_range: painting.prepaint_range,
        paint_range: paint_start..paint_end,
        tile_hashes: hashes,
        dirty_tiles: dirty,
        background,
        hovers,
        dependencies: painting.dependencies.union(&paint_dependencies),
        views,
        has_paths: false,
        overlay,
        view_layouts: Rc::new(mem::take(&mut painting.view_layouts)),
        a11y: painting.a11y.take(),
    });
    let dirtied = layer
        .record
        .as_ref()
        .map_or(0, |record| record.dirty_tiles.len());
    crate::fast::layers::input::painted(window, &painting.id, painting.input);
    window
        .layout_engine
        .as_mut()
        .unwrap()
        .retention
        .stats
        .layer_frames_repainted += 1;
    insert_layer(window, &painting.id, translation, dirtied);
}

/// Ends the layers' part of the frame being drawn, before it becomes the
/// rendered frame.
pub(crate) fn finish_frame(window: &mut Window) {
    debug_assert!(window.fast_layers.painting.is_none());
    for layer in window.fast_layers.layers.values_mut() {
        layer.prepainted = None;
        layer.rows.finish_frame();
    }
}

/// The layer of the container `id`, made if it has none.
pub(crate) fn layer_mut<'a>(window: &'a mut Window, id: &GlobalElementId) -> &'a mut Layer {
    let layers = &mut window.fast_layers;
    if !layers.layers.contains_key(id) {
        let key = LayerKey(layers.next_key % crate::fast::layers::scene::LAYER_KEY_LIMIT);
        layers.next_key = (layers.next_key + 1) % crate::fast::layers::scene::LAYER_KEY_LIMIT;
        layers.layers.insert(
            id.clone(),
            Layer {
                key,
                generation: 0,
                record: None,
                policy: Default::default(),
                input: Default::default(),
                rows: Default::default(),
                last_composited_frame: layers.frame,
                prepainted: None,
            },
        );
    }
    layers.layers.get_mut(id).unwrap()
}

/// Whether `style` scrolls on some axis.
fn scrolls(style: &Style) -> bool {
    style.overflow.x == Overflow::Scroll || style.overflow.y == Overflow::Scroll
}

/// The part of the content a layer paints: the viewport, and one viewport's
/// extent on each side along the scrolled axes as far as the content goes.
/// How far past the viewport a layer paints on each scrolled side, in
/// viewport extents. Measured on `gpui_perf`'s scroll scenarios: 2 cut the
/// mean frame of scroll-child-view by 44 % and of scroll-same-view by 21 %
/// against 1, at the cost of repaint frames about 1.7 times as long (one per
/// two viewports scrolled); 0.5 more than doubled the mean.
pub(crate) const OVERSCAN_VIEWPORTS: f32 = 2.;

pub(crate) fn painted_region(
    viewport: Bounds<Pixels>,
    content: Bounds<Pixels>,
    overflow: Point<Overflow>,
) -> Bounds<Pixels> {
    fn extend(
        min: Pixels,
        extent: Pixels,
        content_min: Pixels,
        content_extent: Pixels,
    ) -> (Pixels, Pixels) {
        let max = min + extent;
        let over = extent * OVERSCAN_VIEWPORTS;
        let low = (min - over).max(content_min).min(min);
        let high = (max + over).min(content_min + content_extent).max(max);
        (low, high - low)
    }
    let mut region = viewport;
    if overflow.x == Overflow::Scroll {
        (region.origin.x, region.size.width) = extend(
            viewport.origin.x,
            viewport.size.width,
            content.origin.x,
            content.size.width,
        );
    }
    if overflow.y == Overflow::Scroll {
        (region.origin.y, region.size.height) = extend(
            viewport.origin.y,
            viewport.size.height,
            content.origin.y,
            content.size.height,
        );
    }
    region
}

/// A layer's translation at `scroll_offset`: the offset in whole device
/// pixels.
pub(crate) fn translation(window: &Window, scroll_offset: Point<Pixels>) -> Point<ScaledPixels> {
    let scale_factor = window.scale_factor();
    point(
        ScaledPixels((scroll_offset.x.0 * scale_factor).round()),
        ScaledPixels((scroll_offset.y.0 * scale_factor).round()),
    )
}

/// `scene` moved by `delta`, finished.
fn translated_scene(scene: &Scene, delta: Point<ScaledPixels>) -> Scene {
    let mut translated = Scene::default();
    for operation in &scene.paint_operations {
        match operation {
            PaintOperation::Primitive(primitive) => {
                translated.insert_primitive(translate_primitive(primitive, delta))
            }
            PaintOperation::StartLayer(bounds) => translated.push_layer(Bounds {
                origin: bounds.origin + delta,
                size: bounds.size,
            }),
            PaintOperation::EndLayer => translated.pop_layer(),
        }
    }
    translated.finish();
    translated
}

/// `offset`, a scroll offset about to be applied, moved to whole device
/// pixels where layers are compiled, with or without a layer, so that a
/// layer's content and content drawn without one land on the same pixels
/// (spec §5.3).
pub(crate) fn snap_scroll_offset(window: &Window, offset: Point<Pixels>) -> Point<Pixels> {
    snap_offset(offset, window.scale_factor(), COMPILED)
}

/// [`snap_scroll_offset`] at `scale_factor`, snapping only when `compiled`,
/// rounding as [`Window::pixel_snap`] does.
pub(crate) fn snap_offset(
    offset: Point<Pixels>,
    scale_factor: f32,
    compiled: bool,
) -> Point<Pixels> {
    if !compiled {
        return offset;
    }
    offset.map(|value| {
        crate::px(crate::util::round_half_toward_zero(value.0 * scale_factor) / scale_factor)
    })
}
