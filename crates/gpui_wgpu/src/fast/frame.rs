//! A frame recorded with one instance upload and reused bind groups, drawing
//! the frame's scroll layer tiles and rounded clip scopes too.
//!
//! Upstream records a frame like this:
//!
//! - seven `Queue::write_buffer` calls, one per primitive kind and one for the
//!   path tiles, curves and paints, each with a bind group created for it;
//! - per sprite batch, a new texture bind group for its atlas texture;
//! - per batch, `set_pipeline` and every bind group again.
//!
//! On native backends every `write_buffer` allocates a staging buffer: that,
//! and creating bind groups, not the draws, is most of the renderer's CPU
//! time.
//!
//! Here all of the frame's instance data, of every scene it draws, goes
//! through one staging buffer; bind groups are kept by [`BindGroupCache`];
//! every scene's globals live in slots of one buffer written only when they
//! change; and pass state is set only when it changes. Each scene is drawn as
//! upstream draws its one: opaque rectangles first, front to back, writing
//! depth, then every batch at the depth of its place among the quads.
//!
//! The recording borrows the GPU objects it draws with through
//! [`FrameHost`], so the pixel tests' surfaceless harness records frames
//! through this same code as the on-screen renderer.

use std::ops::Range;

use anyhow::{Context as _, Result};
use gpui::{
    AtlasTextureId, PathCurve, PathPaint, PathTile, PrimitiveBatch, Scene, quad_depth,
    quad_depth_scale,
};

use crate::WgpuAtlas;
use crate::fast::bind_groups::BindGroupCache;
use crate::fast::globals::{DepthTargets, SceneGlobals};
use crate::fast::layers::{TileCache, composite, raster};
use crate::fast::pass_state::PassState;
use crate::wgpu_renderer::{
    GlobalParams, InstanceData, WgpuBindGroupLayouts, WgpuPipelines, WgpuRendererCore,
};

/// The renderer's state that outlives a frame.
#[derive(Default)]
pub(crate) struct FrameState {
    pub(crate) bind_groups: BindGroupCache,
    pub(crate) globals: SceneGlobals,
    pub(crate) depth: DepthTargets,
    pub(crate) layers: TileCache,
    paints: Vec<PathPaint>,
    curves: Vec<PathCurve>,
    tiles: Vec<PathTile>,
    scene_globals: Vec<GlobalParams>,
}

/// The GPU objects a frame is recorded with, borrowed from whoever owns them:
/// the on-screen renderer, or the pixel tests' surfaceless harness.
pub(crate) struct FrameTarget<'a> {
    pub(crate) dynamic_textures: &'a crate::fast::dynamic_textures::TextureStore,
    pub(crate) device: &'a wgpu::Device,
    pub(crate) queue: &'a wgpu::Queue,
    pub(crate) pipelines: &'a WgpuPipelines,
    pub(crate) bind_group_layouts: &'a WgpuBindGroupLayouts,
    pub(crate) atlas: &'a WgpuAtlas,
    pub(crate) atlas_sampler: &'a wgpu::Sampler,
    pub(crate) instance_data: InstanceSource<'a>,
    /// The buffer holding the gamma parameters, and where in it they are,
    /// for every scene's globals to share them.
    pub(crate) globals_buffer: &'a wgpu::Buffer,
    pub(crate) gamma_offset: u64,
    pub(crate) gamma_size: u64,
    /// The format of the frame's texture, which tiles share.
    pub(crate) format: wgpu::TextureFormat,
    pub(crate) premultiplied_alpha: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum InstanceSource<'a> {
    Storage(&'a wgpu::Buffer),
    Texture(&'a wgpu::TextureView),
}

/// The owner of the GPU objects a frame is recorded with.
pub(crate) trait FrameHost {
    fn frame_state(&mut self) -> &mut FrameState;
    /// The alignment of each array in the instance buffer.
    fn instance_data_alignment(&self) -> u64;
    /// Makes the instance buffer hold at least `size` bytes.
    fn reserve_instance_data(&mut self, size: u64) -> Result<()>;
    fn target(&self) -> Result<FrameTarget<'_>>;
    fn write_instance_texture(&self, _offset: u64, _data: &[u8]) {
        unreachable!("the storage-buffer host does not write an instance texture")
    }
}

impl FrameHost for WgpuRendererCore {
    fn write_instance_texture(&self, offset: u64, data: &[u8]) {
        WgpuRendererCore::write_instance_texture(self.resources(), offset, data);
    }

    fn frame_state(&mut self) -> &mut FrameState {
        &mut self.fast_frame
    }

    fn instance_data_alignment(&self) -> u64 {
        self.instance_data_alignment.max(1)
    }

    fn reserve_instance_data(&mut self, size: u64) -> Result<()> {
        if size > self.instance_data_capacity {
            self.grow_instance_data(size)?;
        }
        Ok(())
    }

    fn target(&self) -> Result<FrameTarget<'_>> {
        let resources = self.resources();
        let instance_data = match &resources.instance_data {
            InstanceData::Storage(buffer) => InstanceSource::Storage(buffer),
            InstanceData::Texture { view, .. } => InstanceSource::Texture(view),
        };
        Ok(FrameTarget {
            dynamic_textures: &self.fast_dynamic_textures,
            device: &resources.device,
            queue: &resources.queue,
            pipelines: &resources.pipelines,
            bind_group_layouts: &resources.bind_group_layouts,
            atlas: &self.atlas,
            atlas_sampler: &resources.atlas_sampler,
            instance_data,
            globals_buffer: &resources.globals_buffer,
            gamma_offset: self.gamma_offset,
            gamma_size: size_of::<crate::wgpu_renderer::GammaParams>() as u64,
            format: self.target_format,
            premultiplied_alpha: resources.pipelines.premultiplied_alpha,
        })
    }
}

