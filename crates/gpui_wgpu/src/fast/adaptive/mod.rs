//! Drawing a frame on the CPU when it changes little, and on the GPU
//! otherwise.
//!
//! `WgpuRenderer::draw` first calls [`draw`]. Per frame, [`policy::Policy`]
//! chooses the path from the scene's damage (`Scene::damage`), the atlas
//! tiles written since the last frame, what the CPU's frame in memory (the
//! canvas) still shows, and how much the CPU frames of the current burst
//! cost. A CPU frame redraws the canvas only inside its region (damage, stale
//! pixels the GPU drew since, sprites over written atlas tiles) with
//! `cpu::raster` and shows it, with that region as its damage, in one of two
//! ways ([`PresentMode`]):
//!
//! - **native**: through the platform's [`CpuPresenter`]
//!   ([`WgpuRenderer::set_cpu_presenter`]; `wl_shm` on Wayland, `PutImage` on
//!   X11), without the GPU;
//! - **blit**: through the GPU ([`blit`]): the region is uploaded to a
//!   texture that a tiny pipeline copies to the swapchain image. It needs no
//!   presenter, so it works on every window.
//!
//! A GPU frame is drawn as before; its damage becomes stale for the canvas
//! ([`Adaptive::gpu_drew`]). The renderer's first frame always draws on the
//! GPU.
//!
//! Environment:
//! - `GPUI_CPU_RENDER=0` never draws on the CPU (the presenter is not kept,
//!   the atlas is not mirrored); `GPUI_CPU_RENDER=always` draws every frame
//!   it can on the CPU, without the size, burst and cost limits, for
//!   measurements.
//! - `GPUI_CPU_PRESENT=native` shows CPU frames with the platform's presenter,
//!   or by blit where the platform installed none; `GPUI_CPU_PRESENT=blit`
//!   always by blit; unset, [`default_present_mode`] decides. A renderer
//!   resolves its mode on its first frame.
//! - `GPUI_RENDER_STATS=1` logs (and prints to stderr) each window's frames
//!   every second, checked when a frame is drawn: frames per path and
//!   presentation mode, pixels the CPU drew, CPU frame times, the CPU-side
//!   time of GPU frames (`WgpuRenderer::draw` from here to `frame.present()`),
//!   and why frames went to the GPU.
//!
//! Atlas: the CPU samples sprites from the atlas's CPU copy, which a new atlas
//! keeps from its first upload (see `cpu::atlas`). A renderer that resolves
//! to no CPU frames turns it off on its first frame.
//!
//! Presenting failures: a presenter may refuse a frame (Wayland's before the
//! GPU presented one, or while the compositor holds all its buffers). The
//! frame then draws on the GPU; after [`MAX_PRESENT_FAILURES`] failures in a
//! row the CPU path is off for the renderer.
//!
//! Composition: a window that composes its content draws each surface from a
//! scene replayed out of the window's scene (`draw_composed` on Linux). Such
//! scenes are not numbered (`SceneDamage::frame` is 0) and always draw on the
//! GPU, and the next numbered scene finds its `since` different from the
//! last scene drawn, so the canvas is drawn whole.
//!
//! CPU time: the raster draws large regions on several threads. Their CPU
//! time is not measured; it is counted as the wall time of the drawing times
//! the threads used, an upper bound, plus the wall time of presenting.
//!
//! See `docs/superpowers/specs/2026-10-08-damage-and-adaptive-cpu-design.md`.

// The web never draws on the CPU: most of this module is unused there.
#![cfg_attr(target_family = "wasm", allow(dead_code, unused_imports))]

#[cfg(not(target_family = "wasm"))]
pub(crate) mod blit;
pub(crate) mod policy;
pub(crate) mod region;
pub(crate) mod stats;
#[cfg(test)]
mod tests;
pub(crate) mod verify;

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use collections::FxHashMap;
use gpui::{AtlasTextureId, Bounds, DevicePixels, Scene};

