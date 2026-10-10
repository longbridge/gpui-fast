//! What frames cost: samples of the window's counters and the process's CPU
//! time, the cost of the frames between two samples, and the status bar that
//! shows it every half second.

use std::time::{Duration, Instant};

use gpui::{App, IntoElement, Window, div, prelude::*};

use super::{
    backend::{self, FrameTimes},
    theme::theme,
};

/// The window's counters and the CPU time used, at one moment.
#[derive(Clone, Copy)]
pub struct Sample {
    at: Instant,
    process_cpu: Duration,
    main_cpu: Duration,
    /// The process's resident memory, in bytes, where the platform says.
    memory: Option<u64>,
    /// CPU time the process has used so far, in all and on performance
    /// cores, where the platform says.
    core_cpu: Option<CoreCpu>,
    frames: u64,
    times: Option<FrameTimes>,
}

impl Sample {
    pub fn take(window: &Window) -> Self {
        Self {
            at: Instant::now(),
            process_cpu: process_cpu_time(),
            main_cpu: main_thread_cpu_time(),
            memory: resident_memory(),
            core_cpu: core_cpu_time(),
            frames: backend::frames(window),
            times: backend::frame_times(window),
        }
    }
}

/// What the frames between two samples cost.
#[derive(Default, Clone, Copy)]
pub struct Cost {
    pub fps: f64,
    pub process_cpu_percent: f64,
    pub main_cpu_percent: f64,
    pub main_cpu_per_frame_ms: f64,
    /// The share of the process's CPU time spent on performance cores, where
    /// the platform says. A CPU with efficiency cores takes longer over the
    /// same work on them, which the CPU times above count in.
    pub performance_core_percent: Option<f64>,
    /// The process's resident memory at the later sample, in MiB.
    pub memory_mib: Option<f64>,
    /// What GPUI's own counters say, where it has them: gpui-fast does,
    /// upstream does not.
    pub phases: Option<PhaseCost>,
}

/// Per frame: the time each phase took, the views built and reused, and what
/// scroll layers did.
#[derive(Default, Clone, Copy)]
pub struct PhaseCost {
    pub build_ms: f64,
    pub prepaint_ms: f64,
    pub layout_ms: f64,
    pub paint_ms: f64,
    pub present_ms: f64,
    pub views_built: f64,
    pub views_reused: f64,
    pub layer_frames_composited: f64,
    pub tiles_dirtied: f64,
    pub layer_rebuilds_for_input: f64,
}

impl Cost {
    pub fn between(from: &Sample, to: &Sample) -> Self {
        let seconds = (to.at - from.at).as_secs_f64().max(1e-6);
        let frames = (to.frames - from.frames) as f64;
        let per_frame = |d: Duration| {
            if frames == 0. {
                0.
            } else {
                d.as_secs_f64() * 1e3 / frames
            }
        };
        let per_frame_count = |n: u64| if frames == 0. { 0. } else { n as f64 / frames };
        Self {
            fps: frames / seconds,
            process_cpu_percent: (to.process_cpu - from.process_cpu).as_secs_f64() * 100. / seconds,
            main_cpu_percent: (to.main_cpu - from.main_cpu).as_secs_f64() * 100. / seconds,
            main_cpu_per_frame_ms: per_frame(to.main_cpu - from.main_cpu),
            performance_core_percent: from.core_cpu.zip(to.core_cpu).and_then(|(from, to)| {
                let all = to.all.checked_sub(from.all)?;
                let performance = to.performance.checked_sub(from.performance)?;
                (all > 0).then(|| performance as f64 * 100. / all as f64)
            }),
            memory_mib: to.memory.map(|bytes| bytes as f64 / (1024. * 1024.)),
            phases: from.times.zip(to.times).map(|(from, to)| PhaseCost {
                build_ms: per_frame(to.build - from.build),
                prepaint_ms: per_frame(to.prepaint - from.prepaint),
                layout_ms: per_frame(to.layout - from.layout),
                paint_ms: per_frame(to.paint - from.paint),
                present_ms: per_frame(to.present - from.present),
                views_built: per_frame_count(to.views_built - from.views_built),
                views_reused: per_frame_count(to.views_reused - from.views_reused),
                layer_frames_composited: per_frame_count(
                    to.layer_frames_composited - from.layer_frames_composited,
                ),
                tiles_dirtied: per_frame_count(to.tiles_dirtied - from.tiles_dirtied),
                layer_rebuilds_for_input: per_frame_count(
                    to.layer_rebuilds_for_input - from.layer_rebuilds_for_input,
                ),
            }),
        }
    }
}