/// Where one scene's primitives landed in the instance buffer.
pub(crate) struct SceneUpload {
    quads: InstanceBinding,
    shadows: InstanceBinding,
    underlines: InstanceBinding,
    monochrome_sprites: InstanceBinding,
    subpixel_sprites: InstanceBinding,
    polychrome_sprites: InstanceBinding,
}

struct InstanceBinding {
    group: wgpu::BindGroup,
    first: u32,
}

/// Where the frame's path tiles, curves and paints, those of every scene it
/// draws, landed in the instance buffer.
pub(crate) struct PathUpload {
    group: Option<wgpu::BindGroup>,
    tiles_first: u32,
    paint_base: u32,
    curve_base: u32,
}

/// A scene the frame draws.
pub(crate) struct PlannedScene<'a> {
    scene: &'a Scene,
    groups: Vec<GroupPlan>,
    /// The size of the texture the scene is drawn into.
    viewport: (u32, u32),
    /// The scene's first path tile in the frame's path tiles.
    tiles_start: u32,
    /// Whether the scene is a rounded clip scope's content, drawn masked.
    clipped: bool,
}

struct GroupPlan {
    texture: u32,
    content: usize,
}

pub(crate) struct DrawGraph<'a> {
    scenes: &'a [PlannedScene<'a>],
    uploads: &'a [SceneUpload],
    paths: &'a PathUpload,
}

struct PathData<'a> {
    paints: &'a mut Vec<PathPaint>,
    curves: &'a mut Vec<PathCurve>,
    tiles: &'a mut Vec<PathTile>,
}

fn plan_scene<'a>(
    scene: &'a Scene,
    viewport: (u32, u32),
    clipped: bool,
    scenes: &mut Vec<PlannedScene<'a>>,
    paths: &mut PathData,
) -> usize {
    let mut groups = Vec::new();
    for sprite in &scene.polychrome_sprites {
        let Some(frame) = crate::fast::rounded_clip::group(scene, sprite.tile.texture_id) else {
            continue;
        };
        let child = frame.content.scene().expect("rounded clip content");
        let content = plan_scene(child, viewport, true, scenes, paths);
        groups.push(GroupPlan {
            texture: sprite.tile.texture_id.index,
            content,
        });
    }
    let tiles_start = paths.tiles.len() as u32;
    for path in &scene.paths {
        path.append_to(paths.paints, paths.curves, paths.tiles);
    }
    let index = scenes.len();
    scenes.push(PlannedScene {
        scene,
        groups,
        viewport,
        tiles_start,
        clipped,
    });
    index
}

