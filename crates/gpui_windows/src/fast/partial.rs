//! Drawing only what a frame changes: partial redraws on Direct3D 11.
//!
//! Every finished scene carries its damage (`Scene::damage`): the rectangles,
//! in device pixels, where it can draw different pixels than the scene drawn
//! before it. The renderer draws every frame into a **canvas**, a texture of
//! the window's size it keeps from frame to frame, then copies the canvas
//! into the swap chain's back buffer and presents. A frame whose damage is
//! relative to the scene the canvas holds is a **partial** frame: each damage
//! rectangle is cleared and the scene's batches drawn into it, scissored to
//! it, and the frame is presented with `Present1` and the damage as its
//! dirty rectangles, so that DWM recomposes only those. Any other frame is
//! drawn whole, as `DirectXRenderer::render` draws it.
//!
//! The canvas, rather than the back buffer, holds the frame drawn last: with
//! `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL` a back buffer holds the frame presented
//! several presents ago, and a copy of the whole canvas keeps every back
//! buffer right whatever DXGI does with them.
//!
//! A frame is drawn whole when:
//! - there is no canvas of the window's size (the first frame, a resize, a
//!   lost device);
//! - the scene is not numbered, or its damage is not relative to the scene
//!   the canvas holds (`since` is not the scene drawn last);
//! - an atlas tile was written since the last frame (atlas content is outside
//!   the scene: a tile freed and allocated again keeps its id);
//! - the scene has surfaces (video frames);
//! - its damage covers more than half the window;
//! - a graphics debugger is capturing (upstream's labeled loop then draws);
//! - `GPUI_PARTIAL_REDRAW=0`, which also keeps the old path without a canvas.
//!
//! Windows that compose native content draw through `fast::composition`,
//! straight into their swap chains: those frames do not touch the canvas, so
//! the next frame here finds its damage relative to a scene the canvas does
//! not hold and is drawn whole.
//!
//! With `GPUI_RENDER_STATS=1` each window prints, every second, a line of
//! `key=value` pairs to stderr: partial and whole frames, the pixels the
//! partial ones drew, and why frames were drawn whole (`why_<reason>`).

use std::slice;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use gpui::{Bounds, DevicePixels, Scene, WindowBackgroundAppearance};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_RASTERIZER_DESC, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    ID3D11Device, ID3D11DeviceContext, ID3D11DeviceContext1, ID3D11RasterizerState,
    ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    Common::DXGI_SAMPLE_DESC, DXGI_PRESENT, DXGI_PRESENT_PARAMETERS,
};
use windows::core::Interface as _;

use crate::DirectXRenderer;
use crate::directx_renderer::{DirectXRenderPipelines, GlobalParams, RENDER_TARGET_FORMAT};
use crate::fast::frame::{FrameState, Target, draw_scene, upload};

/// Damage rectangles a scene carries at most (`fast::damage` merges past it).
const MAX_RECTS: usize = 16;

/// How often a window's statistics are printed.
const STATS_PERIOD: Duration = Duration::from_secs(1);

/// Atlas tiles written by every atlas of the process, ever.
static ATLAS_WRITES: AtomicU64 = AtomicU64::new(0);

/// Notes that an atlas tile was written: the next frame of every window is
/// drawn whole.
pub(crate) fn atlas_written() {
    ATLAS_WRITES.fetch_add(1, Ordering::Relaxed);
}

/// Whether partial frames are on (`GPUI_PARTIAL_REDRAW` is not `0`).
fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("GPUI_PARTIAL_REDRAW").as_deref() != Ok("0"))
}

/// Whether `GPUI_RENDER_STATS=1`.
fn stats_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("GPUI_RENDER_STATS").is_ok_and(|value| value == "1"))
}

/// The renderer's partial-redraw state, kept from frame to frame.
#[derive(Default)]
pub(crate) struct PartialRedraw {
    canvas: Option<Canvas>,
    /// The number of the scene the canvas holds, 0 for none.
    last_drawn: u64,
    /// [`ATLAS_WRITES`] when the canvas was last drawn.
    atlas_writes: u64,
    /// The device's rasterizer state with the scissor test on.
    scissor_state: Option<ID3D11RasterizerState>,
    stats: Stats,
}