use crate::WgpuRenderer;
use crate::fast::adaptive::policy::{AtlasDamage, Decision, Frame, GpuReason, Policy, Target};
use crate::fast::adaptive::region::Region;
use crate::fast::adaptive::stats::WindowStats;
use crate::fast::cpu::atlas::AtlasMirror;
use crate::fast::cpu::raster::{self, Canvas, RasterParams};
use crate::fast::cpu::{CpuFrame, CpuPresenter};
use crate::wgpu_renderer::RendererState;

/// Regions smaller than this, in pixels, are drawn on one thread.
pub(crate) const SINGLE_THREAD_PIXELS: i64 = 64 * 1024;

/// The most threads a region is drawn on.
pub(crate) const MAX_THREADS: usize = 8;

/// After this many CPU frames in a row that could not be shown, the CPU path
/// is off for the renderer.
pub(crate) const MAX_PRESENT_FAILURES: u32 = 64;

/// `GPUI_CPU_RENDER`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Off,
    Auto,
    Always,
}

pub(crate) fn mode() -> Mode {
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("GPUI_CPU_RENDER").as_deref() {
        Ok("0") => Mode::Off,
        Ok("always") => Mode::Always,
        _ => Mode::Auto,
    })
}

/// Whether this process may draw frames on the CPU at all.
pub(crate) fn cpu_frames_possible() -> bool {
    !cfg!(target_family = "wasm") && mode() != Mode::Off
}

/// How CPU frames are shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentMode {
    /// With the platform's [`CpuPresenter`], without the GPU.
    Native,
    /// Uploaded to a texture the GPU copies to the swapchain image.
    Blit,
}

impl PresentMode {
    pub(crate) fn name(self) -> &'static str {
        match self {
            PresentMode::Native => "native",
            PresentMode::Blit => "blit",
        }
    }
}

/// `GPUI_CPU_PRESENT`: `native`, `blit`, or `None` when unset (or anything
/// else).
fn present_setting() -> Option<PresentMode> {
    static SETTING: OnceLock<Option<PresentMode>> = OnceLock::new();
    *SETTING.get_or_init(|| match std::env::var("GPUI_CPU_PRESENT").as_deref() {
        Ok("native") => Some(PresentMode::Native),
        Ok("blit") => Some(PresentMode::Blit),
        _ => None,
    })
}

/// How a renderer shows CPU frames when `GPUI_CPU_PRESENT` is unset, or
/// `None` for no CPU frames: with the platform's presenter where it installed
/// one, by blit otherwise.
pub(crate) fn default_present_mode(has_presenter: bool) -> Option<PresentMode> {
    Some(if has_presenter {
        PresentMode::Native
    } else {
        PresentMode::Blit
    })
}

/// How a renderer shows CPU frames, given `GPUI_CPU_PRESENT` (`setting`) and
/// whether the platform installed a presenter; `None` for no CPU frames.
pub(crate) fn present_mode(
    setting: Option<PresentMode>,
    has_presenter: bool,
) -> Option<PresentMode> {
    if !cpu_frames_possible() {
        return None;
    }
    match setting {
        Some(PresentMode::Blit) => Some(PresentMode::Blit),
        Some(PresentMode::Native) if has_presenter => Some(PresentMode::Native),
        Some(PresentMode::Native) => Some(PresentMode::Blit),
        None => default_present_mode(has_presenter),
    }
}

fn threads_for(pixels: i64) -> usize {
    static THREADS: OnceLock<usize> = OnceLock::new();
    if pixels < SINGLE_THREAD_PIXELS {
        return 1;
    }
    *THREADS.get_or_init(|| {
        std::thread::available_parallelism()
            .map_or(1, |threads| threads.get())
            .min(MAX_THREADS)
    })
}

