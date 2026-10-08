//! Window frames redrawn only where their scene changed.
//!
//! Every finished scene carries its damage (`Scene::damage`, see gpui's
//! `fast::damage`): where it can draw different pixels than the scene drawn
//! before it. Here a window's renderer keeps the last frame in a texture of
//! its own, the **canvas**, and draws each frame into it: wholly, cleared, as
//! upstream draws into the drawable, or, when the canvas holds the scene the
//! damage compares with, only inside the damage. The canvas is then copied
//! into the drawable by a full-screen triangle that reads it texel for texel
//! (a blit cannot write a drawable: `MetalRenderer::configure_layer` leaves
//! the layer's drawables framebuffer-only, except in debug builds with
//! `test-support`, which turn that off for screenshots), and presented as
//! before.
//!
//! A partial frame loads the canvas in one render pass (one more per later
//! group of path batches, as a whole frame) and, in each, scissors to every
//! damage rectangle in turn, fills it with the clear color a whole frame
//! starts from in the first pass, and draws the pass's batches of the scene
//! (`fast::paths::encode_pass`). Overlapping rectangles are merged first, so
//! no pixel is drawn twice. Paths are rasterized into their intermediate
//! texture whole, as in a whole frame, and composited through the scissor, so
//! a pixel inside a rectangle comes out as a whole frame draws it.
//!
//! A scene drawn again (the number the canvas holds) is shown as the canvas
//! holds it.
//!
//! A frame is drawn whole when the canvas is missing or of another size, the
//! scene is not numbered or its damage does not compare with the scene the
//! canvas holds (the first frame, a frame dropped for want of a drawable, a
//! composed window's replayed scenes), the sprite atlas was written since the
//! last frame (a freed tile allocated again to another image keeps its id, so
//! the scene can draw the same sprite with different pixels), the scene has
//! surfaces (video frames change outside the scene), or the damage covers
//! more than half the window.
//!
//! Only `MetalRenderer::draw`, which presents to the window, goes through the
//! canvas. `render_to_image`, `render_scene_to_image` and `render_scene` draw
//! as upstream does: they render into a target of their own each time.
//!
//! Environment:
//! - `GPUI_PARTIAL_REDRAW=0` draws as upstream does, without the canvas.
//! - `GPUI_RENDER_STATS=1` prints, every second, each window's frames:
//!   `gpui render stats: secs=… partial_frames=… full_frames=… partial_px=…
//!   why_<reason>=…`.

use std::ffi::c_void;
use std::mem;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use gpui::{Bounds, DevicePixels, Scene, Size, point, size};

use crate::fast::paths::{PassStart, PathPlan, encode_pass};
use crate::metal_atlas::MetalAtlas;
use crate::metal_renderer::{InstanceBindings, InstanceBufferWriter, MetalRenderer};

#[cfg(test)]
mod tests;

/// How often a window's statistics are printed.
const STATS_PERIOD: Duration = Duration::from_secs(1);

const SHADERS: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct FastFullscreenVertex {
    float4 position [[position]];
};

// A triangle covering the whole viewport: (-1,-1), (3,-1), (-1,3).
vertex FastFullscreenVertex fast_fullscreen_vertex(uint vertex_id [[vertex_id]]) {
    float2 corner = float2((vertex_id << 1) & 2, vertex_id & 2);
    FastFullscreenVertex out;
    out.position = float4(corner * 2.0 - 1.0, 0.0, 1.0);
    return out;
}

fragment float4 fast_fill_fragment(
    FastFullscreenVertex in [[stage_in]],
    constant float4 &color [[buffer(0)]]
) {
    return color;
}

fragment float4 fast_copy_fragment(
    FastFullscreenVertex in [[stage_in]],
    texture2d<float, access::read> canvas [[texture(0)]]
) {
    return canvas.read(uint2(in.position.xy));
}
"#;

/// Whether frames may be drawn partially: `GPUI_PARTIAL_REDRAW` is not `0`.
fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("GPUI_PARTIAL_REDRAW").as_deref() != Ok("0"))
}

/// Whether `GPUI_RENDER_STATS=1`.
fn stats_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("GPUI_RENDER_STATS").is_ok_and(|value| value == "1"))
}