struct Canvas {
    texture: ID3D11Texture2D,
    view: Option<ID3D11RenderTargetView>,
    width: u32,
    height: u32,
}

/// Why a frame was drawn whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Whole {
    NoCanvas,
    NotNumbered,
    NotComparable,
    AtlasWritten,
    Surfaces,
    LargeDamage,
    Capturing,
}

impl Whole {
    const ALL: [Whole; 7] = [
        Whole::NoCanvas,
        Whole::NotNumbered,
        Whole::NotComparable,
        Whole::AtlasWritten,
        Whole::Surfaces,
        Whole::LargeDamage,
        Whole::Capturing,
    ];

    fn name(self) -> &'static str {
        match self {
            Whole::NoCanvas => "no_canvas",
            Whole::NotNumbered => "not_numbered",
            Whole::NotComparable => "not_comparable",
            Whole::AtlasWritten => "atlas_written",
            Whole::Surfaces => "surfaces",
            Whole::LargeDamage => "large_damage",
            Whole::Capturing => "capturing",
        }
    }
}

/// How a frame is drawn.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Plan {
    Whole(Whole),
    /// Inside these rectangles only, clamped to the window; none when the
    /// frame changes nothing.
    Partial(Vec<RECT>),
}

/// What [`plan`] decides from.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Frame<'a> {
    pub(crate) has_canvas: bool,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) number: u64,
    pub(crate) since: u64,
    pub(crate) last_drawn: u64,
    pub(crate) atlas_written: bool,
    pub(crate) has_surfaces: bool,
    pub(crate) capturing: bool,
    pub(crate) damage: &'a [Bounds<DevicePixels>],
}

/// Decides how to draw `frame`.
pub(crate) fn plan(frame: &Frame) -> Plan {
    if !frame.has_canvas {
        return Plan::Whole(Whole::NoCanvas);
    }
    if frame.number == 0 {
        return Plan::Whole(Whole::NotNumbered);
    }
    if frame.since == 0 || frame.since != frame.last_drawn {
        return Plan::Whole(Whole::NotComparable);
    }
    if frame.atlas_written {
        return Plan::Whole(Whole::AtlasWritten);
    }
    if frame.has_surfaces {
        return Plan::Whole(Whole::Surfaces);
    }
    if frame.capturing {
        return Plan::Whole(Whole::Capturing);
    }
    let (width, height) = (frame.width as i32, frame.height as i32);
    let rects: Vec<RECT> = frame
        .damage
        .iter()
        .filter_map(|rect| {
            let clamped = RECT {
                left: rect.origin.x.0.clamp(0, width),
                top: rect.origin.y.0.clamp(0, height),
                right: rect
                    .origin
                    .x
                    .0
                    .saturating_add(rect.size.width.0)
                    .clamp(0, width),
                bottom: rect
                    .origin
                    .y
                    .0
                    .saturating_add(rect.size.height.0)
                    .clamp(0, height),
            };
            (clamped.left < clamped.right && clamped.top < clamped.bottom).then_some(clamped)
        })
        .collect();
    let area: i64 = rects.iter().map(area).sum();
    if rects.len() > MAX_RECTS || area * 2 > i64::from(frame.width) * i64::from(frame.height) {
        return Plan::Whole(Whole::LargeDamage);
    }
    Plan::Partial(rects)
}

fn area(rect: &RECT) -> i64 {
    i64::from(rect.right - rect.left) * i64::from(rect.bottom - rect.top)
}

/// Forgets the canvas and the device objects: on a lost device.
pub(crate) fn release(renderer: &mut DirectXRenderer) {
    let partial = &mut renderer.fast_partial;
    partial.canvas = None;
    partial.scissor_state = None;
    partial.last_drawn = 0;
}

