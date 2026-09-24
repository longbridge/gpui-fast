//! A long list of table-like rows scrolled by a fixed amount every frame,
//! drawn into a real window and presented by the GPU.
//!
//! Like `grid_frames`, this exists to be run against two builds of gpui and
//! compared, so everything that decides what is drawn is fixed here. Where
//! `grid_frames` changes what a still tree shows, this one moves a tree whose
//! contents never change, which is what a user dragging a list around does:
//! the rows that stay in view can be carried over from the last frame, and the
//! rows that scroll in cannot.
//!
//! ```text
//! cargo run -p gpui --example scroll_frames --release -- <container> <motion> <speed> <keying> [rows] [overdraw] [cells] [panes]
//! ```
//!
//! - `container`: `uniform` (`uniform_list`) or `list` (`list`, variable-height
//!   capable, measuring each row once).
//! - `motion`: `still`, `down` (one way, at a constant speed) or `oscillate`
//!   (back and forth over two viewports, so rows leave and come back).
//! - `speed`: pixels scrolled per frame. Rows are 24px tall.
//! - `keying`: `none`, or `index` to give every row an `ElementId` from its
//!   index. Nothing is inserted, so the index is as stable as a data id here.
//! - `rows`: how many rows the list holds, 10000 by default.
//! - `overdraw`: pixels the `list` container renders beyond its viewport.
//! - `cells`: extra narrow text columns per row, 0 by default. A wide table
//!   is what brings the work of a frame near its budget, where the difference
//!   between two builds stops being hidden by the processor clocking down.
//! - `panes`: how many lists are drawn side by side, 1 by default. Each one
//!   scrolls from a different place, so no two show the same rows, and more
//!   than one opens a larger window to fit them.
//!
//! Every row carries text of its own, prepared up front, so a row scrolling
//! back into view cannot borrow the shaping of a row that happens to show the
//! same label.
//!
//! The work here fits well inside a frame, so the wall clock only reports the
//! display's refresh rate. What two builds are compared on is CPU time per
//! frame, which both builds can measure: the main thread's, where gpui builds,
//! lays out and paints, and the whole process's, which adds the renderer's and
//! the driver's threads and is noisier for it.

#[path = "example_support/fonts.rs"]
mod example_support;

use gpui::{
    Bounds, Context, ListAlignment, ListOffset, ListState, Pixels, Render, SharedString,
    UniformListScrollHandle, Window, WindowBounds, WindowOptions, div, hsla, list, point,
    prelude::*, px, size, uniform_list,
};
use gpui_platform::application;
use std::{
    ops::Range,
    rc::Rc,
    time::{Duration, Instant},
};

/// Frames drawn before the clock starts, so that atlas population, pipeline
/// compilation and the window settling are not counted as steady state.
const WARMUP_FRAMES: usize = 60;

/// Frames measured after that: enough for an oscillation at the slowest speed
/// worth measuring to go back and forth several times.
const MEASURED_FRAMES: usize = 600;

const ROW_HEIGHT: f32 = 24.;

/// Where the scroll starts, in rows, so that the list has room to move in
/// both directions without meeting either end.
const START_ROW: usize = 1000;

/// Rows between where one pane starts and where the next one does: more than
/// any measured run scrolls, so the panes never show the same rows.
const PANE_STRIDE: usize = 2000;