/// How many tiles a sprite atlas has written: held by its textures, counted
/// as they upload.
#[derive(Default)]
pub(crate) struct AtlasWrites(u64);

impl AtlasWrites {
    pub(crate) fn note(this: &mut Self) {
        this.0 = this.0.wrapping_add(1);
    }
}

impl MetalAtlas {
    /// The tiles this atlas has written so far.
    pub(crate) fn fast_writes(&self) -> u64 {
        self.0.lock().backend.fast_writes.0
    }
}

/// Why a frame was drawn whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FullReason {
    /// There is no canvas yet, or it was released.
    NoCanvas,
    /// The canvas is of another size than the drawable.
    Resized,
    /// The scene is not numbered, or its damage does not compare with the
    /// scene the canvas holds.
    NotComparable,
    /// The sprite atlas was written since the last frame.
    AtlasWritten,
    /// The scene has surfaces.
    Surfaces,
    /// The damage covers more than half the window.
    LargeDamage,
    /// `GPUI_PARTIAL_REDRAW=0`.
    Disabled,
}

impl FullReason {
    const ALL: [FullReason; 7] = [
        FullReason::NoCanvas,
        FullReason::Resized,
        FullReason::NotComparable,
        FullReason::AtlasWritten,
        FullReason::Surfaces,
        FullReason::LargeDamage,
        FullReason::Disabled,
    ];

    fn name(self) -> &'static str {
        match self {
            FullReason::NoCanvas => "no_canvas",
            FullReason::Resized => "resized",
            FullReason::NotComparable => "not_comparable",
            FullReason::AtlasWritten => "atlas_written",
            FullReason::Surfaces => "surfaces",
            FullReason::LargeDamage => "large_damage",
            FullReason::Disabled => "disabled",
        }
    }
}

/// How to draw a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Full(FullReason),
    /// Inside these rectangles only, clamped to the window, none empty. No
    /// rectangle at all: the frame draws what the canvas holds.
    Partial(Vec<Bounds<DevicePixels>>),
}

/// What [`decide`] decides from.
pub(crate) struct Frame<'a> {
    /// The canvas's size, if there is one.
    pub(crate) canvas: Option<Size<DevicePixels>>,
    /// The drawable's size.
    pub(crate) size: Size<DevicePixels>,
    /// The number of the scene the canvas holds, 0 for none.
    pub(crate) last_drawn: u64,
    /// `SceneDamage::frame`, `since` and `rects`.
    pub(crate) number: u64,
    pub(crate) since: u64,
    pub(crate) damage: &'a [Bounds<DevicePixels>],
    pub(crate) atlas_written: bool,
    pub(crate) has_surfaces: bool,
}

/// Decides how to draw `frame`; see the module documentation.
pub(crate) fn decide(frame: &Frame) -> Plan {
    let Some(canvas) = frame.canvas else {
        return Plan::Full(FullReason::NoCanvas);
    };
    if canvas != frame.size {
        return Plan::Full(FullReason::Resized);
    }
    let comparable = frame.since != 0 && frame.since == frame.last_drawn;
    let redrawn = frame.number == frame.last_drawn;
    if frame.number == 0 || !(comparable || redrawn) {
        return Plan::Full(FullReason::NotComparable);
    }
    if frame.atlas_written {
        return Plan::Full(FullReason::AtlasWritten);
    }
    if frame.has_surfaces {
        return Plan::Full(FullReason::Surfaces);
    }
    if redrawn {
        // The canvas holds this very scene.
        return Plan::Partial(Vec::new());
    }
    let window = Bounds {
        origin: point(DevicePixels(0), DevicePixels(0)),
        size: frame.size,
    };
    let rects = disjoint(
        frame
            .damage
            .iter()
            .map(|rect| rect.intersect(&window))
            .filter(|rect| rect.size.width.0 > 0 && rect.size.height.0 > 0)
            .collect(),
    );
    let damaged: i64 = rects.iter().map(area).sum();
    if damaged * 2 > area(&window) {
        return Plan::Full(FullReason::LargeDamage);
    }
    Plan::Partial(rects)
}