/// The renderer's choice between drawing on the CPU and on the GPU, and what
/// the CPU path keeps between frames.
#[derive(Default)]
pub(crate) struct Adaptive {
    presenter: Option<Box<dyn CpuPresenter>>,
    /// How CPU frames are shown, once resolved on the first frame; `None`
    /// inside for no CPU frames.
    resolved: Option<Option<PresentMode>>,
    /// The CPU path was turned off after [`MAX_PRESENT_FAILURES`].
    disabled: bool,
    /// CPU frames that could not be shown, in a row.
    failures: u32,
    /// The presenter shows a CPU frame: the next GPU frame must tell it
    /// before it is presented. True at first, so that the first GPU frame
    /// tells it too.
    presenter_shows: bool,
    canvas: Option<Canvas>,
    #[cfg(not(target_family = "wasm"))]
    blit: blit::Blit,
    policy: Policy,
    stats: WindowStats,
    /// The frame last handed to the GPU, until it is presented.
    pending: Option<PendingGpu>,
    /// The extents of the sprites of the frame being drawn over atlas
    /// rectangles written since the frame before.
    atlas_region: Region,
    writes: Vec<(AtlasTextureId, Bounds<DevicePixels>)>,
    writes_by_texture: FxHashMap<AtlasTextureId, Vec<Bounds<DevicePixels>>>,
    /// The scene drawn whole, with `GPUI_CPU_VERIFY=1` ([`verify`]).
    verify_canvas: Canvas,
    /// Overrides [`mode`], for tests.
    #[cfg(test)]
    mode: Option<Mode>,
}

struct PendingGpu {
    number: u64,
    atlas_everything: bool,
    start: Instant,
}

/// Where a CPU frame is shown.
pub(crate) trait Output {
    /// Gets ready to show a frame of `target`, before it is drawn.
    fn prepare(&mut self, target: Target) -> Prepared;
    /// Shows `frame`, prepared.
    fn show(&mut self, frame: CpuFrame<'_>) -> anyhow::Result<()>;
}

/// Whether an [`Output`] can show a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Prepared {
    Ready,
    /// Nothing can be shown this frame, by either path (the surface timed
    /// out or is occluded): the frame is not presented.
    NotPresented,
    /// Not this way: the GPU is to draw the frame.
    Gpu,
}

/// The platform's presenter as an [`Output`].
struct Native<'a>(&'a mut dyn CpuPresenter);

impl Output for Native<'_> {
    fn prepare(&mut self, _target: Target) -> Prepared {
        Prepared::Ready
    }

    fn show(&mut self, frame: CpuFrame<'_>) -> anyhow::Result<()> {
        self.0.present(frame)
    }
}

impl Adaptive {
    fn mode(this: &Self) -> Mode {
        #[cfg(test)]
        if let Some(mode) = this.mode {
            return mode;
        }
        let _ = this;
        mode()
    }

    /// Starts over with `presenter`, sampling sprites from `mirror`.
    fn install(this: &mut Self, presenter: Box<dyn CpuPresenter>, mirror: &mut AtlasMirror) {
        AtlasMirror::enable(mirror);
        *this = Adaptive {
            presenter: Some(presenter),
            presenter_shows: true,
            #[cfg(test)]
            mode: this.mode,
            ..Adaptive::default()
        };
    }

    /// Resolves how CPU frames are shown, on the first frame, and turns the
    /// atlas's CPU copy off when there are none.
    fn resolve(this: &mut Self, mirror: &mut AtlasMirror) -> Option<PresentMode> {
        if let Some(resolved) = this.resolved {
            return resolved;
        }
        let resolved = present_mode(present_setting(), this.presenter.is_some());
        this.resolved = Some(resolved);
        if resolved.is_none() {
            AtlasMirror::disable(mirror);
        }
        this.presenter_shows = resolved == Some(PresentMode::Native);
        resolved
    }

    /// [`Adaptive::frame`] shown with the platform's presenter.
    fn frame_native(
        this: &mut Self,
        scene: &Scene,
        mirror: &mut AtlasMirror,
        target: Target,
        params: &RasterParams,
        now: Instant,
    ) -> Option<bool> {
        let mut presenter = this.presenter.take()?;
        let result = Self::frame(
            this,
            scene,
            mirror,
            target,
            params,
            now,
            &mut Native(&mut *presenter),
            PresentMode::Native,
        );
        if this.disabled {
            if this.presenter_shows {
                // The GPU frame drawn now presents over it.
                presenter.gpu_presented();
            }
        } else {
            this.presenter = Some(presenter);
        }
        result
    }