/// Forwarded to by `WgpuRendererCore::record_frame`, which draws both the
/// windows' frames and the headless renderer's. Records and submits the
/// frame. The storage-buffer and WebGL instance-texture transports share the
/// scene graph, including rounded clip scopes.
pub(crate) fn record_frame(
    renderer: &mut WgpuRendererCore,
    scene: &Scene,
    frame_view: &wgpu::TextureView,
    size: gpui::Size<gpui::DevicePixels>,
    clear_color: wgpu::Color,
) -> Result<Option<wgpu::SubmissionIndex>> {
    record_into(renderer, scene, frame_view, size, clear_color).map(Some)
}

/// Records `scene` into `frame_view`, of `size`, cleared to `clear` first, and
/// submits it.
pub(crate) fn record_into(
    host: &mut impl FrameHost,
    scene: &Scene,
    frame_view: &wgpu::TextureView,
    size: gpui::Size<gpui::DevicePixels>,
    clear: wgpu::Color,
) -> Result<wgpu::SubmissionIndex> {
    // The state is taken for the frame so that it can change while the host
    // lends out the GPU objects.
    let mut state = std::mem::take(host.frame_state());
    let result = record_with(host, &mut state, scene, frame_view, size, clear);
    *host.frame_state() = state;
    result
}

fn record_with(
    host: &mut impl FrameHost,
    state: &mut FrameState,
    scene: &Scene,
    frame_view: &wgpu::TextureView,
    size: gpui::Size<gpui::DevicePixels>,
    clear: wgpu::Color,
) -> Result<wgpu::SubmissionIndex> {
    state.bind_groups.begin_frame();
    state.depth.begin_frame();

    // The layer tiles to rasterize before the frame draws them.
    let planned = state
        .layers
        .begin_frame(&scene.layers, composite::composited_tiles(scene));
    let rasters = raster::plan(&scene.layers, &planned);

    let mut paints = std::mem::take(&mut state.paints);
    let mut curves = std::mem::take(&mut state.curves);
    let mut tiles = std::mem::take(&mut state.tiles);
    let mut path_data = PathData {
        paints: &mut paints,
        curves: &mut curves,
        tiles: &mut tiles,
    };
    let mut scenes = Vec::with_capacity(rasters.len() + 1);
    let mut raster_indices = Vec::with_capacity(rasters.len());
    for raster in &rasters {
        let tile_size = scene.layers.frames[raster.layer].tile_size;
        raster_indices.push(plan_scene(
            &raster.scene,
            (tile_size, tile_size),
            false,
            &mut scenes,
            &mut path_data,
        ));
    }
    let viewport = (size.width.0 as u32, size.height.0 as u32);
    plan_scene(scene, viewport, false, &mut scenes, &mut path_data);

    let result = upload(host, &state.bind_groups, &scenes, &paints, &curves, &tiles)
        .with_context(|| {
            format!(
                "scene too large: {} paths, {} shadows, {} quads, {} underlines, {} monochrome sprites, {} subpixel sprites, {} polychrome sprites, {} layer tiles to rasterize",
                scene.paths.len(),
                scene.shadows.len(),
                scene.quads.len(),
                scene.underlines.len(),
                scene.monochrome_sprites.len(),
                scene.subpixel_sprites.len(),
                scene.polychrome_sprites.len(),
                rasters.len(),
            )
        })
        .and_then(|(uploads, paths)| {
            let target = host.target()?;
            state.layers.prepare(&target, &scene.layers, &planned);
            let mut globals = std::mem::take(&mut state.scene_globals);
            globals.clear();
            globals.extend(scenes.iter().zip(&uploads).map(|(plan, upload)| {
                let quad_count = upload.quads.first + plan.scene.quads.len() as u32;
                GlobalParams {
                    viewport_size: [plan.viewport.0 as f32, plan.viewport.1 as f32],
                    path_paint_base: paths.paint_base,
                    path_curve_base: paths.curve_base,
                    depth_scale: quad_depth_scale(quad_count as usize),
                    quad_count,
                    premultiplied_alpha: target.premultiplied_alpha as u32,
                    // Read by `rounded_clip::shader`'s quad pass.
                    pad: plan.clipped as u32,
                }
            }));
            state.globals.write(&target, &globals);
            state.depth.ensure(target.device, viewport.0, viewport.1);
            for raster in &rasters {
                let tile_size = scene.layers.frames[raster.layer].tile_size;
                state.depth.ensure(target.device, tile_size, tile_size);
            }
            let graph = DrawGraph {
                scenes: &scenes,
                uploads: &uploads,
                paths: &paths,
            };
            let submission = record(
                &target,
                state,
                &globals,
                scene,
                frame_view,
                clear,
                &rasters,
                &raster_indices,
                &graph,
            );
            state.scene_globals = globals;
            Ok(submission)
        });
    if result.is_err() {
        // The tiles this frame was to rasterize were not.
        state.layers.clear();
    }

    drop(scenes);
    paints.clear();
    curves.clear();
    tiles.clear();
    state.paints = paints;
    state.curves = curves;
    state.tiles = tiles;
    result
}

