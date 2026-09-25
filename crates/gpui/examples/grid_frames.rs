//! A grid of small labels, all of which change every frame, drawn into a real
//! window and presented by the GPU.
//!
//! This exists to be run against two builds of gpui and compared. Everything
//! that decides what is drawn — the grid, the labels, how many frames are
//! warmed up and how many are measured — is fixed here so that the only
//! difference between two runs is the gpui underneath it.
//!
//! It also says where the frame went, through [`Window::layout_stats`]. A build
//! old enough to lack those counters can still be compared on the wall clock
//! and on main-thread CPU time, which is what the two have in common. On a
//! display fast enough that a frame fits its refresh, the wall clock only
//! reports the refresh rate, and CPU time is the one to compare.
//!
//! The third argument is the share of cells that change from one frame to the
//! next, which is the axis worth sweeping: reuse can only save the work of
//! whatever stood still.
//!
//! ```text
//! cargo run -p gpui --example grid_frames --release -- 50 50 25
//! ```

#[path = "example_support/fonts.rs"]
mod example_support;

use gpui::{
    Bounds, Context, Render, SharedString, Window, WindowBounds, WindowOptions, div, hsla,
    prelude::*, px, size,
};
use gpui_platform::application;
use std::time::{Duration, Instant};

/// Frames drawn before the clock starts, so that atlas population, pipeline
/// compilation and the window settling are not counted as steady state.
const WARMUP_FRAMES: usize = 60;

/// Frames measured after that.
const MEASURED_FRAMES: usize = 300;

/// Distinct labels a cell can show. Cycling through a prepared set keeps the
/// measurement on gpui rather than on `format!`, while still giving every cell
/// different text on every frame.
const LABELS: usize = 97;

struct Grid {
    rows: usize,
    columns: usize,
    /// How much of the grid is alive: the share of cells whose text differs
    /// from one frame to the next. The rest stand still.
    ///
    /// This is the axis the layout engine's node retention is judged on. It
    /// can only save the work of rebuilding what did not change, so a grid
    /// where everything changes is the case it helps least.
    changing_percent: usize,
    labels: Vec<SharedString>,
    tick: usize,

    frames: usize,
    measuring_since: Option<Instant>,
    last_frame_at: Option<Instant>,
    slowest: Duration,
    /// Main-thread CPU time when measuring started, at the last frame, and
    /// what each measured frame took of it.
    cpu_since: Option<Duration>,
    last_main_cpu: Option<Duration>,
    frame_main_cpu: Vec<Duration>,
}

impl Render for Grid {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Asking for the next frame from inside this one is what keeps the
        // window redrawing without an input to provoke it.
        window.request_animation_frame();

        let now = Instant::now();
        if let Some(last) = self.last_frame_at {
            if self.measuring_since.is_some() {
                self.slowest = self.slowest.max(now - last);
            }
        }
        self.last_frame_at = Some(now);

        let main_cpu = main_thread_cpu_time();
        if let (Some(main), Some(last)) = (main_cpu, self.last_main_cpu)
            && self.measuring_since.is_some()
        {
            self.frame_main_cpu.push(main - last);
        }
        self.last_main_cpu = main_cpu;

        self.tick += 1;
        self.frames += 1;
        if self.frames == WARMUP_FRAMES {
            window.reset_layout_stats();
            self.measuring_since = Some(now);
            self.slowest = Duration::ZERO;
            self.cpu_since = main_cpu;
            self.frame_main_cpu.clear();
        } else if self.frames == WARMUP_FRAMES + MEASURED_FRAMES {
            self.report(window);
            cx.quit();
        }