    /// Draws `scene` into `target` on the CPU and shows it with `output`, if
    /// it should be, and returns whether it was presented, or `None` for the
    /// GPU to draw it. `now` is when the frame began.
    #[allow(clippy::too_many_arguments)]
    fn frame(
        this: &mut Self,
        scene: &Scene,
        mirror: &mut AtlasMirror,
        target: Target,
        params: &RasterParams,
        now: Instant,
        output: &mut dyn Output,
        present_mode: PresentMode,
    ) -> Option<bool> {
        let real_start = Instant::now();
        let always = Self::mode(this) == Mode::Always;
        let damage = &scene.damage;

        let everything = AtlasMirror::take_writes(mirror, &mut this.writes);
        this.atlas_region.clear();
        if !everything && this.policy.has_canvas() {
            region::add_written_sprites(
                &mut this.atlas_region,
                scene,
                &this.writes,
                target.bounds(),
                &mut this.writes_by_texture,
            );
        }
        let atlas = if everything {
            AtlasDamage::Everything
        } else {
            AtlasDamage::Rects(this.atlas_region.rects())
        };
        let needs_gpu = (AtlasMirror::has_missing(mirror)
            && region::samples_any(scene, |id| AtlasMirror::is_missing(mirror, id)))
        .then_some(GpuReason::MissingAtlas);
        let decision = this.policy.decide(&Frame {
            now,
            target,
            number: damage.frame,
            since: damage.since,
            damage: &damage.rects,
            atlas,
            needs_gpu,
            always,
        });
        let plan = match decision {
            Decision::Cpu(plan) => match raster::can_draw(scene, plan.region.rects(), params) {
                Ok(()) => plan,
                Err(needs) => {
                    let reason = match needs {
                        raster::NeedsGpu::Surfaces => GpuReason::Surfaces,
                        raster::NeedsGpu::PathSampling => GpuReason::PathSampling,
                    };
                    return Self::to_gpu(this, scene, everything, reason, now);
                }
            },
            Decision::Gpu(reason) => return Self::to_gpu(this, scene, everything, reason, now),
        };
        match output.prepare(target) {
            Prepared::Ready => {}
            Prepared::NotPresented => {
                this.pending = None;
                return Some(false);
            }
            Prepared::Gpu => {
                return Self::to_gpu(this, scene, everything, GpuReason::SurfaceUnavailable, now);
            }
        }

        let canvas = match &mut this.canvas {
            Some(canvas) if canvas.width() == target.width && canvas.height() == target.height => {
                canvas
            }
            slot => {
                debug_assert!(plan.whole, "a new canvas is drawn whole");
                slot.insert(Canvas::new(target.width, target.height))
            }
        };
        let pixels = plan.region.area();
        let threads = threads_for(pixels);
        let drawing = Instant::now();
        raster::draw(
            canvas,
            scene,
            plan.region.rects(),
            &*mirror,
            params,
            threads,
        );
        let drawn = drawing.elapsed();

        let shown = output.show(CpuFrame {
            pixels: canvas.pixels(),
            width: canvas.width(),
            height: canvas.height(),
            damage: plan.region.rects(),
            opaque: target.opaque,
        });
        let took = real_start.elapsed();
        if let Err(error) = shown {
            // The canvas was drawn, not shown: it holds this scene inside the
            // region, and still the last one drawn elsewhere, which the
            // policy's bookkeeping covers (the region holds the stale pixels).
            this.failures += 1;
            if this.failures >= MAX_PRESENT_FAILURES {
                log::error!(
                    "{} CPU frames in a row could not be shown, drawing on the GPU from \
                     now: {error:#}",
                    this.failures
                );
                Self::disable(this, mirror);
            } else {
                log::debug!("a CPU frame could not be shown, drawing it on the GPU: {error:#}");
            }
            return Self::to_gpu(this, scene, everything, GpuReason::PresentFailed, now);
        }
        this.failures = 0;
        if verify::enabled() {
            let threads = threads_for(i64::from(target.width) * i64::from(target.height));
            let mismatch = verify::check(
                canvas,
                &mut this.verify_canvas,
                scene,
                &*mirror,
                params,
                threads,
            );
            if let Some(mismatch) = mismatch {
                log::error!(
                    "CPU frame {} (since {}) differs from its scene drawn whole in {} pixels \
                     within {:?}; damage {:?}, region {:?}",
                    damage.frame,
                    damage.since,
                    mismatch.pixels,
                    mismatch.bounds,
                    damage.rects,
                    plan.region.rects(),
                );
            }
            this.stats.verified(mismatch);
        }
        this.presenter_shows = present_mode == PresentMode::Native;
        // Wall time, plus the other threads' share of the drawing.
        let cpu = took + drawn * (threads as u32 - 1);
        let end = now + took;
        this.policy
            .cpu_drew(damage.frame, target, now, end, cpu, always);
        this.pending = None;
        this.stats.cpu_frame(present_mode, pixels, took);
        this.stats.tick(end);
        Some(true)
    }

