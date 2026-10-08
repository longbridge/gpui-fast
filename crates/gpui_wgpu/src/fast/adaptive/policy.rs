//! The choice between the CPU and the GPU for each frame, without the
//! renderer: what the frame changes, what the CPU's frame in memory (the
//! canvas) still shows, and how much the CPU's frames of the current burst
//! cost. Times are given, never read, so tests drive it with a fake clock.
//!
//! The canvas, when valid, shows the scene the renderer last presented
//! (`last_drawn`), except inside `stale`: the damage of the frames the GPU
//! drew since the CPU last drew the canvas.
//!
//! The constants below are first guesses, to be tuned with measurements.

use std::time::{Duration, Instant};

use gpui::{Bounds, DevicePixels};

use crate::fast::adaptive::region::{self, Region};

/// Frames that begin within this long of the previous frame ending are in a
/// burst with it (scrolling, an animation, a drag). A pause of this long
/// ends the burst.
pub(crate) const BURST_GAP: Duration = Duration::from_millis(50);

/// During a burst, a frame whose damage covers more than this fraction of
/// the window (one over this) draws on the GPU: the next frames will likely
/// change as much.
pub(crate) const BURST_CHANGE_DIVISOR: i64 = 16;

/// During a burst, a frame that would draw the canvas whole draws on the GPU,
/// unless the burst's frames have each changed at most a sixteenth of the
/// window for this long: then the CPU catches the canvas up once, and the
/// frames after draw only what they change. An animation that runs for long
/// (a spinner) so leaves the GPU.
pub(crate) const CATCH_UP_AFTER: Duration = Duration::from_millis(250);

/// A frame whose CPU region is larger than this, in pixels, draws on the GPU.
pub(crate) const MAX_CPU_PIXELS: i64 = 8 << 20;

/// Once the CPU frames of a burst cost more than this fraction of the time
/// the burst lasted, counted from its first CPU frame and only once it lasted
/// [`CPU_LOAD_AFTER`], the rest of the burst draws on the GPU.
pub(crate) const MAX_CPU_LOAD: f64 = 0.25;

/// How long a burst must last before its CPU load is judged.
pub(crate) const CPU_LOAD_AFTER: Duration = Duration::from_millis(250);

/// After this long of GPU frames only, the canvas is released; the next CPU
/// frame draws it whole.
pub(crate) const CANVAS_RELEASE_AFTER: Duration = Duration::from_secs(1);

/// Why a frame was drawn on the GPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GpuReason {
    /// The scene has primitives only the GPU draws (surfaces).
    Surfaces,
    /// The scene was not numbered by its window: a composed window's scene,
    /// replayed for one of its surfaces.
    Composition,
    /// The scene samples an atlas texture the CPU has no copy of.
    MissingAtlas,
    /// The CPU frames of this burst cost too much.
    CpuHeavy,
    /// The frame changes more than a sixteenth of the window in a burst.
    LargeChange,
    /// The canvas would be drawn whole in a burst.
    WholeInBurst,
    /// The CPU region is larger than [`MAX_CPU_PIXELS`].
    TooLarge,
    /// The renderer's first frame: drawn on the GPU whatever it changes, so
    /// that the window shows a frame of the GPU before any of the CPU's (a
    /// Wayland window surface has no buffer until then).
    FirstFrame,
    /// Presenting the CPU frame failed (the frame was drawn, not shown).
    PresentFailed,
    /// The surface had no image to blit the CPU frame to.
    SurfaceUnavailable,
    /// The scene has paths the GPU rasterizes with a sample count the CPU
    /// does not reproduce.
    PathSampling,
}

impl GpuReason {
    pub(crate) const ALL: [GpuReason; 11] = [
        GpuReason::Surfaces,
        GpuReason::Composition,
        GpuReason::MissingAtlas,
        GpuReason::CpuHeavy,
        GpuReason::LargeChange,
        GpuReason::WholeInBurst,
        GpuReason::TooLarge,
        GpuReason::FirstFrame,
        GpuReason::PresentFailed,
        GpuReason::SurfaceUnavailable,
        GpuReason::PathSampling,
    ];

    pub(crate) fn index(self) -> usize {
        self as usize
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            GpuReason::Surfaces => "surfaces",
            GpuReason::Composition => "composition",
            GpuReason::MissingAtlas => "missing_atlas",
            GpuReason::CpuHeavy => "cpu_heavy",
            GpuReason::LargeChange => "large_change",
            GpuReason::WholeInBurst => "whole_in_burst",
            GpuReason::TooLarge => "too_large",
            GpuReason::FirstFrame => "first_frame",
            GpuReason::PresentFailed => "present_failed",
            GpuReason::SurfaceUnavailable => "surface_unavailable",
            GpuReason::PathSampling => "path_sampling",
        }
    }
}