const WORDS: [&str; 16] = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliett",
    "kilo", "lima", "mike", "november", "oscar", "papa",
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Container {
    Uniform,
    List,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Motion {
    Still,
    Down,
    Oscillate,
}

/// The text of one row, prepared before the first frame.
struct RowText {
    number: SharedString,
    name: SharedString,
    description: SharedString,
    tag: SharedString,
    cells: Vec<SharedString>,
}

/// One of the lists drawn side by side, scrolled by the same amount as the
/// others from a place of its own.
struct Pane {
    start_row: usize,
    uniform_scroll: UniformListScrollHandle,
    list_state: ListState,
    /// The rows in view last frame, to count the ones that scrolled in.
    last_visible: Option<Range<usize>>,
}

struct ScrollFrames {
    container: Container,
    motion: Motion,
    speed: f32,
    keyed: bool,
    overdraw: Pixels,
    rows: Rc<[RowText]>,
    panes: Vec<Pane>,

    tick: usize,
    frames: usize,
    measuring_since: Option<Instant>,
    cpu_since: Option<(Duration, Duration)>,
    last_frame_at: Option<Instant>,
    slowest: Duration,
    /// Main-thread CPU time at the last frame, and what each measured frame
    /// took of it. Spread matters as much as the mean here: a row that moves
    /// into a different slot costs its work in one frame, not across all.
    last_main_cpu: Option<Duration>,
    frame_main_cpu: Vec<Duration>,
    rows_entered: usize,
    rows_visible: usize,
}

impl Render for ScrollFrames {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Asking for the next frame from inside this one is what keeps the
        // window redrawing without an input to provoke it.
        window.request_animation_frame();

        let now = Instant::now();
        if let Some(last) = self.last_frame_at
            && self.measuring_since.is_some()
        {
            self.slowest = self.slowest.max(now - last);
        }
        self.last_frame_at = Some(now);

        let main_cpu = cpu_time().map(|(main, _)| main);
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
            self.cpu_since = cpu_time();
            self.slowest = Duration::ZERO;
            self.frame_main_cpu.clear();
            self.rows_entered = 0;
            self.rows_visible = 0;
        } else if self.frames == WARMUP_FRAMES + MEASURED_FRAMES {
            self.report(window);
            cx.quit();
        }

        let viewport_height = window.viewport_size().height;
        let offset = self.scrolled(viewport_height);
        self.tick += 1;

        let rows = self.rows.clone();
        let keyed = self.keyed;
        let container = self.container;
        let (mut entered, mut visible) = (0, 0);
        let panes = self
            .panes
            .iter_mut()
            .map(|pane| {
                let scroll_top = px(pane.start_row as f32 * ROW_HEIGHT) + offset;
                let (pane_entered, pane_visible) =
                    pane.count_rows(scroll_top, viewport_height, rows.len());
                entered += pane_entered;
                visible += pane_visible;

                let rows = rows.clone();
                let content = match container {
                    Container::Uniform => {
                        pane.uniform_scroll
                            .0
                            .borrow()
                            .base_handle
                            .set_offset(point(px(0.), -scroll_top));
                        uniform_list("rows", rows.len(), move |range, _, _| {
                            range.map(|ix| render_row(&rows, ix, keyed)).collect()
                        })
                        .track_scroll(&pane.uniform_scroll)
                        .size_full()
                        .into_any_element()
                    }
                    Container::List => {
                        let row = (scroll_top / px(ROW_HEIGHT)).floor() as usize;
                        pane.list_state.scroll_to(ListOffset {
                            item_ix: row,
                            offset_in_item: scroll_top - px(row as f32 * ROW_HEIGHT),
                        });
                        list(pane.list_state.clone(), move |ix, _, _| {
                            render_row(&rows, ix, keyed)
                        })
                        .size_full()
                        .into_any_element()
                    }
                };
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_hidden()
                    .border_r_1()
                    .border_color(hsla(0., 0., 0.3, 1.))
                    .child(content)
            })
            .collect::<Vec<_>>();
        self.rows_entered += entered;
        self.rows_visible += visible;

        div()
            .size_full()
            .flex()
            .flex_row()
            .bg(hsla(0., 0., 0.12, 1.))
            .text_color(hsla(0., 0., 0.85, 1.))
            .text_sm()
            .children(panes)
    }
}

impl Pane {
    /// Records which rows are in view, and returns how many of them were not
    /// in view last frame and how many there are. This is worked out from the
    /// scroll position rather than counted in the render closure, which also
    /// renders a row to measure it.
    fn count_rows(
        &mut self,
        scroll_top: Pixels,
        viewport_height: Pixels,
        row_count: usize,
    ) -> (usize, usize) {
        let first = (f32::from(scroll_top) / ROW_HEIGHT).floor() as usize;
        let last = (f32::from(scroll_top + viewport_height) / ROW_HEIGHT).ceil() as usize;
        let visible = first..last.min(row_count);
        let entered = match &self.last_visible {
            Some(previous) => visible.clone().filter(|ix| !previous.contains(ix)).count(),
            None => visible.len(),
        };
        let count = visible.len();
        self.last_visible = Some(visible);
        (entered, count)
    }
}