/// Writes every scene's instance data, and the frame's paths. Storage arrays
/// share one staging buffer; WebGL arrays start on both a texel boundary and
/// a whole record, so its shaders can index them absolutely through the
/// instance texture.
fn upload(
    host: &mut impl FrameHost,
    bind_groups: &BindGroupCache,
    scenes: &[PlannedScene],
    paints: &[PathPaint],
    curves: &[PathCurve],
    tiles: &[PathTile],
) -> Result<(Vec<SceneUpload>, PathUpload)> {
    const SCENE_ARRAYS: usize = 6;
    let mut arrays: Vec<&[u8]> = Vec::with_capacity(scenes.len() * SCENE_ARRAYS + 3);
    // SAFETY: the primitives and path records are `#[repr(C)]` plain data, as
    // upstream's `write_instance_binding` relies on too.
    unsafe {
        for planned in scenes {
            let scene = planned.scene;
            arrays.extend([
                bytes_of(&scene.quads),
                bytes_of(&scene.shadows),
                bytes_of(&scene.underlines),
                bytes_of(&scene.monochrome_sprites),
                bytes_of(&scene.subpixel_sprites),
                bytes_of(&scene.polychrome_sprites),
            ]);
        }
        arrays.extend([bytes_of(paints), bytes_of(curves), bytes_of(tiles)]);
    }

    let alignment = host.instance_data_alignment();
    let texture_transport = matches!(host.target()?.instance_data, InstanceSource::Texture(_));
    let mut strides = [
        size_of::<gpui::Quad>(),
        size_of::<gpui::Shadow>(),
        size_of::<gpui::Underline>(),
        size_of::<gpui::MonochromeSprite>(),
        size_of::<gpui::SubpixelSprite>(),
        size_of::<gpui::PolychromeSprite>(),
    ]
    .repeat(scenes.len());
    strides.extend([
        size_of::<PathPaint>(),
        size_of::<PathCurve>(),
        size_of::<PathTile>(),
    ]);
    let mut offsets = Vec::with_capacity(arrays.len());
    let mut end = 0u64;
    for (data, &stride) in arrays.iter().zip(&strides) {
        let alignment = if texture_transport {
            common_multiple(alignment, stride as u64)
        } else {
            alignment
        };
        let offset = end.next_multiple_of(alignment);
        offsets.push(offset);
        // wgpu rejects zero-sized bindings, so empty arrays still reserve the
        // 16-byte minimum.
        end = offset + binding_size(data);
        if texture_transport {
            end = end.next_multiple_of(16);
        }
    }
    let end = end.next_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT);
    host.reserve_instance_data(end)?;

    let target = host.target()?;
    if let InstanceSource::Storage(buffer) = target.instance_data
        && arrays.iter().any(|data| !data.is_empty())
    {
        let size = wgpu::BufferSize::new(end).context("empty instance upload")?;
        let mut view = target
            .queue
            .write_buffer_with(buffer, 0, size)
            .context("instance upload rejected")?;
        for (&offset, data) in offsets.iter().zip(&arrays) {
            if !data.is_empty() {
                let offset = offset as usize;
                view.slice(offset..offset + data.len())
                    .copy_from_slice(data);
            }
        }
    }

    let texture_binding = if let InstanceSource::Texture(view) = target.instance_data {
        for (&offset, data) in offsets.iter().zip(&arrays) {
            if !data.is_empty() {
                host.write_instance_texture(offset, data);
            }
        }
        Some(target.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("frame_instance_texture"),
            layout: &target.bind_group_layouts.instances,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            }],
        }))
    } else {
        None
    };
    let first = |index: usize| match texture_transport {
        true => (offsets[index] / strides[index] as u64) as u32,
        false => 0,
    };

    let bind = |index: usize, label: &str| {
        if let Some(group) = &texture_binding {
            return InstanceBinding {
                group: group.clone(),
                first: first(index),
            };
        }
        let InstanceSource::Storage(buffer) = target.instance_data else {
            unreachable!()
        };
        InstanceBinding {
            group: bind_groups.storage(
                target.device,
                &target.bind_group_layouts.instances,
                label,
                buffer,
                offsets[index],
                binding_size(arrays[index]),
            ),
            first: 0,
        }
    };
    let uploads = (0..scenes.len())
        .map(|scene| {
            let first = scene * SCENE_ARRAYS;
            SceneUpload {
                quads: bind(first, "quads_bind_group"),
                shadows: bind(first + 1, "shadows_bind_group"),
                underlines: bind(first + 2, "underlines_bind_group"),
                monochrome_sprites: bind(first + 3, "monochrome_sprites_bind_group"),
                subpixel_sprites: bind(first + 4, "subpixel_sprites_bind_group"),
                polychrome_sprites: bind(first + 5, "polychrome_sprites_bind_group"),
            }
        })
        .collect();

    let (paint_index, curve_index, tile_index) = {
        let base = scenes.len() * SCENE_ARRAYS;
        (base, base + 1, base + 2)
    };
    let path_group = (!tiles.is_empty()).then(|| {
        let layout = &target.bind_group_layouts.path_tiles;
        match target.instance_data {
            InstanceSource::Texture(view) => {
                target.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("path_tiles_bind_group"),
                    layout,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(view),
                    }],
                })
            }
            InstanceSource::Storage(buffer) => {
                let entry = |binding: u32, index: usize| wgpu::BindGroupEntry {
                    binding,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer,
                        offset: offsets[index],
                        size: wgpu::BufferSize::new(binding_size(arrays[index])),
                    }),
                };
                target.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("path_tiles_bind_group"),
                    layout,
                    entries: &[
                        entry(0, tile_index),
                        entry(1, curve_index),
                        entry(2, paint_index),
                    ],
                })
            }
        }
    });
    let paths = PathUpload {
        group: path_group,
        tiles_first: first(tile_index),
        paint_base: first(paint_index),
        curve_base: first(curve_index),
    };
    Ok((uploads, paths))
}

