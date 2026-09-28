//! A dashboard of panels, each its own view, a few of which change every
//! frame, drawn into a real window and presented by the GPU.
//!
//! This is the shape most applications have: a window made of views, most of
//! them still while one or two are notified. Retained views draw the ones
//! nobody notified again from the last frame; run it with
//! `GPUI_VIEW_RETENTION=0` to draw every view from scratch, as upstream GPUI
//! does, and compare.
//!
//! The arguments are the number of panels, the labels in each, and how many
//! panels change every frame.
//!
//! ```text
//! cargo run -p gpui_perf --example views_frames --release -- 60 64 2
//! GPUI_VIEW_RETENTION=0 cargo run -p gpui_perf --example views_frames --release -- 60 64 2
//! ```

extern crate gpui_fast as gpui;
extern crate gpui_platform_fast as gpui_platform;

#[path = "../../gpui/examples/example_support/fonts.rs"]
mod example_support;

use gpui::{
    Bounds, Context, Entity, Render, SharedString, Window, WindowBounds, WindowOptions, div, hsla,
    prelude::*, px, size,
};
use gpui_platform::application;
use std::time::{Duration, Instant};

/// Frames drawn before the clock starts.
const WARMUP_FRAMES: usize = 60;

/// Frames measured after that.
const MEASURED_FRAMES: usize = 300;

/// Distinct labels a cell can show.
const LABELS: usize = 97;

struct Dashboard {
    panels: Vec<Entity<Panel>>,
    changing: usize,
    labels_per_panel: usize,
    tick: usize,

    frames: usize,
    measuring_since: Option<Instant>,
    cpu_since: Option<Duration>,
    last_main_cpu: Option<Duration>,
    frame_main_cpu: Vec<Duration>,
}

struct Panel {
    index: usize,
    labels: Vec<SharedString>,
    count: usize,
    tick: usize,
}

impl Render for Panel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let tick = self.tick;
        div()
            .flex()
            .flex_col()
            .w(px(220.))
            .p_1()
            .gap_1()
            .rounded_sm()
            .bg(hsla(0.6, 0.2, 0.18, 1.))
            .child(format!("panel {} · {}", self.index, tick))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .children((0..self.count).map(|cell| {
                        div()
                            .w(px(26.))
                            .h(px(14.))
                            .hover(|style| style.bg(hsla(0.1, 0.6, 0.4, 1.)))
                            .child(self.labels[(cell + tick) % LABELS].clone())
                    })),
            )
    }
}

impl Render for Dashboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Instant::now();
        let main_cpu = main_thread_cpu_time();
        if let (Some(main), Some(last)) = (main_cpu, self.last_main_cpu)
            && self.measuring_since.is_some()
        {
            self.frame_main_cpu.push(main - last);
        }
        self.last_main_cpu = main_cpu;

        self.frames += 1;
        if self.frames == WARMUP_FRAMES {
            window.reset_layout_stats();
            self.measuring_since = Some(now);
            self.cpu_since = main_cpu;
            self.frame_main_cpu.clear();
        } else if self.frames == WARMUP_FRAMES + MEASURED_FRAMES {
            self.report(window);
            cx.quit();
        }

        // Before the next frame, change a few panels and notify only them,
        // as a timer or a message from elsewhere would.
        cx.on_next_frame(window, |this, _, cx| {
            this.tick += 1;
            let count = this.panels.len();
            for n in 0..this.changing {
                let panel = &this.panels[(this.tick * 7 + n * 13) % count];
                panel.update(cx, |panel, cx| {
                    panel.tick += 1;
                    cx.notify();
                });
            }
            cx.notify();
        });

        div()
            .size_full()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_1()
            .bg(hsla(0., 0., 0.12, 1.))
            .text_color(hsla(0., 0., 0.85, 1.))
            .text_xs()
            .children(self.panels.iter().cloned())
    }
}

impl Dashboard {
    fn report(&self, window: &Window) {
        let frames = MEASURED_FRAMES as f64;
        let main_cpu = match (main_thread_cpu_time(), self.cpu_since) {
            (Some(now), Some(since)) => (now - since).as_secs_f64() * 1e3 / frames,
            _ => f64::NAN,
        };
        let mut frame_cpu = self.frame_main_cpu.clone();
        frame_cpu.sort();
        let percentile = |p: f64| {
            frame_cpu
                .get(((frame_cpu.len() as f64 - 1.) * p).round() as usize)
                .map_or(0., |d| d.as_secs_f64() * 1e3)
        };
        let stats = window.layout_stats();
        let counted = stats.frames.max(1) as f64;
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        println!(
            "\n  {} panels of {} labels, {} changing per frame, retention {}, over {} frames\n    \
             main cpu          {main_cpu:>8.2} ms/frame  (per frame p50 {:.2}, p95 {:.2} ms)\n    \
             build             {:>8.2} ms/frame\n    \
             prepaint          {:>8.2} ms/frame\n    \
             paint             {:>8.2} ms/frame",
            self.panels.len(),
            self.labels_per_panel,
            self.changing,
            if window.view_retention() { "on" } else { "off" },
            MEASURED_FRAMES,
            percentile(0.5),
            percentile(0.95),
            ms(stats.build_time) / counted,
            ms(stats.prepaint_time) / counted,
            ms(stats.paint_time) / counted,
        );
    }
}

/// CPU time the calling thread has used so far, which has to be the main
/// thread, where gpui builds, lays out and paints.
#[cfg(unix)]
fn main_thread_cpu_time() -> Option<Duration> {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: `clock_gettime` fills in the whole struct when it returns 0.
    let time = unsafe {
        if libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, time.as_mut_ptr()) != 0 {
            return None;
        }
        time.assume_init()
    };
    Some(Duration::new(time.tv_sec as u64, time.tv_nsec as u32))
}

#[cfg(not(unix))]
fn main_thread_cpu_time() -> Option<Duration> {
    None
}

fn run_example() {
    let mut args = std::env::args().skip(1);
    let panels: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(60);
    let labels_per_panel: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(64);
    let changing: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(2);

    application().run(move |cx| {
        if !example_support::load_fonts(cx) {
            return;
        }
        cx.open_window(
            WindowOptions {
                focus: true,
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1400.), px(900.)),
                    cx,
                ))),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|cx| {
                    let labels: Vec<SharedString> = (0..LABELS)
                        .map(|n| SharedString::from(format!("{n:02}")))
                        .collect();
                    Dashboard {
                        panels: (0..panels)
                            .map(|index| {
                                let labels = labels.clone();
                                cx.new(|_| Panel {
                                    index,
                                    labels,
                                    count: labels_per_panel,
                                    tick: index,
                                })
                            })
                            .collect(),
                        changing,
                        labels_per_panel,
                        tick: 0,
                        frames: 0,
                        measuring_since: None,
                        cpu_since: None,
                        last_main_cpu: None,
                        frame_main_cpu: Vec::new(),
                    }
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    run_example();
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    gpui_platform::web_init();
    run_example();
}