/// The status bar: what the last half second of frames cost.
/// What the frames of the last half second cost, sampled by a timer. A model
/// the root view reads to draw its status bar, as an application's status
/// bar reads its state.
pub struct Stats {
    last: Option<Sample>,
    cost: Option<Cost>,
    retention: Option<bool>,
}

impl Stats {
    pub fn new() -> Self {
        Self {
            last: None,
            cost: None,
            retention: None,
        }
    }

    pub fn sample(&mut self, window: &Window) {
        let now = Sample::take(window);
        if let Some(last) = &self.last {
            self.cost = Some(Cost::between(last, &now));
        }
        self.retention = backend::view_retention(window);
        self.last = Some(now);
    }
}

/// The status bar, drawn by the root view from the [`Stats`] it reads.
pub fn status_bar(stats: &Stats, cx: &App) -> impl IntoElement {
    {
        let theme = theme(cx);
        let bar = div()
            .flex()
            .items_center()
            .flex_shrink_0()
            .h_7()
            .px_4()
            .gap_3()
            .overflow_hidden()
            .whitespace_nowrap()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .text_xs()
            .text_color(theme.muted_foreground);
        let Some(cost) = stats.cost else {
            return bar.child("Measuring…");
        };

        // Each value keeps a fixed-width, trailing-aligned lane, so that the
        // numbers stay in place as they change.
        let value = |text: String, lane: fn(gpui::Div) -> gpui::Div| {
            lane(div())
                .flex()
                .justify_end()
                .text_color(theme.foreground)
                .child(text)
        };
        let field = |label: &'static str, values: Vec<gpui::AnyElement>| {
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_1()
                .child(label)
                .children(values)
        };
        let separator = || div().w_px().h_3().bg(theme.border);

        let bar = match stats.retention {
            Some(retention) => bar.child(field(
                "Retained views",
                vec![
                    div()
                        .text_color(theme.foreground)
                        .child(if retention { "on" } else { "off" })
                        .into_any_element(),
                ],
            )),
            None => bar.child(div().text_color(theme.foreground).child(backend::GPUI)),
        };
        let bar = bar
            .child(separator())
            .child(field(
                "FPS",
                vec![value(format!("{:.0}", cost.fps), |d| d.w_7()).into_any_element()],
            ))
            .child(field(
                "CPU",
                vec![
                    value(format!("{:.0}%", cost.process_cpu_percent), |d| d.w_8())
                        .into_any_element(),
                ],
            ))
            .child(field(
                "Main thread",
                vec![
                    value(format!("{:.0}%", cost.main_cpu_percent), |d| d.w_8()).into_any_element(),
                ],
            ))
            .when_some(cost.performance_core_percent, |bar, percent| {
                bar.child(field(
                    "P-cores",
                    vec![value(format!("{percent:.0}%"), |d| d.w_8()).into_any_element()],
                ))
            })
            .when_some(cost.memory_mib, |bar, memory| {
                bar.child(field(
                    "Memory",
                    vec![
                        value(format!("{memory:.0}"), |d| d.w_10()).into_any_element(),
                        div().child("MB").into_any_element(),
                    ],
                ))
            })
            .child(separator())
            .child(field(
                "Frame",
                vec![
                    value(format!("{:.2}", cost.main_cpu_per_frame_ms), |d| d.w_10())
                        .into_any_element(),
                    div().child("ms").into_any_element(),
                ],
            ));
        // Upstream has no counters of its own to show.
        let Some(phases) = cost.phases else {
            return bar;
        };
        bar.child(field(
            "Build",
            vec![value(format!("{:.2}", phases.build_ms), |d| d.w_10()).into_any_element()],
        ))
        .child(field(
            "Prepaint",
            vec![value(format!("{:.2}", phases.prepaint_ms), |d| d.w_10()).into_any_element()],
        ))
        .child(field(
            "Layout",
            vec![value(format!("{:.2}", phases.layout_ms), |d| d.w_10()).into_any_element()],
        ))
        .child(field(
            "Paint",
            vec![value(format!("{:.2}", phases.paint_ms), |d| d.w_10()).into_any_element()],
        ))
        .child(field(
            "Present",
            vec![value(format!("{:.2}", phases.present_ms), |d| d.w_10()).into_any_element()],
        ))
        .child(separator())
        .child(field(
            "Views built",
            vec![value(format!("{:.1}", phases.views_built), |d| d.w_8()).into_any_element()],
        ))
        .child(field(
            "reused",
            vec![value(format!("{:.1}", phases.views_reused), |d| d.w_8()).into_any_element()],
        ))
    }
}