fn binding_size(data: &[u8]) -> u64 {
    (data.len() as u64).max(16)
}

fn common_multiple(left: u64, right: u64) -> u64 {
    let (mut a, mut b) = (left, right);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    left / a * right
}

unsafe fn bytes_of<T>(instances: &[T]) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            instances.as_ptr() as *const u8,
            std::mem::size_of_val(instances),
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn record(
    target: &FrameTarget,
    state: &FrameState,
    globals: &[GlobalParams],
    scene: &Scene,
    frame_view: &wgpu::TextureView,
    clear: wgpu::Color,
    rasters: &[raster::TileRaster],
    raster_indices: &[usize],
    graph: &DrawGraph,
) -> wgpu::SubmissionIndex {
    let mut encoder = target
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("main_encoder"),
        });
    // Scenes are planned and uploaded tiles first, the frame's own last.
    for (index, tile) in rasters.iter().enumerate() {
        raster::rasterize(
            target,
            state,
            globals,
            &mut encoder,
            &scene.layers,
            tile,
            raster_indices[index],
            graph,
        );
    }
    let main = graph.scenes.len() - 1;
    draw_scene(
        target,
        state,
        globals,
        &mut encoder,
        &SceneDraw {
            scene: main,
            view: frame_view,
            clear,
            label: "main_pass",
            graph,
        },
    );
    target.queue.submit(std::iter::once(encoder.finish()))
}

/// A scene to draw, where, and with what.
pub(crate) struct SceneDraw<'a> {
    /// The scene's index in the frame's planned scenes.
    pub(crate) scene: usize,
    pub(crate) view: &'a wgpu::TextureView,
    pub(crate) clear: wgpu::Color,
    pub(crate) label: &'a str,
    pub(crate) graph: &'a DrawGraph<'a>,
}

