//! What the adaptive renderer drew: per window, logged every second with
//! `GPUI_RENDER_STATS=1`, and in total for the process, for tests.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::fast::adaptive::PresentMode;
use crate::fast::adaptive::policy::GpuReason;
use crate::fast::adaptive::verify::Mismatch;

const REASONS: usize = GpuReason::ALL.len();

/// How often a window's statistics are logged.
const PERIOD: Duration = Duration::from_secs(1);

/// Whether `GPUI_RENDER_STATS=1`.
fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("GPUI_RENDER_STATS").is_ok_and(|value| value == "1"))
}

const MODES: [PresentMode; 2] = [PresentMode::Native, PresentMode::Blit];

fn mode_index(mode: PresentMode) -> usize {
    match mode {
        PresentMode::Native => 0,
        PresentMode::Blit => 1,
    }
}

static CPU_FRAMES: AtomicU64 = AtomicU64::new(0);
static CPU_FRAMES_BY_MODE: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static GPU_FRAMES: AtomicU64 = AtomicU64::new(0);
static CPU_PIXELS: AtomicU64 = AtomicU64::new(0);
static CPU_NANOS: AtomicU64 = AtomicU64::new(0);
static GPU_REASONS: [AtomicU64; REASONS] = [const { AtomicU64::new(0) }; REASONS];

/// Frames drawn by every adaptive renderer of the process since it started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RenderTotals {
    pub(crate) cpu_frames: u64,
    /// CPU frames shown natively, and by blit.
    pub(crate) cpu_frames_by_mode: [u64; 2],
    pub(crate) gpu_frames: u64,
    pub(crate) cpu_pixels: u64,
    pub(crate) cpu_time: Duration,
    /// GPU frames by [`GpuReason::index`].
    pub(crate) gpu_reasons: [u64; REASONS],
}

/// The totals of the process, for tests.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn totals() -> RenderTotals {
    RenderTotals {
        cpu_frames: CPU_FRAMES.load(Ordering::Relaxed),
        cpu_frames_by_mode: std::array::from_fn(|ix| {
            CPU_FRAMES_BY_MODE[ix].load(Ordering::Relaxed)
        }),
        gpu_frames: GPU_FRAMES.load(Ordering::Relaxed),
        cpu_pixels: CPU_PIXELS.load(Ordering::Relaxed),
        cpu_time: Duration::from_nanos(CPU_NANOS.load(Ordering::Relaxed)),
        gpu_reasons: std::array::from_fn(|ix| GPU_REASONS[ix].load(Ordering::Relaxed)),
    }
}

/// One window's frames since its statistics were last logged.
#[derive(Debug, Default)]
pub(crate) struct WindowStats {
    period_start: Option<Instant>,
    /// CPU frames by presentation mode (`mode_index`).
    cpu: [CpuStats; 2],
    gpu_frames: u64,
    /// GPU frames whose CPU-side time was measured, and that time.
    gpu_timed: u64,
    gpu_time: Duration,
    gpu_reasons: [u64; REASONS],
    /// CPU frames checked with `GPUI_CPU_VERIFY=1`, those that differed from
    /// their scene drawn whole, and their differing pixels.
    verified: u64,
    mismatched: u64,
    mismatched_pixels: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct CpuStats {
    frames: u64,
    pixels: u64,
    time: Duration,
    max: Duration,
}

impl WindowStats {
    /// Notes a frame drawn on the CPU, of `pixels`, shown with `mode`, that
    /// took `took`.
    pub(crate) fn cpu_frame(&mut self, mode: PresentMode, pixels: i64, took: Duration) {
        let pixels = pixels.max(0) as u64;
        CPU_FRAMES.fetch_add(1, Ordering::Relaxed);
        CPU_FRAMES_BY_MODE[mode_index(mode)].fetch_add(1, Ordering::Relaxed);
        CPU_PIXELS.fetch_add(pixels, Ordering::Relaxed);
        CPU_NANOS.fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
        let cpu = &mut self.cpu[mode_index(mode)];
        cpu.frames += 1;
        cpu.pixels += pixels;
        cpu.time += took;
        cpu.max = cpu.max.max(took);
    }