    /// Turns the CPU path off for good.
    fn disable(this: &mut Self, mirror: &mut AtlasMirror) {
        this.disabled = true;
        this.canvas = None;
        #[cfg(not(target_family = "wasm"))]
        {
            this.blit = blit::Blit::default();
        }
        this.policy = Policy::default();
        AtlasMirror::disable(mirror);
    }

    fn to_gpu(
        this: &mut Self,
        scene: &Scene,
        atlas_everything: bool,
        reason: GpuReason,
        now: Instant,
    ) -> Option<bool> {
        this.stats.gpu_chosen(reason);
        this.stats.tick(now);
        this.pending = Some(PendingGpu {
            number: scene.damage.frame,
            atlas_everything,
            start: Instant::now(),
        });
        None
    }

    /// Notes that the GPU drew `scene` and is about to present it: called
    /// right before the swapchain image is presented, so a presenter showing
    /// a CPU frame is told first ([`CpuPresenter::gpu_presented`]).
    pub(crate) fn gpu_drew(this: &mut Self, scene: &Scene) {
        if this.disabled || !matches!(this.resolved, Some(Some(_))) {
            return;
        }
        Self::gpu_drew_at(this, scene, Instant::now());
    }

    fn gpu_drew_at(this: &mut Self, scene: &Scene, now: Instant) {
        if std::mem::take(&mut this.presenter_shows)
            && let Some(presenter) = &mut this.presenter
        {
            presenter.gpu_presented();
        }
        let pending = this
            .pending
            .take()
            .filter(|pending| pending.number == scene.damage.frame);
        let atlas = match &pending {
            Some(pending) if !pending.atlas_everything => {
                AtlasDamage::Rects(this.atlas_region.rects())
            }
            _ => AtlasDamage::Everything,
        };
        let damage = &scene.damage;
        if this
            .policy
            .gpu_drew(now, damage.frame, damage.since, &damage.rects, atlas)
        {
            this.canvas = None;
            #[cfg(not(target_family = "wasm"))]
            blit::Blit::release_texture(&mut this.blit);
            if let Some(presenter) = &mut this.presenter {
                presenter.release();
            }
        }
        this.stats
            .gpu_frame(pending.map(|pending| pending.start.elapsed()));
        this.stats.tick(now);
    }
}

/// The surface, by blit, as an [`Output`].
#[cfg(not(target_family = "wasm"))]
struct BlitOutput<'a> {
    surface: &'a wgpu::Surface<'static>,
    config: &'a wgpu::SurfaceConfiguration,
    device: &'a Arc<wgpu::Device>,
    queue: &'a wgpu::Queue,
    blit: &'a mut blit::Blit,
    frame: Option<wgpu::SurfaceTexture>,
}