enum StepKind {
    /// The scene's opaque rectangles, front to back, writing depth.
    Opaque,
    Batch(PrimitiveBatch),
}

struct DrawStep {
    scene: usize,
    kind: StepKind,
    clip: Option<usize>,
    /// The viewport depth range the step is drawn with.
    depth: (f32, f32),
    /// For a path batch, its tiles in the frame's path tiles.
    tiles: Range<u32>,
}

/// Collects the draws of scene `index`, whose depths map into `range` of the
/// target's depth.
///
/// Upstream draws a scene's opaque rectangles first, front to back, writing
/// depth, and everything else afterwards, at the depth of its place among the
/// quads, so what an opaque rectangle covers is culled. A rounded clip
/// scope's content is drawn in its parent's pass at the parent's place of the
/// clip, so its own depths are mapped into the range between the parent quads
/// before and after it; it has no opaque pass, its quad pass drawing every
/// quad masked (see `rounded_clip::shader`).
#[allow(clippy::too_many_arguments)]
fn collect_steps(
    index: usize,
    graph: &DrawGraph,
    clips: &mut Vec<crate::fast::rounded_clip::Clip>,
    binding: Option<usize>,
    range: (f32, f32),
    steps: &mut Vec<DrawStep>,
    clip_sets: &mut Vec<Vec<crate::fast::rounded_clip::Clip>>,
) {
    let plan = &graph.scenes[index];
    let scene = plan.scene;
    let base = graph.uploads[index].quads.first;
    let scale = quad_depth_scale((base as usize) + scene.quads.len());
    let map = |depth: f32| range.0 + depth * (range.1 - range.0);
    if !scene.quads.is_empty() && !plan.clipped {
        steps.push(DrawStep {
            scene: index,
            kind: StepKind::Opaque,
            clip: binding,
            depth: range,
            tiles: 0..0,
        });
    }
    let mut quad_cursor = 0u32;
    let mut tile_cursor = plan.tiles_start;
    for batch in scene.batches() {
        if let PrimitiveBatch::PolychromeSprites {
            texture_id,
            ref range,
        } = batch
            && crate::fast::rounded_clip::group(scene, texture_id).is_some()
        {
            let group = plan
                .groups
                .iter()
                .find(|group| group.texture == texture_id.index)
                .expect("planned clip group");
            let child = &graph.scenes[group.content];
            let at = base + quad_cursor;
            let before = if at == 0 {
                0.
            } else {
                quad_depth(at - 1, scale)
            };
            let after = quad_depth(at, scale);
            // The content doesn't write depth, so it may span up to the next
            // parent quad's depth: equal to it, it is culled where that quad,
            // drawn over it, is opaque.
            let child_range = (map(before), map(after));
            for index in range.clone() {
                clips.push(crate::fast::rounded_clip::Clip::new(
                    &scene.polychrome_sprites[index],
                    child.scene,
                ));
                let child_binding = clip_sets.len();
                clip_sets.push(clips.clone());
                collect_steps(
                    group.content,
                    graph,
                    clips,
                    Some(child_binding),
                    child_range,
                    steps,
                    clip_sets,
                );
                clips.pop();
            }
            continue;
        }
        let mut tiles = 0..0;
        let depth = match &batch {
            PrimitiveBatch::Quads(range) => {
                quad_cursor += range.len() as u32;
                (map(0.), map(1.))
            }
            batch => {
                if let PrimitiveBatch::Paths(range) = batch {
                    let count = scene.paths[range.clone()]
                        .iter()
                        .map(gpui::PaintedPath::tile_count)
                        .sum::<usize>() as u32;
                    tiles = tile_cursor..tile_cursor + count;
                    tile_cursor += count;
                }
                let depth = map(quad_depth(base + quad_cursor, scale));
                (depth, depth)
            }
        };
        steps.push(DrawStep {
            scene: index,
            kind: StepKind::Batch(batch),
            clip: binding,
            depth,
            tiles,
        });
    }
}