    /// Notes a CPU frame checked against its scene drawn whole.
    pub(crate) fn verified(&mut self, mismatch: Option<Mismatch>) {
        self.verified += 1;
        if let Some(mismatch) = mismatch {
            self.mismatched += 1;
            self.mismatched_pixels += mismatch.pixels;
        }
    }

    fn cpu_frames(&self) -> u64 {
        self.cpu.iter().map(|cpu| cpu.frames).sum()
    }

    /// Notes that a frame goes to the GPU for `reason`.
    pub(crate) fn gpu_chosen(&mut self, reason: GpuReason) {
        GPU_REASONS[reason.index()].fetch_add(1, Ordering::Relaxed);
        self.gpu_reasons[reason.index()] += 1;
    }

    /// Notes a frame the GPU drew, whose CPU-side time in the renderer was
    /// `took`, when known.
    pub(crate) fn gpu_frame(&mut self, took: Option<Duration>) {
        GPU_FRAMES.fetch_add(1, Ordering::Relaxed);
        self.gpu_frames += 1;
        if let Some(took) = took {
            self.gpu_timed += 1;
            self.gpu_time += took;
        }
    }

    /// Logs the period's statistics once it lasted a second, when
    /// `GPUI_RENDER_STATS=1`, and starts another.
    pub(crate) fn tick(&mut self, now: Instant) {
        if !enabled() {
            return;
        }
        let start = *self.period_start.get_or_insert(now);
        let elapsed = now.saturating_duration_since(start);
        if elapsed < PERIOD {
            return;
        }
        if self.cpu_frames() + self.gpu_frames > 0 {
            eprintln!("{}", self.summary(elapsed));
        }
        *self = WindowStats {
            period_start: Some(now),
            ..WindowStats::default()
        };
    }

    /// One line of `key=value` pairs, after `gpui render stats:`, for
    /// scripts to read: the period's length, frames on each path, the CPU
    /// frames' pixels and times by presentation mode, the GPU frames'
    /// CPU-side time, and why frames went to the GPU (`why_<reason>`).
    fn summary(&self, elapsed: Duration) -> String {
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        let mean = |total: Duration, n: u64| if n == 0 { 0.0 } else { ms(total) / n as f64 };
        let cpu_time: Duration = self.cpu.iter().map(|cpu| cpu.time).sum();
        let mut line = format!(
            "gpui render stats: secs={:.2} cpu_frames={} gpu_frames={} cpu_px={} \
             cpu_ms_mean={:.3} gpu_ms_mean={:.3}",
            elapsed.as_secs_f64(),
            self.cpu_frames(),
            self.gpu_frames,
            self.cpu.iter().map(|cpu| cpu.pixels).sum::<u64>(),
            mean(cpu_time, self.cpu_frames()),
            mean(self.gpu_time, self.gpu_timed),
        );
        for mode in MODES {
            let stats = &self.cpu[mode_index(mode)];
            if stats.frames > 0 {
                line.push_str(&format!(
                    " {name}_n={} {name}_px={} {name}_ms_mean={:.3} {name}_ms_max={:.3}",
                    stats.frames,
                    stats.pixels,
                    mean(stats.time, stats.frames),
                    ms(stats.max),
                    name = mode.name(),
                ));
            }
        }
        if self.verified > 0 {
            line.push_str(&format!(
                " verify_n={} verify_bad={} verify_bad_px={}",
                self.verified, self.mismatched, self.mismatched_pixels
            ));
        }
        for reason in GpuReason::ALL {
            let count = self.gpu_reasons[reason.index()];
            if count > 0 {
                line.push_str(&format!(" why_{}={count}", reason.name()));
            }
        }
        line
    }

    #[cfg(test)]
    pub(crate) fn counts(&self) -> (u64, u64, [u64; REASONS]) {
        (self.cpu_frames(), self.gpu_frames, self.gpu_reasons)
    }

    #[cfg(test)]
    pub(crate) fn summary_for_test(&self) -> String {
        self.summary(Duration::from_secs(1))
    }
}