        let tick = self.tick;
        let columns = self.columns;
        let changing_percent = self.changing_percent;
        let labels = self.labels.clone();
        div()
            .flex()
            .flex_col()
            .bg(hsla(0., 0., 0.12, 1.))
            .text_color(hsla(0., 0., 0.85, 1.))
            .text_xs()
            .children((0..self.rows).map(move |row| {
                let labels = labels.clone();
                div()
                    .flex()
                    .flex_row()
                    .h(px(15.))
                    .children((0..columns).map(move |column| {
                        // Every cell differs from its neighbours; only the
                        // live share of them differs from last frame too.
                        let cell = row * columns + column;
                        let index = if cell % 100 < changing_percent {
                            (tick + cell) % LABELS
                        } else {
                            cell % LABELS
                        };
                        div().w(px(25.)).h(px(15.)).child(labels[index].clone())
                    }))
            }))
    }
}

impl Grid {
    fn new(rows: usize, columns: usize, changing_percent: usize) -> Self {
        Grid {
            rows,
            columns,
            changing_percent,
            labels: (0..LABELS)
                .map(|n| SharedString::from(format!("{n:02}")))
                .collect(),
            tick: 0,
            frames: 0,
            measuring_since: None,
            last_frame_at: None,
            slowest: Duration::ZERO,
            cpu_since: None,
            last_main_cpu: None,
            frame_main_cpu: Vec::new(),
        }
    }

    fn report(&self, window: &Window) {
        let wall = self
            .measuring_since
            .map(|at| at.elapsed())
            .unwrap_or_default();
        let frames = MEASURED_FRAMES as f64;
        let main_cpu = match (main_thread_cpu_time(), self.cpu_since) {
            (Some(now), Some(since)) => {
                format!(
                    "{:>8.2} ms/frame",
                    (now - since).as_secs_f64() * 1e3 / frames
                )
            }
            _ => "     n/a".into(),
        };
        let mut frame_cpu = self.frame_main_cpu.clone();
        frame_cpu.sort();
        let percentile = |p: f64| {
            frame_cpu
                .get(((frame_cpu.len() as f64 - 1.) * p).round() as usize)
                .map_or(0., |d| d.as_secs_f64() * 1e3)
        };
        let (p50, p95) = (percentile(0.5), percentile(0.95));
        println!(
            "\n  grid {}x{}, {}% of cells changing, over {} frames\n    \
             main cpu          {main_cpu}  (per frame p50 {p50:.2}, p95 {p95:.2} ms)\n    \
             wall              {:>8.2} ms/frame  ({:.1} fps, slowest {:.2} ms)",
            self.rows,
            self.columns,
            self.changing_percent,
            MEASURED_FRAMES,
            wall.as_secs_f64() * 1e3 / frames,
            frames / wall.as_secs_f64(),
            self.slowest.as_secs_f64() * 1e3,
        );

        // Everything below this line is unavailable in the build being compared
        // against, and is here to explain the wall clock rather than to be
        // compared with it.
        let stats = window.layout_stats();
        let counted = stats.frames.max(1) as f64;
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        println!(
            "    layout nodes      {:>8.0}\n    \
             build             {:>8.2} ms/frame\n    \
             prepaint          {:>8.2} ms/frame\n    \
             paint             {:>8.2} ms/frame\n    \
             taffy compute     {:>8.2} ms/frame  ({:.0}% of it measuring text)\n    \
             nodes             {:>8.0} created  {:.0} reused\n    \
             style writes      {:>8.0}/frame",
            (stats.nodes_created + stats.nodes_reused) as f64 / counted,
            ms(stats.build_time) / counted,
            ms(stats.prepaint_time) / counted,
            ms(stats.paint_time) / counted,
            ms(stats.compute_layout_time) / counted,
            if stats.compute_layout_time.is_zero() {
                0.0
            } else {
                100.0 * stats.measure_time.as_secs_f64() / stats.compute_layout_time.as_secs_f64()
            },
            stats.nodes_created as f64 / counted,
            stats.nodes_reused as f64 / counted,
            stats.style_writes as f64 / counted,
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
    let rows: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(50);
    let columns: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(50);
    let changing_percent: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(100);

    application().run(move |cx| {
        if !example_support::load_fonts(cx) {
            return;
        }
        cx.open_window(
            WindowOptions {
                focus: true,
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1400.), px(820.)),
                    cx,
                ))),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Grid::new(rows, columns, changing_percent)),
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