#[cfg(not(target_family = "wasm"))]
impl Output for BlitOutput<'_> {
    fn prepare(&mut self, target: Target) -> Prepared {
        // As `WgpuRenderer::draw` handles each case; the GPU path then
        // acquires again.
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                drop(frame);
                self.surface.configure(self.device, self.config);
                return Prepared::Gpu;
            }
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(self.device, self.config);
                return Prepared::Gpu;
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Prepared::NotPresented;
            }
            wgpu::CurrentSurfaceTexture::Validation => return Prepared::Gpu,
        };
        if frame.texture.width() != target.width || frame.texture.height() != target.height {
            return Prepared::Gpu;
        }
        self.frame = Some(frame);
        Prepared::Ready
    }

    fn show(&mut self, frame: CpuFrame<'_>) -> anyhow::Result<()> {
        let surface_frame = self
            .frame
            .take()
            .ok_or_else(|| anyhow::anyhow!("no surface image to blit to"))?;
        blit::Blit::upload(
            self.blit,
            self.device,
            self.queue,
            frame.pixels,
            frame.width,
            frame.height,
            frame.damage,
        );
        let view = surface_frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("cpu_frame_blit"),
            });
        blit::Blit::draw(
            self.blit,
            self.device,
            &mut encoder,
            &view,
            surface_frame.texture.format(),
        );
        self.queue.submit(std::iter::once(encoder.finish()));
        surface_frame.present();
        Ok(())
    }
}

impl WgpuRenderer {
    /// Lets this renderer show the frames it draws on the CPU with
    /// `presenter`, without the GPU (see `GPUI_CPU_PRESENT`). Does nothing
    /// with `GPUI_CPU_RENDER=0`, or on the web.
    pub fn set_cpu_presenter(&mut self, presenter: Box<dyn CpuPresenter>) {
        if !cpu_frames_possible() {
            return;
        }
        Adaptive::install(
            &mut self.fast_adaptive,
            presenter,
            &mut self.atlas.cpu_mirror(),
        );
    }
}

/// Draws `scene` on the CPU and presents it, if it should be: returns
/// whether it was presented, or `None` for the GPU to draw it.
pub(crate) fn draw(renderer: &mut WgpuRenderer, scene: &Scene) -> Option<bool> {
    if renderer.fast_adaptive.disabled || matches!(renderer.fast_adaptive.resolved, Some(None)) {
        return None;
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = scene;
        None
    }
    #[cfg(not(target_family = "wasm"))]
    {
        let WgpuRenderer {
            state,
            surface_config,
            atlas,
            fast_adaptive: this,
            ..
        } = renderer;
        let mut mirror = atlas.cpu_mirror();
        let present_mode = Adaptive::resolve(this, &mut mirror)?;
        let RendererState::Ready { surface, core } = state else {
            this.pending = None;
            return None;
        };
        let target = Target {
            width: surface_config.width,
            height: surface_config.height,
            opaque: surface_config.alpha_mode == wgpu::CompositeAlphaMode::Opaque,
        };
        // As `WgpuRenderer::draw` passes them to the GPU.
        let params = RasterParams {
            gamma_ratios: core.rendering_params.gamma_ratios,
            grayscale_enhanced_contrast: core.rendering_params.grayscale_enhanced_contrast,
            subpixel_enhanced_contrast: core.rendering_params.subpixel_enhanced_contrast,
            is_bgr: core.is_bgr,
            premultiplied_alpha: surface_config.alpha_mode
                == wgpu::CompositeAlphaMode::PreMultiplied,
            dual_source_blending: core.dual_source_blending,
            path_sample_count: core.rendering_params.path_sample_count,
            fragment_bits: raster::fragment_bits(core.adapter_info.vendor),
        };
        let now = Instant::now();
        match present_mode {
            PresentMode::Native => {
                Adaptive::frame_native(this, scene, &mut mirror, target, &params, now)
            }
            PresentMode::Blit => {
                let mut blit = std::mem::take(&mut this.blit);
                let mut output = BlitOutput {
                    surface,
                    config: surface_config,
                    device: &core.resources.device,
                    queue: &core.resources.queue,
                    blit: &mut blit,
                    frame: None,
                };
                let result = Adaptive::frame(
                    this,
                    scene,
                    &mut mirror,
                    target,
                    &params,
                    now,
                    &mut output,
                    PresentMode::Blit,
                );
                // An image acquired and not shown is dropped before the GPU
                // path acquires one.
                drop(output);
                if !this.disabled {
                    this.blit = blit;
                }
                result
            }
        }
    }
}
