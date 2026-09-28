//! What frames cost: samples of the window's counters and the process's CPU
//! time, the cost of the frames between two samples, and the status bar that
//! shows it every half second.

use std::time::{Duration, Instant};

use gpui::{Context, IntoElement, Render, Window, div, prelude::*};

use super::theme::theme;

/// The window's counters and the CPU time used, at one moment.
#[derive(Clone, Copy)]
pub struct Sample {
    at: Instant,
    process_cpu: Duration,
    main_cpu: Duration,
    frames: u64,
    build: Duration,
    prepaint: Duration,
    layout: Duration,
    paint: Duration,
    views_built: u64,
    views_reused: u64,
}

impl Sample {
    pub fn take(window: &Window) -> Self {
        let stats = window.layout_stats();
        Self {
            at: Instant::now(),
            process_cpu: process_cpu_time(),
            main_cpu: main_thread_cpu_time(),
            frames: stats.frames,
            build: stats.build_time,
            prepaint: stats.prepaint_time,
            layout: stats.compute_layout_time,
            paint: stats.paint_time,
            views_built: stats.views_built,
            views_reused: stats.views_reused,
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
    pub build_ms: f64,
    pub prepaint_ms: f64,
    pub layout_ms: f64,
    pub paint_ms: f64,
    pub views_built: f64,
    pub views_reused: f64,
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
            build_ms: per_frame(to.build - from.build),
            prepaint_ms: per_frame(to.prepaint - from.prepaint),
            layout_ms: per_frame(to.layout - from.layout),
            paint_ms: per_frame(to.paint - from.paint),
            views_built: per_frame_count(to.views_built - from.views_built),
            views_reused: per_frame_count(to.views_reused - from.views_reused),
        }
    }
}

/// The status bar: what the last half second of frames cost.
pub struct StatusBar {
    last: Option<Sample>,
    cost: Option<Cost>,
    retention: bool,
}

impl StatusBar {
    pub fn new() -> Self {
        Self {
            last: None,
            cost: None,
            retention: true,
        }
    }

    pub fn sample(&mut self, window: &Window) {
        let now = Sample::take(window);
        if let Some(last) = &self.last {
            self.cost = Some(Cost::between(last, &now));
        }
        self.retention = window.view_retention();
        self.last = Some(now);
    }
}

impl Render for StatusBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let bar = div()
            .flex()
            .items_center()
            .flex_shrink_0()
            .h_7()
            .px_4()
            .gap_4()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .text_xs()
            .text_color(theme.muted_foreground);
        let Some(cost) = self.cost else {
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
                .items_center()
                .gap_1()
                .child(label)
                .children(values)
        };
        let separator = || div().w_px().h_3().bg(theme.border);

        bar.child(field(
            "Retained views",
            vec![
                div()
                    .text_color(theme.foreground)
                    .child(if self.retention { "on" } else { "off" })
                    .into_any_element(),
            ],
        ))
        .child(separator())
        .child(field(
            "FPS",
            vec![value(format!("{:.0}", cost.fps), |d| d.w_7()).into_any_element()],
        ))
        .child(field(
            "CPU",
            vec![
                value(format!("{:.0}%", cost.process_cpu_percent), |d| d.w_8()).into_any_element(),
            ],
        ))
        .child(field(
            "Main thread",
            vec![value(format!("{:.0}%", cost.main_cpu_percent), |d| d.w_8()).into_any_element()],
        ))
        .child(separator())
        .child(field(
            "Frame",
            vec![
                value(format!("{:.2}", cost.main_cpu_per_frame_ms), |d| d.w_10())
                    .into_any_element(),
                div().child("ms").into_any_element(),
            ],
        ))
        .child(field(
            "Build",
            vec![value(format!("{:.2}", cost.build_ms), |d| d.w_10()).into_any_element()],
        ))
        .child(field(
            "Prepaint",
            vec![value(format!("{:.2}", cost.prepaint_ms), |d| d.w_10()).into_any_element()],
        ))
        .child(field(
            "Layout",
            vec![value(format!("{:.2}", cost.layout_ms), |d| d.w_10()).into_any_element()],
        ))
        .child(field(
            "Paint",
            vec![value(format!("{:.2}", cost.paint_ms), |d| d.w_10()).into_any_element()],
        ))
        .child(separator())
        .child(field(
            "Views built",
            vec![value(format!("{:.1}", cost.views_built), |d| d.w_8()).into_any_element()],
        ))
        .child(field(
            "reused",
            vec![value(format!("{:.1}", cost.views_reused), |d| d.w_8()).into_any_element()],
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

#[cfg(not(unix))]
pub fn main_thread_cpu_time() -> Duration {
    Duration::ZERO
}

#[cfg(not(unix))]
fn process_cpu_time() -> Duration {
    Duration::ZERO
}