/// Records the pass that draws `draw.scene` into `draw.view`.
pub(crate) fn draw_scene(
    target: &FrameTarget,
    state: &FrameState,
    globals: &[GlobalParams],
    encoder: &mut wgpu::CommandEncoder,
    draw: &SceneDraw,
) {
    let graph = draw.graph;
    let pipelines = target.pipelines;
    let (width, height) = graph.scenes[draw.scene].viewport;
    let Some(depth_view) = state.depth.view(width, height) else {
        debug_assert!(false, "no depth target for the scene");
        return;
    };
    let texture_bind_group = |label: &str, view: &wgpu::TextureView| {
        state.bind_groups.texture(
            target.device,
            &target.bind_group_layouts.texture,
            target.atlas_sampler,
            label,
            view,
        )
    };

    let mut steps = Vec::new();
    let mut clip_sets = Vec::new();
    collect_steps(
        draw.scene,
        graph,
        &mut Vec::new(),
        None,
        (0., 1.),
        &mut steps,
        &mut clip_sets,
    );
    let clip_data = (!clip_sets.is_empty()).then(|| {
        target.bind_group_layouts.fast_rounded_clip.clip_sets(
            target.device,
            target.queue,
            &clip_sets,
        )
    });
    let clip_binding = clip_data
        .as_ref()
        .map(|data| &data.0)
        .unwrap_or(&target.bind_group_layouts.fast_rounded_clip.unmasked);
    let clip_alignment = clip_data.as_ref().map_or(0, |data| data.1);

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(draw.label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: draw.view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(draw.clear),
                store: wgpu::StoreOp::Store,
            },
            depth_slice: None,
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: depth_view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(0.0),
                store: wgpu::StoreOp::Discard,
            }),
            stencil_ops: None,
        }),
        ..Default::default()
    });
    let mut bound = PassState::default();
    let mut viewport = None;
    for step in steps {
        let upload = &graph.uploads[step.scene];
        let scene = graph.scenes[step.scene].scene;
        let scene_globals = state.globals.bind_group(step.scene);
        debug_assert!(step.scene < globals.len());
        let clip_offset = step
            .clip
            .map_or(0, |index| (index as u32 + 1) * clip_alignment);
        pass.set_bind_group(3, clip_binding, &[clip_offset]);
        if viewport != Some(step.depth) {
            pass.set_viewport(
                0.,
                0.,
                width as f32,
                height as f32,
                step.depth.0.clamp(0., 1.),
                step.depth.1.clamp(0., 1.),
            );
            viewport = Some(step.depth);
        }
        let batch = match step.kind {
            StepKind::Opaque => {
                bound.set_pipeline(&mut pass, &pipelines.opaque_quads);
                bound.set_bind_group(&mut pass, 0, scene_globals);
                bound.set_bind_group(&mut pass, 1, &upload.quads.group);
                pass.draw(0..4, 0..scene.quads.len() as u32);
                continue;
            }
            StepKind::Batch(batch) => batch,
        };
        match batch {
            PrimitiveBatch::Quads(range) => draw_batch(
                &mut pass,
                &mut bound,
                scene_globals,
                &pipelines.quads,
                &upload.quads,
                None,
                range,
            ),
            PrimitiveBatch::Shadows(range) => draw_batch(
                &mut pass,
                &mut bound,
                scene_globals,
                &pipelines.shadows,
                &upload.shadows,
                None,
                range,
            ),
            PrimitiveBatch::Paths(_) => {
                let Some(group) = &graph.paths.group else {
                    continue;
                };
                if step.tiles.is_empty() {
                    continue;
                }
                bound.set_pipeline(&mut pass, &pipelines.path_tiles);
                bound.set_bind_group(&mut pass, 0, scene_globals);
                bound.set_bind_group(&mut pass, 1, group);
                let first = graph.paths.tiles_first;
                pass.draw(
                    0..4,
                    (first + step.tiles.start)..(first + step.tiles.end),
                );
            }
            PrimitiveBatch::Underlines(range) => draw_batch(
                &mut pass,
                &mut bound,
                scene_globals,
                &pipelines.underlines,
                &upload.underlines,
                None,
                range,
            ),
            PrimitiveBatch::MonochromeSprites { texture_id, range } => {
                let Some(texture) = atlas_bind_group(target.atlas, &texture_bind_group, texture_id)
                else {
                    continue;
                };
                draw_batch(
                    &mut pass,
                    &mut bound,
                    scene_globals,
                    &pipelines.mono_sprites,
                    &upload.monochrome_sprites,
                    Some(&texture),
                    range,
                );
            }
            PrimitiveBatch::SubpixelSprites { texture_id, range } => {
                let Some(texture) = atlas_bind_group(target.atlas, &texture_bind_group, texture_id)
                else {
                    continue;
                };
                draw_batch(
                    &mut pass,
                    &mut bound,
                    scene_globals,
                    pipelines
                        .subpixel_sprites
                        .as_ref()
                        .unwrap_or(&pipelines.mono_sprites),
                    &upload.subpixel_sprites,
                    Some(&texture),
                    range,
                );
            }
            PrimitiveBatch::PolychromeSprites { texture_id, range }
                if composite::is_layer_texture(texture_id) =>
            {
                let sprites = &scene.polychrome_sprites;
                for run in composite::tile_runs(sprites, range) {
                    let Some(view) = composite::texture_for_batch(
                        &state.layers,
                        texture_id,
                        &sprites[run.clone()],
                    ) else {
                        continue;
                    };
                    draw_batch(
                        &mut pass,
                        &mut bound,
                        scene_globals,
                        &pipelines.poly_sprites,
                        &upload.polychrome_sprites,
                        Some(&texture_bind_group("layer_tile_bind_group", view)),
                        run,
                    );
                }
            }
            PrimitiveBatch::PolychromeSprites { texture_id, range } => {
                let Some(texture) = polychrome_bind_group(target, &texture_bind_group, texture_id)
                else {
                    continue;
                };
                draw_batch(
                    &mut pass,
                    &mut bound,
                    scene_globals,
                    &pipelines.poly_sprites,
                    &upload.polychrome_sprites,
                    Some(&texture),
                    range,
                );
            }
            // Surfaces are macOS-only for video playback and are not
            // implemented by the WGPU renderer.
            PrimitiveBatch::Surfaces(_surfaces) => {}
            // Native views are only composited by the Metal renderer.
            PrimitiveBatch::NativeViews(_) => {}
        }
    }
    drop(pass);
}