/// `rects` with every two that overlap replaced by their union, until none
/// overlap: a partial frame draws each rectangle once through the scene, and
/// a pixel in two would be drawn twice. Scene damage merges rectangles that
/// come near each other, so this rarely merges anything.
fn disjoint(mut rects: Vec<Bounds<DevicePixels>>) -> Vec<Bounds<DevicePixels>> {
    let overlap = |a: &Bounds<DevicePixels>, b: &Bounds<DevicePixels>| {
        let i = a.intersect(b);
        i.size.width.0 > 0 && i.size.height.0 > 0
    };
    'merge: loop {
        for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                if overlap(&rects[i], &rects[j]) {
                    let other = rects.swap_remove(j);
                    rects[i] = rects[i].union(&other);
                    continue 'merge;
                }
            }
        }
        return rects;
    }
}

fn area(rect: &Bounds<DevicePixels>) -> i64 {
    rect.size.width.0.max(0) as i64 * rect.size.height.0.max(0) as i64
}

/// A partial frame for `fast::paths::draw_primitives_to_texture` to encode:
/// its rectangles, and the color a whole frame is cleared to.
struct Pending {
    rects: Vec<metal::MTLScissorRect>,
    color: [f32; 4],
}

/// The pipelines of the fill and the copy, built from [`SHADERS`] once.
struct Pipelines {
    fill: metal::RenderPipelineState,
    copy: metal::RenderPipelineState,
}

/// A renderer's canvas and the bookkeeping of its partial frames.
#[derive(Default)]
pub(crate) struct PartialRedraw {
    canvas: Option<metal::Texture>,
    /// The number of the scene the canvas holds, 0 for none.
    last_drawn: u64,
    /// The atlas's writes when the canvas was last drawn.
    atlas_writes: u64,
    /// Whether the canvas was drawn for an opaque window (cleared to opaque
    /// black) or a transparent one.
    canvas_opaque: bool,
    /// `None` until the first frame; `Some(None)` when they failed to build.
    pipelines: Option<Option<Pipelines>>,
    pending: Option<Pending>,
    stats: Stats,
    /// The last frame's plan, for tests.
    #[cfg(test)]
    pub(crate) last_plan: Option<Plan>,
    /// The main passes over the canvas the last frame encoded, for tests.
    #[cfg(test)]
    pub(crate) main_passes: usize,
}

/// Forwarded to by `MetalRenderer::draw`: draws `scene` into the canvas, as
/// little of it as it can, and copies the canvas into `target`. Returns the
/// frame's command buffer, not committed.
pub(crate) fn render_frame(
    renderer: &mut MetalRenderer,
    scene: &Scene,
    target: &metal::TextureRef,
    viewport_size: Size<DevicePixels>,
) -> Result<metal::CommandBuffer> {
    let now = Instant::now();
    if !enabled() || viewport_size.width.0 <= 0 || viewport_size.height.0 <= 0 {
        renderer
            .fast_partial
            .stats
            .frame(&Plan::Full(FullReason::Disabled), now);
        return renderer.render_frame(scene, target, viewport_size);
    }
    let Some(pipelines) = pipelines(renderer) else {
        return renderer.render_frame(scene, target, viewport_size);
    };

    let this = &mut renderer.fast_partial;
    if this.canvas_opaque != renderer.opaque {
        // Drawn with the other clear color: none of it can be kept.
        this.canvas = None;
        this.canvas_opaque = renderer.opaque;
    }
    let canvas_size = this.canvas.as_ref().map(|canvas| {
        size(
            DevicePixels(canvas.width() as i32),
            DevicePixels(canvas.height() as i32),
        )
    });
    let atlas_writes = renderer.sprite_atlas.fast_writes();
    let damage = &scene.damage;
    let plan = decide(&Frame {
        canvas: canvas_size,
        size: viewport_size,
        last_drawn: this.last_drawn,
        number: damage.frame,
        since: damage.since,
        damage: &damage.rects,
        atlas_written: atlas_writes != this.atlas_writes,
        has_surfaces: !scene.surfaces.is_empty(),
    });

    if canvas_size != Some(viewport_size) {
        this.canvas = Some(new_canvas(&renderer.device, viewport_size));
    }
    let canvas = this.canvas.clone().context("canvas missing")?;
    let alpha = if renderer.opaque { 1. } else { 0. };
    this.pending = match &plan {
        Plan::Partial(rects) => Some(Pending {
            rects: rects.iter().map(scissor_rect).collect(),
            color: [0., 0., 0., alpha],
        }),
        Plan::Full(_) => None,
    };
    // Until the frame is drawn, the canvas holds no known scene.
    this.last_drawn = 0;
    #[cfg(test)]
    {
        this.main_passes = 0;
    }

    let result = renderer.render_frame(scene, &canvas, viewport_size);
    let this = &mut renderer.fast_partial;
    this.pending = None;
    let command_buffer = result?;

    copy(
        &pipelines.copy,
        &command_buffer,
        &canvas,
        target,
        viewport_size,
    );

    this.last_drawn = damage.frame;
    this.atlas_writes = atlas_writes;
    this.stats.frame(&plan, now);
    #[cfg(test)]
    {
        this.last_plan = Some(plan);
    }
    Ok(command_buffer)
}