/// Draws `scene` and presents it, in place of `DirectXRenderer::draw`.
pub(crate) fn draw(
    renderer: &mut DirectXRenderer,
    scene: &Scene,
    background_appearance: WindowBackgroundAppearance,
) -> Result<()> {
    if !enabled() {
        renderer.render(scene, background_appearance)?;
        return renderer.present();
    }
    let started = Instant::now();
    ensure_canvas(renderer)?;
    let capturing = renderer
        .devices
        .as_ref()
        .and_then(|devices| devices.annotation.as_ref())
        .is_some_and(|annotation| unsafe { annotation.GetStatus().as_bool() });
    let atlas_writes = ATLAS_WRITES.load(Ordering::Relaxed);
    let partial = &renderer.fast_partial;
    let plan = plan(&Frame {
        has_canvas: partial.canvas.is_some(),
        width: renderer.width,
        height: renderer.height,
        number: scene.damage.frame,
        since: scene.damage.since,
        last_drawn: partial.last_drawn,
        atlas_written: atlas_writes != partial.atlas_writes,
        has_surfaces: !scene.surfaces.is_empty(),
        capturing,
        damage: &scene.damage.rects,
    });

    // `render` and the batches draw into the window's render target view,
    // which paths also bind again after their intermediate pass: lend it the
    // canvas's for the draw.
    swap_render_target(renderer);
    let drawn = match &plan {
        Plan::Whole(_) => renderer.render(scene, background_appearance),
        Plan::Partial(rects) => draw_partial(renderer, scene, background_appearance, rects),
    };
    swap_render_target(renderer);
    drawn?;

    copy_canvas_to_back_buffer(renderer)?;
    match &plan {
        Plan::Partial(rects) => present_dirty(renderer, rects)?,
        Plan::Whole(_) => renderer.present()?,
    }

    let partial = &mut renderer.fast_partial;
    partial.last_drawn = scene.damage.frame;
    partial.atlas_writes = atlas_writes;
    partial.stats.frame(&plan, started.elapsed());
    Ok(())
}

/// Makes the canvas the window's size, creating it if needed.
fn ensure_canvas(renderer: &mut DirectXRenderer) -> Result<()> {
    let (width, height) = (renderer.width, renderer.height);
    if renderer
        .fast_partial
        .canvas
        .as_ref()
        .is_some_and(|canvas| canvas.width == width && canvas.height == height)
    {
        return Ok(());
    }
    renderer.fast_partial.canvas = None;
    renderer.fast_partial.last_drawn = 0;
    let device = &renderer.devices.as_ref().context("devices missing")?.device;
    let texture = create_canvas_texture(device, width, height)?;
    let mut view = None;
    unsafe { device.CreateRenderTargetView(&texture, None, Some(&mut view))? };
    renderer.fast_partial.canvas = Some(Canvas {
        texture,
        view,
        width,
        height,
    });
    Ok(())
}

/// A render target of the swap chain's format, `width` × `height`.
pub(crate) fn create_canvas_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: RENDER_TARGET_FORMAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    texture.context("creating the partial-redraw canvas")
}

/// Exchanges the window's render target view with the canvas's.
fn swap_render_target(renderer: &mut DirectXRenderer) {
    if let (Some(resources), Some(canvas)) = (
        renderer.resources.as_mut(),
        renderer.fast_partial.canvas.as_mut(),
    ) {
        std::mem::swap(&mut resources.render_target_view, &mut canvas.view);
    }
}