/// CPU time the calling thread has used so far, which has to be the main
/// thread, where gpui builds, lays out and paints.
#[cfg(unix)]
pub fn main_thread_cpu_time() -> Duration {
    clock_time(libc::CLOCK_THREAD_CPUTIME_ID)
}

/// CPU time every thread of the process has used so far.
#[cfg(unix)]
fn process_cpu_time() -> Duration {
    clock_time(libc::CLOCK_PROCESS_CPUTIME_ID)
}

#[cfg(unix)]
fn clock_time(clock: libc::clockid_t) -> Duration {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: `clock_gettime` fills in the whole struct when it returns 0.
    unsafe {
        if libc::clock_gettime(clock, time.as_mut_ptr()) != 0 {
            return Duration::ZERO;
        }
        let time = time.assume_init();
        Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
    }
}

pub use gpui_perf::instructions::main_thread_instructions;

#[cfg(not(unix))]
pub fn main_thread_cpu_time() -> Duration {
    Duration::ZERO
}

#[cfg(not(unix))]
fn process_cpu_time() -> Duration {
    Duration::ZERO
}

/// CPU time the process has used, in the platform's units. Only macOS says
/// which cores it was spent on.
#[derive(Clone, Copy)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct CoreCpu {
    all: u64,
    /// The part of `all` spent on performance cores.
    performance: u64,
}

/// The start of `rusage_info_v6` from `<sys/resource.h>`, up to the fields
/// read here.
#[cfg(target_os = "macos")]
#[repr(C)]
struct RusageInfo {
    uuid: [u8; 16],
    user_time: u64,
    system_time: u64,
    pkg_idle_wkups: u64,
    interrupt_wkups: u64,
    pageins: u64,
    wired_size: u64,
    resident_size: u64,
    phys_footprint: u64,
    /// v0's last two fields, v1 to v5.
    unread: [u64; 28],
    user_ptime: u64,
    system_ptime: u64,
    /// The rest of v6.
    rest: [u64; 18],
}

#[cfg(target_os = "macos")]
fn rusage_info() -> Option<RusageInfo> {
    const RUSAGE_INFO_V6: libc::c_int = 6;
    let mut info = std::mem::MaybeUninit::<RusageInfo>::zeroed();
    // SAFETY: `info` is as large as the `rusage_info_v6` the call fills in.
    unsafe {
        (libc::proc_pid_rusage(libc::getpid(), RUSAGE_INFO_V6, info.as_mut_ptr().cast()) == 0)
            .then(|| info.assume_init())
    }
}

#[cfg(target_os = "macos")]
fn core_cpu_time() -> Option<CoreCpu> {
    let info = rusage_info()?;
    Some(CoreCpu {
        all: info.user_time + info.system_time,
        performance: info.user_ptime + info.system_ptime,
    })
}

#[cfg(not(target_os = "macos"))]
fn core_cpu_time() -> Option<CoreCpu> {
    None
}

/// The process's memory footprint, in bytes: what Activity Monitor shows as
/// its memory.
#[cfg(target_os = "macos")]
fn resident_memory() -> Option<u64> {
    Some(rusage_info()?.phys_footprint)
}

/// The process's resident memory, in bytes: what it holds in RAM, as a system
/// monitor shows it.
#[cfg(target_os = "linux")]
fn resident_memory() -> Option<u64> {
    // The second field of statm is the resident set, in pages.
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: `sysconf` only reads a system constant.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    Some(pages * u64::try_from(page_size).ok()?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn resident_memory() -> Option<u64> {
    None
}