/// Encodes the pending partial frame in place of a whole one, if there is
/// one: forwarded to by `fast::paths::draw_primitives_to_texture`.
pub(crate) fn draw_pending(
    renderer: &mut MetalRenderer,
    scene: &Scene,
    instance_bindings: &InstanceBindings,
    writer: &mut InstanceBufferWriter,
    texture: &metal::TextureRef,
    viewport_size: Size<DevicePixels>,
) -> Result<Option<metal::CommandBuffer>> {
    let Some(pending) = renderer.fast_partial.pending.take() else {
        return Ok(None);
    };
    let fill_pipeline = match &renderer.fast_partial.pipelines {
        Some(Some(pipelines)) => pipelines.fill.clone(),
        _ => return Err(anyhow!("partial frame without its pipelines")),
    };
    let paths = PathPlan::new(scene, writer)?;
    let command_queue = renderer.command_queue.clone();
    let command_buffer = command_queue.new_command_buffer();
    let color = pending.color;
    let fill = |encoder: &metal::RenderCommandEncoderRef| {
        encoder.set_render_pipeline_state(&fill_pipeline);
        encoder.set_fragment_bytes(
            0,
            mem::size_of_val(&color) as u64,
            color.as_ptr() as *const c_void,
        );
        encoder.draw_primitives(metal::MTLPrimitiveType::Triangle, 0, 3);
    };
    if !pending.rects.is_empty() {
        encode_pass(
            renderer,
            scene,
            instance_bindings,
            writer,
            &paths,
            texture,
            viewport_size,
            command_buffer,
            PassStart::Scissor {
                rects: &pending.rects,
                fill: &fill,
            },
        )?;
    }
    Ok(Some(command_buffer.to_owned()))
}

/// The fill and copy pipelines, built on the renderer's first frame.
fn pipelines(renderer: &mut MetalRenderer) -> Option<PipelinesRef> {
    if renderer.fast_partial.pipelines.is_none() {
        let built = build_pipelines(&renderer.device);
        if let Err(error) = &built {
            log::error!("partial redraw is off: {error:#}");
        }
        renderer.fast_partial.pipelines = Some(built.ok());
    }
    match &renderer.fast_partial.pipelines {
        Some(Some(pipelines)) => Some(PipelinesRef {
            copy: pipelines.copy.clone(),
        }),
        _ => None,
    }
}

/// What `render_frame` needs of the pipelines while the renderer is borrowed.
struct PipelinesRef {
    copy: metal::RenderPipelineState,
}

fn build_pipelines(device: &metal::DeviceRef) -> Result<Pipelines> {
    let library = device
        .new_library_with_source(SHADERS, &metal::CompileOptions::new())
        .map_err(|error| anyhow!("compiling the partial redraw shaders: {error}"))?;
    let pipeline = |label: &str, fragment: &str| -> Result<metal::RenderPipelineState> {
        let vertex_fn = library
            .get_function("fast_fullscreen_vertex", None)
            .map_err(|error| anyhow!("{error}"))?;
        let fragment_fn = library
            .get_function(fragment, None)
            .map_err(|error| anyhow!("{error}"))?;
        let descriptor = metal::RenderPipelineDescriptor::new();
        descriptor.set_label(label);
        descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
        descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
        let color_attachment = descriptor
            .color_attachments()
            .object_at(0)
            .context("pipeline has no color attachment")?;
        color_attachment.set_pixel_format(metal::MTLPixelFormat::BGRA8Unorm);
        color_attachment.set_blending_enabled(false);
        device
            .new_render_pipeline_state(&descriptor)
            .map_err(|error| anyhow!("building the {label} pipeline: {error}"))
    };
    Ok(Pipelines {
        fill: pipeline("partial_fill", "fast_fill_fragment")?,
        copy: pipeline("partial_copy", "fast_copy_fragment")?,
    })
}