/// What changed in the atlas since the last frame, as the scene samples it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum AtlasDamage<'a> {
    /// The extents of the scene's sprites over written rectangles.
    Rects(&'a [Bounds<DevicePixels>]),
    /// The atlas was cleared or its log overflowed: any sprite may differ.
    Everything,
}

/// The target a frame is drawn into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) opaque: bool,
}

impl Target {
    pub(crate) fn area(&self) -> i64 {
        self.width as i64 * self.height as i64
    }

    pub(crate) fn bounds(&self) -> Bounds<DevicePixels> {
        region::whole(self.width, self.height)
    }
}

/// A frame to decide about.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Frame<'a> {
    pub(crate) now: Instant,
    pub(crate) target: Target,
    /// The scene's number (`SceneDamage::frame`), 0 when not numbered.
    pub(crate) number: u64,
    /// `SceneDamage::since`.
    pub(crate) since: u64,
    /// `SceneDamage::rects`.
    pub(crate) damage: &'a [Bounds<DevicePixels>],
    pub(crate) atlas: AtlasDamage<'a>,
    /// A reason the scene needs the GPU whatever it changes.
    pub(crate) needs_gpu: Option<GpuReason>,
    /// `GPUI_CPU_RENDER=always`: no size, burst or cost limits.
    pub(crate) always: bool,
}

/// What to draw on the CPU.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CpuPlan {
    /// Where to draw: the canvas outside it is up to date.
    pub(crate) region: Region,
    /// Whether the canvas is drawn whole (it was missing, resized, or not
    /// comparable with this scene).
    pub(crate) whole: bool,
    /// How many pixels the frame changes on screen (its damage and its
    /// sprites over written atlas tiles), stale pixels excluded.
    pub(crate) changed: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Decision {
    Cpu(CpuPlan),
    Gpu(GpuReason),
}

/// The CPU time of the CPU frames of a burst.
#[derive(Clone, Copy, Debug, Default)]
struct CpuLoad {
    /// When the first CPU frame of the burst began and the last one ended.
    start: Option<Instant>,
    end: Option<Instant>,
    busy: Duration,
}

impl CpuLoad {
    /// Notes a CPU frame from `start` to `end` that cost `cpu`, and returns
    /// whether the burst's CPU frames cost too much (see [`MAX_CPU_LOAD`]).
    fn add(&mut self, start: Instant, end: Instant, cpu: Duration) -> bool {
        if self
            .end
            .is_none_or(|last| start.saturating_duration_since(last) >= BURST_GAP)
        {
            *self = CpuLoad {
                start: Some(start),
                end: None,
                busy: Duration::ZERO,
            };
        }
        self.end = Some(end);
        self.busy += cpu;
        let lasted = end.saturating_duration_since(self.start.unwrap_or(start));
        lasted >= CPU_LOAD_AFTER && self.busy.as_secs_f64() > MAX_CPU_LOAD * lasted.as_secs_f64()
    }
}

/// The per-window state of the choice; see the module documentation.
#[derive(Debug, Default)]
pub(crate) struct Policy {
    /// The number of the scene last presented, on either path; 0 for none or
    /// one not numbered.
    last_drawn: u64,
    /// The canvas's size and alpha mode, when it shows `last_drawn` outside
    /// `stale`.
    canvas: Option<Target>,
    stale: Region,
    last_frame_end: Option<Instant>,
    load: CpuLoad,
    cpu_heavy: bool,
    /// When the first GPU frame since the last CPU frame was drawn.
    gpu_since: Option<Instant>,
    /// The canvas was released since the last CPU frame.
    released: bool,
    /// The GPU presented a frame of this renderer.
    gpu_presented_any: bool,
    /// When the frames of the current burst began to change at most a
    /// sixteenth of the window each (see [`CATCH_UP_AFTER`]).
    small_since: Option<Instant>,
}