/// The bind group of an atlas texture, or `None` once the atlas has released
/// it: the batch then belongs to a stale paint that will be replaced once its
/// view renders again, and is skipped, as upstream's `draw_sprites` does.
fn atlas_bind_group(
    atlas: &WgpuAtlas,
    texture_bind_group: &impl Fn(&str, &wgpu::TextureView) -> wgpu::BindGroup,
    texture_id: AtlasTextureId,
) -> Option<wgpu::BindGroup> {
    let texture_info = atlas.get_texture_info(texture_id)?;
    Some(texture_bind_group(
        "atlas_texture_bind_group",
        &texture_info.view,
    ))
}

/// The bind group of a polychrome batch's texture: a dynamic texture's own,
/// or the atlas texture's. A dynamic texture the frame's device does not hold
/// is skipped like a released atlas texture, never looked up in the atlas.
fn polychrome_bind_group(
    target: &FrameTarget,
    texture_bind_group: &impl Fn(&str, &wgpu::TextureView) -> wgpu::BindGroup,
    texture_id: AtlasTextureId,
) -> Option<wgpu::BindGroup> {
    if let Some(id) = gpui::DynamicTextureId::from_atlas_texture_id(texture_id) {
        let view = target.dynamic_textures.view(id)?;
        return Some(texture_bind_group("dynamic_texture_bind_group", &view));
    }
    atlas_bind_group(target.atlas, texture_bind_group, texture_id)
}

fn draw_batch(
    pass: &mut wgpu::RenderPass<'_>,
    state: &mut PassState,
    globals: &wgpu::BindGroup,
    pipeline: &wgpu::RenderPipeline,
    instances: &InstanceBinding,
    texture: Option<&wgpu::BindGroup>,
    range: Range<usize>,
) {
    if range.is_empty() {
        return;
    }
    state.set_pipeline(pass, pipeline);
    state.set_bind_group(pass, 0, globals);
    state.set_bind_group(pass, 1, &instances.group);
    if let Some(texture) = texture {
        state.set_bind_group(pass, 2, texture);
    }
    pass.draw(
        0..4,
        (instances.first + range.start as u32)..(instances.first + range.end as u32),
    );
}

#[cfg(all(test, feature = "test-support", not(target_family = "wasm")))]
mod tests;