/// A canvas of `size`, in the drawables' format, for the GPU only.
fn new_canvas(device: &metal::DeviceRef, size: Size<DevicePixels>) -> metal::Texture {
    let descriptor = metal::TextureDescriptor::new();
    descriptor.set_width(size.width.0 as u64);
    descriptor.set_height(size.height.0 as u64);
    descriptor.set_pixel_format(metal::MTLPixelFormat::BGRA8Unorm);
    descriptor.set_storage_mode(metal::MTLStorageMode::Private);
    descriptor.set_usage(metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead);
    device.new_texture(&descriptor)
}

fn scissor_rect(rect: &Bounds<DevicePixels>) -> metal::MTLScissorRect {
    metal::MTLScissorRect {
        x: rect.origin.x.0.max(0) as u64,
        y: rect.origin.y.0.max(0) as u64,
        width: rect.size.width.0.max(0) as u64,
        height: rect.size.height.0.max(0) as u64,
    }
}

/// Encodes a pass copying `canvas` into `target`, texel for texel.
fn copy(
    pipeline: &metal::RenderPipelineStateRef,
    command_buffer: &metal::CommandBufferRef,
    canvas: &metal::TextureRef,
    target: &metal::TextureRef,
    viewport_size: Size<DevicePixels>,
) {
    let descriptor = metal::RenderPassDescriptor::new();
    let Some(color_attachment) = descriptor.color_attachments().object_at(0) else {
        return;
    };
    color_attachment.set_texture(Some(target));
    color_attachment.set_load_action(metal::MTLLoadAction::DontCare);
    color_attachment.set_store_action(metal::MTLStoreAction::Store);
    let encoder = command_buffer.new_render_command_encoder(descriptor);
    encoder.set_viewport(metal::MTLViewport {
        originX: 0.0,
        originY: 0.0,
        width: viewport_size.width.0 as f64,
        height: viewport_size.height.0 as f64,
        znear: 0.0,
        zfar: 1.0,
    });
    encoder.set_render_pipeline_state(pipeline);
    encoder.set_fragment_texture(0, Some(canvas));
    encoder.draw_primitives(metal::MTLPrimitiveType::Triangle, 0, 3);
    encoder.end_encoding();
}

/// One window's frames since its statistics were last printed.
#[derive(Default)]
struct Stats {
    period_start: Option<Instant>,
    partial: u64,
    full: u64,
    partial_pixels: u64,
    reasons: [u64; FullReason::ALL.len()],
}

impl Stats {
    fn frame(&mut self, plan: &Plan, now: Instant) {
        if !stats_enabled() {
            return;
        }
        match plan {
            Plan::Partial(rects) => {
                self.partial += 1;
                self.partial_pixels += rects.iter().map(area).sum::<i64>() as u64;
            }
            Plan::Full(reason) => {
                self.full += 1;
                self.reasons[*reason as usize] += 1;
            }
        }
        let start = *self.period_start.get_or_insert(now);
        let elapsed = now.saturating_duration_since(start);
        if elapsed < STATS_PERIOD {
            return;
        }
        eprintln!("{}", self.summary(elapsed));
        *self = Stats {
            period_start: Some(now),
            ..Stats::default()
        };
    }

    fn summary(&self, elapsed: Duration) -> String {
        let mut line = format!(
            "gpui render stats: secs={:.2} partial_frames={} full_frames={} partial_px={}",
            elapsed.as_secs_f64(),
            self.partial,
            self.full,
            self.partial_pixels,
        );
        for reason in FullReason::ALL {
            let count = self.reasons[reason as usize];
            if count > 0 {
                line.push_str(&format!(" why_{}={count}", reason.name()));
            }
        }
        line
    }
}