/// One table-like row: a number, a status swatch, two text columns of
/// different lengths and a bordered tag.
fn render_row(rows: &[RowText], ix: usize, keyed: bool) -> gpui::AnyElement {
    let text = &rows[ix];
    let row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(px(ROW_HEIGHT))
        .px_2()
        .border_b_1()
        .border_color(hsla(0., 0., 0.2, 1.))
        .child(
            div()
                .w(px(56.))
                .text_color(hsla(0., 0., 0.55, 1.))
                .child(text.number.clone()),
        )
        .child(
            div()
                .size(px(10.))
                .rounded_sm()
                .bg(hsla((ix % 5) as f32 / 5., 0.6, 0.5, 1.)),
        )
        .child(div().w(px(220.)).overflow_hidden().child(text.name.clone()))
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .child(text.description.clone()),
        )
        .child(
            div()
                .w(px(96.))
                .px_1()
                .border_1()
                .border_color(hsla(0., 0., 0.35, 1.))
                .rounded_sm()
                .child(div().child(text.tag.clone())),
        )
        .children(
            text.cells
                .iter()
                .map(|cell| div().w(px(52.)).overflow_hidden().child(cell.clone())),
        );
    if keyed {
        row.id(("row", ix)).into_any_element()
    } else {
        row.into_any_element()
    }
}

impl ScrollFrames {
    fn new(
        container: Container,
        motion: Motion,
        speed: f32,
        keyed: bool,
        rows: usize,
        overdraw: Pixels,
        cells: usize,
        panes: usize,
    ) -> Self {
        let rows: Rc<[RowText]> = (0..rows)
            .map(|ix| RowText {
                number: format!("{ix:05}").into(),
                name: format!("{} {} {ix}", WORDS[ix % 16], WORDS[(ix / 16) % 16]).into(),
                description: format!(
                    "{} {} {} {} — row {ix} of the table",
                    WORDS[(ix * 7) % 16],
                    WORDS[(ix * 3) % 16],
                    WORDS[(ix * 5) % 16],
                    WORDS[(ix * 11) % 16],
                )
                .into(),
                tag: format!("tag-{}", ix % 1000).into(),
                cells: (0..cells).map(|c| format!("{ix}.{c}").into()).collect(),
            })
            .collect();
        let panes = (0..panes)
            .map(|pane| Pane {
                start_row: START_ROW + pane * PANE_STRIDE,
                uniform_scroll: UniformListScrollHandle::new(),
                list_state: ListState::new(rows.len(), ListAlignment::Top, overdraw),
                last_visible: None,
            })
            .collect();
        ScrollFrames {
            container,
            motion,
            speed,
            keyed,
            overdraw,
            rows,
            panes,
            tick: 0,
            frames: 0,
            measuring_since: None,
            cpu_since: None,
            last_frame_at: None,
            slowest: Duration::ZERO,
            last_main_cpu: None,
            frame_main_cpu: Vec::new(),
            rows_entered: 0,
            rows_visible: 0,
        }
    }

    /// How far every pane has scrolled from where it started, this frame.
    fn scrolled(&self, viewport_height: Pixels) -> Pixels {
        let travelled = self.tick as f32 * self.speed;
        let offset = match self.motion {
            Motion::Still => 0.,
            Motion::Down => travelled,
            Motion::Oscillate => {
                // A triangle wave over two viewports: down, then back up over
                // the same rows.
                let span = 2. * f32::from(viewport_height);
                let phase = travelled % (2. * span);
                if phase < span {
                    phase
                } else {
                    2. * span - phase
                }
            }
        };
        px(offset)
    }

