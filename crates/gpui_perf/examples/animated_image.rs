//! Animated images (GIFs) in a feed: how many frames the window draws per
//! second, and the CPU it spends drawing them.
//!
//! - `one`: one 10 fps GIF, the window otherwise still.
//! - `many`: six GIFs at 5 to 25 fps, the window otherwise still.
//! - `scroll`: the feed scrolled by the wheel every frame, a 10 fps GIF every
//!   eight rows.
//!
//! Prints one line, `scenario=… fps=… process_cpu=… main_cpu=…
//! main_ms_per_frame=… composited=… active=…`, after measuring for
//! `seconds` (5 by default) past a second of warm-up.
//!
//! ```text
//! cargo run -p gpui_perf --example animated_image --release -- one
//! cargo run -p gpui_perf --example animated_image --release -- scroll 10
//! ```

extern crate gpui_fast as gpui;
extern crate gpui_platform_fast as gpui_platform;

#[path = "../../gpui/examples/example_support/fonts.rs"]
mod example_support;

use gpui::{
    Bounds, Context, PlatformInput, RenderImage, ScrollDelta, ScrollEvent, TouchPhase, Window,
    WindowBounds, WindowOptions, div, hsla, img, point, prelude::*, px, size,
};
use gpui_platform::application;
use image::{Delay, Frame, ImageBuffer, Rgba};
use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};

const WARMUP: Duration = Duration::from_secs(1);
const ROWS: usize = 200;
/// Wheel pixels per frame, and frames before the scroll turns back.
const WHEEL_STEP: f32 = 4.;
const WHEEL_TURN_FRAMES: u32 = 150;

#[derive(Clone, Copy, PartialEq)]
enum Scenario {
    One,
    Many,
    Scroll,
}

impl Scenario {
    fn parse(name: &str) -> Self {
        match name {
            "one" => Self::One,
            "many" => Self::Many,
            "scroll" => Self::Scroll,
            _ => panic!("unknown scenario {name:?}: one, many or scroll"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::One => "one",
            Self::Many => "many",
            Self::Scroll => "scroll",
        }
    }

    /// The GIF shown in `row`, if any.
    fn gif_in(self, row: usize, gifs: &[Arc<RenderImage>]) -> Option<Arc<RenderImage>> {
        match self {
            Self::One => (row == 3).then(|| gifs[0].clone()),
            Self::Many => gifs.get(row.wrapping_sub(1)).cloned(),
            Self::Scroll => row.is_multiple_of(8).then(|| gifs[0].clone()),
        }
    }
}

/// A 10-frame GIF, each frame shown for `delay`.
fn gif(delay: Duration) -> Arc<RenderImage> {
    const FRAMES: u32 = 10;
    let delay = Delay::from_saturating_duration(delay);
    Arc::new(RenderImage::new(
        (0..FRAMES)
            .map(|n| {
                let shade = (n * 255 / FRAMES) as u8;
                let buffer = ImageBuffer::from_pixel(64, 64, Rgba([shade, 96, 255 - shade, 255]));
                Frame::from_parts(buffer, 0, 0, delay)
            })
            .collect::<Vec<_>>(),
    ))
}

fn gifs(scenario: Scenario) -> Vec<Arc<RenderImage>> {
    let delays: &[u64] = match scenario {
        Scenario::Many => &[40, 60, 80, 100, 150, 200],
        Scenario::One | Scenario::Scroll => &[100],
    };
    delays
        .iter()
        .map(|ms| gif(Duration::from_millis(*ms)))
        .collect()
}

/// A feed of rows in a scroll container, some holding a GIF.
struct Feed {
    scenario: Scenario,
    gifs: Vec<Arc<RenderImage>>,
}

impl Render for Feed {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(hsla(0., 0., 0.12, 1.))
            .text_color(hsla(0., 0., 0.85, 1.))
            .child(
                div()
                    .id("feed")
                    .size_full()
                    .overflow_y_scroll()
                    .children((0..ROWS).map(|row| {
                        let line = div().p_2().child(format!("message {row}"));
                        match self.scenario.gif_in(row, &self.gifs) {
                            Some(gif) => line.child(img(gif).id(row).size(px(64.))),
                            None => line,
                        }
                    })),
            )
    }
}

/// Turns the wheel over the feed before every frame, down and back up.
fn keep_scrolling(window: &mut Window, frame: Rc<Cell<u32>>) {
    window.on_next_frame(move |window, cx| {
        let n = frame.get();
        frame.set(n + 1);
        let down = (n / WHEEL_TURN_FRAMES).is_multiple_of(2);
        window.dispatch_event(
            PlatformInput::Scroll(ScrollEvent {
                position: point(px(400.), px(300.)),
                delta: ScrollDelta::Pixels(point(
                    px(0.),
                    px(if down { -WHEEL_STEP } else { WHEEL_STEP }),
                )),
                touch_phase: TouchPhase::Moved,
                ..Default::default()
            }),
            cx,
        );
        keep_scrolling(window, frame);
    });
}

fn cpu_time(clock: libc::clockid_t) -> Duration {
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

fn main() {
    let mut args = std::env::args().skip(1);
    let scenario = Scenario::parse(&args.next().unwrap_or_else(|| "one".into()));
    let measured = Duration::from_secs(args.next().and_then(|s| s.parse().ok()).unwrap_or(5));

    application().run(move |cx| {
        if !example_support::load_fonts(cx) {
            return;
        }
        let window = cx
            .open_window(
                WindowOptions {
                    focus: true,
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(800.), px(600.)),
                        cx,
                    ))),
                    inactive_frame_interval: None,
                    ..Default::default()
                },
                |_, cx| {
                    cx.new(|_| Feed {
                        scenario,
                        gifs: gifs(scenario),
                    })
                },
            )
            .unwrap();
        cx.activate(true);
        if scenario == Scenario::Scroll {
            window
                .update(cx, |_, window, _| {
                    keep_scrolling(window, Rc::new(Cell::new(0)))
                })
                .unwrap();
        }
        cx.spawn(async move |cx| {
            cx.background_executor().timer(WARMUP).await;
            window
                .update(cx, |_, window, _| window.reset_layout_stats())
                .unwrap();
            let process_start = cpu_time(libc::CLOCK_PROCESS_CPUTIME_ID);
            let main_start = cpu_time(libc::CLOCK_THREAD_CPUTIME_ID);
            cx.background_executor().timer(measured).await;
            window
                .update(cx, |_, window, cx| {
                    let seconds = measured.as_secs_f64();
                    let stats = window.layout_stats();
                    let frames = stats.frames.max(1) as f64;
                    let process = cpu_time(libc::CLOCK_PROCESS_CPUTIME_ID) - process_start;
                    let main = cpu_time(libc::CLOCK_THREAD_CPUTIME_ID) - main_start;
                    println!(
                        "scenario={} fps={:.1} process_cpu={:.2} main_cpu={:.2} \
                         main_ms_per_frame={:.3} composited={:.2} active={}",
                        scenario.name(),
                        frames / seconds,
                        process.as_secs_f64() / seconds * 100.,
                        main.as_secs_f64() / seconds * 100.,
                        main.as_secs_f64() * 1e3 / frames,
                        stats.layer_frames_composited as f64 / frames,
                        window.is_window_active(),
                    );
                    cx.quit();
                })
                .unwrap();
        })
        .detach();
    });
}