impl Policy {
    /// Decides how to draw `frame`. Nothing is drawn yet: the renderer calls
    /// [`Policy::cpu_drew`] or [`Policy::gpu_drew`] once it presented it.
    pub(crate) fn decide(&mut self, frame: &Frame) -> Decision {
        let pause = self
            .last_frame_end
            .is_none_or(|end| frame.now.saturating_duration_since(end) >= BURST_GAP);
        if pause {
            self.cpu_heavy = false;
            self.load = CpuLoad::default();
            self.small_since = None;
        }
        let burst = !pause;

        if let Some(reason) = frame.needs_gpu {
            return Decision::Gpu(reason);
        }
        if frame.number == 0 {
            return Decision::Gpu(GpuReason::Composition);
        }
        if !self.gpu_presented_any {
            return Decision::Gpu(GpuReason::FirstFrame);
        }
        if self.cpu_heavy && !frame.always {
            return Decision::Gpu(GpuReason::CpuHeavy);
        }

        let target = frame.target;
        let clip = target.bounds();
        let window = target.area();
        let comparable = frame.since != 0 && frame.since == self.last_drawn;
        let redrawn = frame.number == self.last_drawn;
        let mut region = Region::default();
        // What the frame changes on screen: known when its damage is
        // relative to the scene presented last, whatever the canvas holds.
        let known = (comparable || redrawn) && !matches!(frame.atlas, AtlasDamage::Everything);
        let changed = if known {
            if comparable {
                region.add_all(frame.damage, clip);
            }
            if let AtlasDamage::Rects(rects) = frame.atlas {
                region.add_all(rects, clip);
            }
            region.area().min(window)
        } else {
            window
        };
        let mut whole = !known || self.canvas != Some(target);
        if !whole {
            region.add_all(self.stale.rects(), clip);
            // As damage does: past half the window, draw it whole.
            whole = region.area() * 2 > window;
        }
        if whole {
            region.set(clip);
        }

        let small = changed * BURST_CHANGE_DIVISOR <= window;
        if !small {
            self.small_since = None;
        } else if burst {
            self.small_since.get_or_insert(frame.now);
        }
        let caught_up = self
            .small_since
            .is_some_and(|since| frame.now.saturating_duration_since(since) >= CATCH_UP_AFTER);

        if !frame.always {
            if burst && !small {
                return Decision::Gpu(GpuReason::LargeChange);
            }
            if burst && whole && !caught_up {
                return Decision::Gpu(GpuReason::WholeInBurst);
            }
            if region.area() > MAX_CPU_PIXELS {
                return Decision::Gpu(GpuReason::TooLarge);
            }
        }
        Decision::Cpu(CpuPlan {
            region,
            whole,
            changed,
        })
    }

    /// Notes that the CPU drew scene `number` into the canvas of `target`
    /// and presented it, from `start` to `end`, costing `cpu` of CPU time.
    pub(crate) fn cpu_drew(
        &mut self,
        number: u64,
        target: Target,
        start: Instant,
        end: Instant,
        cpu: Duration,
        always: bool,
    ) {
        self.last_drawn = number;
        self.canvas = Some(target);
        self.stale.clear();
        self.gpu_since = None;
        self.released = false;
        self.last_frame_end = Some(end);
        if self.load.add(start, end, cpu) && !always {
            self.cpu_heavy = true;
        }
    }

    /// Notes that the GPU drew and presented scene `number`, whose damage
    /// since scene `since` is `damage`, at `now`. Returns whether the canvas
    /// is to be released, once after [`CANVAS_RELEASE_AFTER`] of GPU frames
    /// only.
    pub(crate) fn gpu_drew(
        &mut self,
        now: Instant,
        number: u64,
        since: u64,
        damage: &[Bounds<DevicePixels>],
        atlas: AtlasDamage,
    ) -> bool {
        self.last_frame_end = Some(now);
        self.gpu_presented_any = true;
        if let Some(target) = self.canvas {
            let clip = target.bounds();
            let comparable = since != 0 && since == self.last_drawn;
            let redrawn = number != 0 && number == self.last_drawn;
            match atlas {
                AtlasDamage::Everything => self.canvas = None,
                _ if number == 0 || !(comparable || redrawn) => self.canvas = None,
                AtlasDamage::Rects(rects) => {
                    if comparable {
                        self.stale.add_all(damage, clip);
                    }
                    self.stale.add_all(rects, clip);
                }
            }
        }
        if self.canvas.is_none() {
            self.stale.clear();
        }
        self.last_drawn = number;

        let since = *self.gpu_since.get_or_insert(now);
        if !self.released && now.saturating_duration_since(since) >= CANVAS_RELEASE_AFTER {
            self.release_canvas();
            self.released = true;
            return true;
        }
        false
    }

    /// Forgets the canvas, as when it is released.
    pub(crate) fn release_canvas(&mut self) {
        self.canvas = None;
        self.stale.clear();
    }

    pub(crate) fn has_canvas(&self) -> bool {
        self.canvas.is_some()
    }

    #[cfg(test)]
    pub(crate) fn stale(&self) -> &Region {
        &self.stale
    }
}