    fn report(&self, window: &Window) {
        let wall = self
            .measuring_since
            .map(|at| at.elapsed())
            .unwrap_or_default();
        let frames = MEASURED_FRAMES as f64;
        let per_frame = |d: Duration| format!("{:>8.2} ms/frame", d.as_secs_f64() * 1e3 / frames);
        let (main_cpu, process_cpu) = match (cpu_time(), self.cpu_since) {
            (Some((main, process)), Some((main_since, process_since))) => (
                per_frame(main - main_since),
                per_frame(process - process_since),
            ),
            _ => ("     n/a".into(), "     n/a".into()),
        };
        let mut frame_cpu = self.frame_main_cpu.clone();
        frame_cpu.sort();
        let percentile = |p: f64| {
            frame_cpu
                .get(((frame_cpu.len() as f64 - 1.) * p).round() as usize)
                .map_or(0., |d| d.as_secs_f64() * 1e3)
        };
        let (p50, p95, worst) = (percentile(0.5), percentile(0.95), percentile(1.));
        println!(
            "\n  {:?} list x{}, {} rows, {:?} at {} px/frame, {}, overdraw {} px, over {} frames\n    \
             main cpu          {main_cpu}  (per frame p50 {p50:.2}, p95 {p95:.2}, max {worst:.2} ms)\n    \
             process cpu       {process_cpu}\n    \
             wall              {:>8.2} ms/frame  ({:.1} fps, slowest {:.2} ms)\n    \
             rows in view      {:>8.1}/frame  ({:.1} scrolled in)",
            self.container,
            self.panes.len(),
            self.rows.len(),
            self.motion,
            self.speed,
            if self.keyed {
                "keyed by index"
            } else {
                "unkeyed"
            },
            f32::from(self.overdraw),
            MEASURED_FRAMES,
            wall.as_secs_f64() * 1e3 / frames,
            frames / wall.as_secs_f64(),
            self.slowest.as_secs_f64() * 1e3,
            self.rows_visible as f64 / frames,
            self.rows_entered as f64 / frames,
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
             text shaping      {:>8.2} ms/frame  ({:.1} lines/frame)\n    \
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
            ms(stats.shape_time) / counted,
            stats.lines_shaped as f64 / counted,
            stats.nodes_created as f64 / counted,
            stats.nodes_reused as f64 / counted,
            stats.style_writes as f64 / counted,
        );
    }
}

/// CPU time used so far by the calling thread, which has to be the main
/// thread for this to mean anything, and by the whole process.
#[cfg(unix)]
fn cpu_time() -> Option<(Duration, Duration)> {
    let mut thread = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: `clock_gettime` fills in the whole struct when it returns 0.
    let thread = unsafe {
        if libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, thread.as_mut_ptr()) != 0 {
            return None;
        }
        thread.assume_init()
    };
    let thread = Duration::new(thread.tv_sec as u64, thread.tv_nsec as u32);

    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `getrusage` fills in the whole struct when it returns 0.
    let usage = unsafe {
        if libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) != 0 {
            return None;
        }
        usage.assume_init()
    };
    let time = |t: libc::timeval| {
        Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
    };
    Some((thread, time(usage.ru_utime) + time(usage.ru_stime)))
}

#[cfg(not(unix))]
fn cpu_time() -> Option<(Duration, Duration)> {
    None
}

fn run_example() {
    let mut args = std::env::args().skip(1);
    let container = match args.next().as_deref() {
        Some("list") => Container::List,
        _ => Container::Uniform,
    };
    let motion = match args.next().as_deref() {
        Some("still") => Motion::Still,
        Some("down") => Motion::Down,
        _ => Motion::Oscillate,
    };
    let speed: f32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(12.);
    let keyed = matches!(args.next().as_deref(), Some("index"));
    let rows: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(10_000);
    let overdraw: f32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(0.);
    let cells: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(0);
    let panes: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(1).max(1);
    let window_size = if panes > 1 {
        size(px(1800.), px(1000.))
    } else {
        size(px(1000.), px(820.))
    };

    application().run(move |cx| {
        if !example_support::load_fonts(cx) {
            return;
        }
        cx.open_window(
            WindowOptions {
                focus: true,
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    window_size,
                    cx,
                ))),
                ..Default::default()
            },
            move |_, cx| {
                cx.new(|_| {
                    ScrollFrames::new(
                        container,
                        motion,
                        speed,
                        keyed,
                        rows,
                        px(overdraw),
                        cells,
                        panes,
                    )
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