/// Draws `scene` into the canvas, bound as the window's render target view,
/// inside `rects` only.
fn draw_partial(
    renderer: &mut DirectXRenderer,
    scene: &Scene,
    background_appearance: WindowBackgroundAppearance,
    rects: &[RECT],
) -> Result<()> {
    crate::fast::layers::raster::rasterize_tiles(renderer, scene)?;
    if rects.is_empty() {
        return Ok(());
    }
    let devices = renderer.devices.as_ref().context("devices missing")?;
    let resources = renderer.resources.as_ref().context("resources missing")?;
    let view = resources
        .render_target_view
        .as_ref()
        .context("missing render target view")?;
    let scissor_state = match &renderer.fast_partial.scissor_state {
        Some(state) => state.clone(),
        None => {
            let state = create_scissor_state(&devices.device, &devices.device_context)?;
            renderer.fast_partial.scissor_state = Some(state.clone());
            state
        }
    };
    let device_context = &devices.device_context;
    // What `DirectXRenderer::pre_draw` binds, without clearing the target.
    crate::fast::globals::write_globals(
        &renderer.fast_frame,
        device_context,
        renderer
            .globals
            .global_params_buffer
            .as_ref()
            .context("global params buffer missing")?,
        &[GlobalParams {
            gamma_ratios: renderer.font_info.gamma_ratios,
            viewport_size: [resources.viewport.Width, resources.viewport.Height],
            grayscale_enhanced_contrast: renderer.font_info.grayscale_enhanced_contrast,
            subpixel_enhanced_contrast: renderer.font_info.subpixel_enhanced_contrast,
            is_bgr: renderer.font_info.is_bgr as u32,
            _pad: [0; 3],
        }],
    )?;
    unsafe {
        device_context
            .OMSetRenderTargets(Some(slice::from_ref(&resources.render_target_view)), None);
        device_context.RSSetViewports(Some(slice::from_ref(&resources.viewport)));
        device_context.VSSetConstantBuffers(
            0,
            Some(slice::from_ref(&renderer.globals.global_params_buffer)),
        );
        device_context.VSSetConstantBuffers(
            1,
            Some(slice::from_ref(&renderer.globals.batch_params_buffer)),
        );
        device_context.PSSetConstantBuffers(
            0,
            Some(slice::from_ref(&renderer.globals.global_params_buffer)),
        );
    }
    let target = Target {
        device: &devices.device,
        device_context,
        atlas: &renderer.atlas,
        globals: &renderer.globals,
        resources: Some(resources),
    };
    let view = view.clone();
    draw_rects(
        &target,
        &mut renderer.pipelines,
        &mut renderer.fast_frame,
        scene,
        &view,
        &scissor_state,
        clear_color(background_appearance),
        rects,
    )
}

/// The color `DirectXRenderer::render` clears the target to.
pub(crate) fn clear_color(background_appearance: WindowBackgroundAppearance) -> [f32; 4] {
    match background_appearance {
        WindowBackgroundAppearance::Opaque => [1.0; 4],
        _ => [0.0; 4],
    }
}

/// Uploads `scene`'s instances and draws it into `view`, bound with its
/// viewport and globals, inside `rects` only: each cleared to `clear`, then
/// the scene's batches drawn with `scissor_state`, scissored to it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_rects(
    target: &Target<'_>,
    pipelines: &mut DirectXRenderPipelines,
    frame: &mut FrameState,
    scene: &Scene,
    view: &ID3D11RenderTargetView,
    scissor_state: &ID3D11RasterizerState,
    clear: [f32; 4],
    rects: &[RECT],
) -> Result<()> {
    if rects.is_empty() {
        // `ClearView` with no rectangle clears the whole view.
        return Ok(());
    }
    let context = target.device_context;
    let context1: ID3D11DeviceContext1 = context
        .cast()
        .context("partial redraws need Direct3D 11.1")?;
    unsafe { context1.ClearView(view, &clear, Some(rects)) };
    upload(target, pipelines, scene)?;
    let previous = unsafe { context.RSGetState() }.ok();
    unsafe { context.RSSetState(scissor_state) };
    let drawn = (|| {
        for rect in rects {
            unsafe { context.RSSetScissorRects(Some(slice::from_ref(rect))) };
            draw_scene(target, pipelines, frame, scene)?;
        }
        Ok(())
    })();
    unsafe { context.RSSetState(previous.as_ref()) };
    drawn
}

/// The rasterizer state bound on `device_context`, with the scissor test on.
pub(crate) fn create_scissor_state(
    device: &ID3D11Device,
    device_context: &ID3D11DeviceContext,
) -> Result<ID3D11RasterizerState> {
    let current =
        unsafe { device_context.RSGetState() }.context("no rasterizer state bound to copy")?;
    let mut desc = D3D11_RASTERIZER_DESC::default();
    unsafe { current.GetDesc(&mut desc) };
    desc.ScissorEnable = true.into();
    let mut state = None;
    unsafe { device.CreateRasterizerState(&desc, Some(&mut state))? };
    state.context("creating the scissor rasterizer state")
}

/// Copies the whole canvas into the swap chain's back buffer.
fn copy_canvas_to_back_buffer(renderer: &DirectXRenderer) -> Result<()> {
    let devices = renderer.devices.as_ref().context("devices missing")?;
    let resources = renderer.resources.as_ref().context("resources missing")?;
    let canvas = renderer
        .fast_partial
        .canvas
        .as_ref()
        .context("canvas missing")?;
    let back_buffer = resources
        .render_target
        .as_ref()
        .context("render target missing")?;
    unsafe {
        devices
            .device_context
            .CopyResource(back_buffer, &canvas.texture)
    };
    Ok(())
}

/// Presents the back buffer, telling DXGI that only `rects` changed since
/// the frame presented before (a pixel when nothing changed).
fn present_dirty(renderer: &DirectXRenderer, rects: &[RECT]) -> Result<()> {
    let resources = renderer.resources.as_ref().context("resources missing")?;
    let mut dirty: Vec<RECT> = rects.to_vec();
    if dirty.is_empty() {
        // No dirty rectangle means the whole frame to DXGI.
        dirty.push(RECT {
            left: 0,
            top: 0,
            right: 1,
            bottom: 1,
        });
    }
    let parameters = DXGI_PRESENT_PARAMETERS {
        DirtyRectsCount: dirty.len() as u32,
        pDirtyRects: dirty.as_mut_ptr(),
        pScrollRect: std::ptr::null_mut(),
        pScrollOffset: std::ptr::null_mut(),
    };
    unsafe {
        resources
            .swap_chain
            .Present1(0, DXGI_PRESENT(0), &parameters)
    }
    .ok()
    .context("Presenting swap chain failed")
}

/// A window's frames since its statistics were last printed.
#[derive(Default)]
struct Stats {
    period_start: Option<Instant>,
    partial: u64,
    whole: u64,
    partial_pixels: i64,
    time: Duration,
    reasons: [u64; Whole::ALL.len()],
}

impl Stats {
    fn frame(&mut self, plan: &Plan, took: Duration) {
        if !stats_enabled() {
            return;
        }
        let now = Instant::now();
        let start = *self.period_start.get_or_insert(now);
        match plan {
            Plan::Partial(rects) => {
                self.partial += 1;
                self.partial_pixels += rects.iter().map(area).sum::<i64>();
            }
            Plan::Whole(reason) => {
                self.whole += 1;
                if let Some(index) = Whole::ALL.iter().position(|r| r == reason) {
                    self.reasons[index] += 1;
                }
            }
        }
        self.time += took;
        let elapsed = now.saturating_duration_since(start);
        if elapsed < STATS_PERIOD {
            return;
        }
        let frames = self.partial + self.whole;
        let mut line = format!(
            "gpui render stats: secs={:.2} partial_frames={} full_frames={} partial_px={} \
             ms_mean={:.3}",
            elapsed.as_secs_f64(),
            self.partial,
            self.whole,
            self.partial_pixels,
            self.time.as_secs_f64() * 1e3 / frames.max(1) as f64,
        );
        for (reason, count) in Whole::ALL.iter().zip(self.reasons) {
            if count > 0 {
                line.push_str(&format!(" why_{}={count}", reason.name()));
            }
        }
        eprintln!("{line}");
        *self = Stats {
            period_start: Some(now),
            ..Stats::default()
        };
    }
}

#[cfg(test)]
mod tests;
