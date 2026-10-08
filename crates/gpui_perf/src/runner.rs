//! Drives the scenarios headlessly and measures what their frames cost.
//!
//! Every scenario runs in its own [`HeadlessAppContext`], with a real text
//! system (cosmic-text, loaded with the repository's embedded IBM Plex Sans
//! and Lilex fonts only, so results don't depend on the fonts installed), in
//! one window of a fixed size. It runs once per retention mode, each time from
//! a freshly built scenario, so the two runs draw the same frames.
//!
//! # What a frame costs
//!
//! A frame is: `Scenario::step` inside `update_window`, then the app runs until
//! it is parked. The test platform draws a window that became dirty as soon as
//! the update that dirtied it flushes its effects, exactly once, the way a
//! real frame callback would; that draw — and every observer, subscription and
//! task the step set off — is the frame. The step itself is timed separately
//! and taken out. If a step leaves the window clean, so that nothing drew, the
//! window is drawn explicitly and that draw counts instead (`forced_draws`).
//!
//! Times are per-thread CPU time (`CLOCK_THREAD_CPUTIME_ID`) where available,
//! which doesn't count time the thread spent descheduled; wall time is kept
//! alongside.
//!
//! Allocations are counted process-wide by [`crate::alloc::CountingAllocator`]
//! when the binary installs it, and, like time, exclude the step's own.

use std::{
    borrow::Cow,
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    AnyView, AnyWindowHandle, App, AppContext as _, Context, HeadlessAppContext, IntoElement,
    LayoutStats, Quad, Render, Size, Window, px,
};
use serde::Serialize;

use crate::{Scenario, all_scenarios, alloc::allocations};

/// Which retention modes to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionModes {
    Off,
    On,
    Both,
}

impl RetentionModes {
    fn modes(self) -> &'static [bool] {
        match self {
            RetentionModes::Off => &[false],
            RetentionModes::On => &[true],
            RetentionModes::Both => &[false, true],
        }
    }
}

/// What to run and how.
#[derive(Clone, Debug)]
pub struct Options {
    /// Run only scenarios whose name contains one of these. Empty runs all.
    pub filters: Vec<String>,
    /// Frames measured per run.
    pub frames: usize,
    /// Frames drawn before measuring.
    pub warmup: usize,
    pub retention: RetentionModes,
    /// Also run both modes in lockstep and compare what they paint.
    pub verify: bool,
    /// Window size in logical pixels.
    pub window_size: (f32, f32),
}

impl Default for Options {
    fn default() -> Self {
        Self {
            filters: Vec::new(),
            frames: 200,
            warmup: 30,
            retention: RetentionModes::Both,
            verify: false,
            window_size: (1440., 900.),
        }
    }
}

/// Distribution of a per-frame time, in milliseconds.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Summary {
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

impl Summary {
    fn of(samples: &[Duration]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1e3).collect();
        ms.sort_by(|a, b| a.total_cmp(b));
        let percentile = |p: f64| {
            let index = ((ms.len() - 1) as f64 * p).round() as usize;
            ms[index]
        };
        Self {
            mean_ms: ms.iter().sum::<f64>() / ms.len() as f64,
            p50_ms: percentile(0.5),
            p95_ms: percentile(0.95),
            max_ms: *ms.last().unwrap(),
        }
    }
}

/// [`LayoutStats`] over the measured frames, averaged per frame.
#[derive(Clone, Debug, Default, Serialize)]
pub struct PhaseAverages {
    pub build_ms: f64,
    pub prepaint_ms: f64,
    pub paint_ms: f64,
    pub compute_layout_ms: f64,
    pub shape_ms: f64,
    pub lines_shaped: f64,
    pub nodes_created: f64,
    pub nodes_reused: f64,
    pub style_writes: f64,
    pub measure_calls: f64,
    pub measure_rebinds: f64,
    pub compute_layout_calls: f64,
    /// Draws per frame (`LayoutStats::frames` per measured frame).
    pub draws: f64,
    pub views_built: f64,
    pub views_reused: f64,
    /// Scroll containers drawn from a scroll layer's cached tiles.
    pub layer_frames_composited: f64,
    /// Of those, the frames whose content was painted into the layer again.
    pub layer_frames_repainted: f64,
    /// Scroll layer tiles repaints changed.
    pub tiles_dirtied: f64,
    /// Scroll layers painted again before an input event.
    pub layer_rebuilds_for_input: f64,
}

impl PhaseAverages {
    fn of(stats: &LayoutStats, frames: usize) -> Self {
        let n = frames.max(1) as f64;
        let ms = |d: Duration| d.as_secs_f64() * 1e3 / n;
        Self {
            build_ms: ms(stats.build_time),
            prepaint_ms: ms(stats.prepaint_time),
            paint_ms: ms(stats.paint_time),
            compute_layout_ms: ms(stats.compute_layout_time),
            shape_ms: ms(stats.shape_time),
            lines_shaped: stats.lines_shaped as f64 / n,
            nodes_created: stats.nodes_created as f64 / n,
            nodes_reused: stats.nodes_reused as f64 / n,
            style_writes: stats.style_writes as f64 / n,
            measure_calls: stats.measure_calls as f64 / n,
            measure_rebinds: stats.measure_rebinds as f64 / n,
            compute_layout_calls: stats.compute_layout_calls as f64 / n,
            draws: stats.frames as f64 / n,
            views_built: stats.views_built as f64 / n,
            views_reused: stats.views_reused as f64 / n,
            layer_frames_composited: stats.layer_frames_composited as f64 / n,
            layer_frames_repainted: stats.layer_frames_repainted as f64 / n,
            tiles_dirtied: stats.tiles_dirtied as f64 / n,
            layer_rebuilds_for_input: stats.layer_rebuilds_for_input as f64 / n,
        }
    }
}

/// One scenario in one retention mode.
#[derive(Clone, Debug, Serialize)]
pub struct RunReport {
    pub retention: bool,
    pub frames: usize,
    pub warmup: usize,
    /// Whether `frame` is thread CPU time; if not, it is wall time.
    pub cpu_clock: bool,
    /// What a frame cost, step excluded (CPU time where available).
    pub frame: Summary,
    /// The same, in wall time.
    pub frame_wall: Summary,
    /// Main-thread instructions per frame, the mean, in millions, where the
    /// CPU counts them: unlike time, it doesn't depend on what else the
    /// machine is doing.
    pub instructions_m: Option<f64>,
    /// What `Scenario::step` itself took; not part of `frame`.
    pub step: Summary,
    pub phases: PhaseAverages,
    /// Allocations per frame, step excluded.
    pub allocations: f64,
    /// Bytes allocated per frame, in KiB, step excluded.
    pub allocated_kib: f64,
    /// Measured frames whose step left the window clean, drawn explicitly.
    pub forced_draws: usize,
}

/// Result of running two windows in lockstep and comparing what they painted.
#[derive(Clone, Debug, Serialize)]
pub struct VerifyReport {
    pub frames_compared: usize,
    pub passed: bool,
    /// The first frame whose painted quads differed, and how.
    pub first_mismatch: Option<String>,
}

/// Everything measured for one scenario.
#[derive(Clone, Debug, Serialize)]
pub struct ScenarioReport {
    pub name: String,
    pub description: String,
    pub runs: Vec<RunReport>,
    /// Retention on against off, scroll layers off in both.
    pub verify: Option<VerifyReport>,
    /// For scroll scenarios, scroll layers on against off, retention on in
    /// both.
    pub verify_layers: Option<VerifyReport>,
}

impl ScenarioReport {
    /// Whether a verification ran and found the two windows painting
    /// differently.
    pub fn failed_verification(&self) -> bool {
        [&self.verify, &self.verify_layers]
            .into_iter()
            .any(|verify| verify.as_ref().is_some_and(|verify| !verify.passed))
    }

    pub fn run(&self, retention: bool) -> Option<&RunReport> {
        self.runs.iter().find(|run| run.retention == retention)
    }
}

/// The scenarios `options` selects, as (index into [`all_scenarios`], name,
/// description).
pub fn selected_scenarios(options: &Options) -> Vec<(usize, &'static str, &'static str)> {
    all_scenarios()
        .iter()
        .enumerate()
        .filter(|(_, scenario)| {
            options.filters.is_empty()
                || options
                    .filters
                    .iter()
                    .any(|filter| scenario.name().contains(filter.as_str()))
        })
        .map(|(index, scenario)| (index, scenario.name(), scenario.description()))
        .collect()
}

/// Runs every selected scenario in every selected mode, printing progress to
/// stderr.
pub fn run(options: &Options) -> Vec<ScenarioReport> {
    let mut reports = Vec::new();
    for (index, name, description) in selected_scenarios(options) {
        let mut runs = Vec::new();
        for &retention in options.retention.modes() {
            eprintln!(
                "running {name} (retention {})...",
                if retention { "on" } else { "off" }
            );
            runs.push(measure(index, retention, options));
        }
        let verify = options.verify.then(|| {
            eprintln!("verifying {name}...");
            verify(index, options)
        });
        let verify_layers = (options.verify && is_scroll_scenario(name)).then(|| {
            eprintln!("verifying {name} with scroll layers...");
            verify_layers(index, options)
        });
        reports.push(ScenarioReport {
            name: name.to_string(),
            description: description.to_string(),
            runs,
            verify,
            verify_layers,
        });
    }
    reports
}

/// Whether scenario `name` scrolls, which `--verify` then also runs with
/// scroll layers on and off.
fn is_scroll_scenario(name: &str) -> bool {
    name.contains("scroll")
}

/// A fresh instance of scenario `index`, so no state carries over between runs.
fn fresh_scenario(index: usize) -> Box<dyn Scenario> {
    all_scenarios().swap_remove(index)
}

struct Host(AnyView);

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.0.clone()
    }
}

fn load_fonts(cx: &App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(include_bytes!(
            "../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Italic.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBoldItalic.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../../assets/fonts/lilex/Lilex-Regular.ttf"
        )),
        Cow::Borrowed(include_bytes!("../../../assets/fonts/lilex/Lilex-Bold.ttf")),
        Cow::Borrowed(include_bytes!(
            "../../../assets/fonts/lilex/Lilex-Italic.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../../assets/fonts/lilex/Lilex-BoldItalic.ttf"
        )),
    ];
    cx.text_system()
        .add_fonts(fonts)
        .expect("failed to load the embedded fonts");
}

fn new_context() -> HeadlessAppContext {
    let text_system = Arc::new(gpui_wgpu::CosmicTextSystem::new_without_system_fonts(
        "IBM Plex Sans",
    ));
    let mut cx = HeadlessAppContext::with_asset_source(
        text_system,
        Arc::new(crate::scenarios::scroll::ScenarioAssets),
    );
    cx.update(|cx| load_fonts(cx));
    cx
}

/// Opens a window showing a freshly built scenario and draws its first frame.
fn open(
    cx: &mut HeadlessAppContext,
    scenario: &dyn Scenario,
    retention: bool,
    options: &Options,
) -> (AnyWindowHandle, AnyView) {
    let (width, height) = options.window_size;
    let mut root = None;
    let handle = cx
        .open_window(
            Size {
                width: px(width),
                height: px(height),
            },
            |window, cx| {
                let view = scenario.build(window, cx);
                root = Some(view.clone());
                cx.new(|_| Host(view))
            },
        )
        .expect("failed to open a headless window");
    let window: AnyWindowHandle = handle.into();
    cx.update_window(window, |_, window, cx| {
        window.set_view_retention(retention);
        window.refresh();
        window.draw(cx).clear(cx);
    })
    .unwrap();
    cx.run_until_parked();
    (window, root.unwrap())
}

#[derive(Clone, Copy)]
struct Stamp {
    wall: Instant,
    cpu: Option<Duration>,
    instructions: Option<u64>,
}

impl Stamp {
    fn now() -> Self {
        Self {
            wall: Instant::now(),
            cpu: thread_cpu_time(),
            instructions: crate::instructions::main_thread_instructions(),
        }
    }

    /// Instructions retired since `self`, where the CPU counts them.
    fn instructions_since(&self) -> Option<u64> {
        let now = crate::instructions::main_thread_instructions()?;
        Some(now.saturating_sub(self.instructions?))
    }

    /// (cpu-or-wall, wall) elapsed since `self`.
    fn elapsed(&self) -> (Duration, Duration) {
        let now = Stamp::now();
        let wall = now.wall - self.wall;
        let cpu = match (self.cpu, now.cpu) {
            (Some(start), Some(end)) => end.saturating_sub(start),
            _ => wall,
        };
        (cpu, wall)
    }
}

#[cfg(unix)]
fn thread_cpu_time() -> Option<Duration> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    (result == 0).then(|| Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
}

#[cfg(not(unix))]
fn thread_cpu_time() -> Option<Duration> {
    None
}

struct FrameSample {
    /// CPU (or wall) time of the frame, step excluded.
    cost: Duration,
    cost_wall: Duration,
    /// Main-thread instructions of the frame, step excluded, where the CPU
    /// counts them.
    instructions: Option<u64>,
    step: Duration,
    forced: bool,
    allocations: u64,
    allocated_bytes: u64,
}

fn draws_so_far(cx: &mut HeadlessAppContext, window: AnyWindowHandle) -> u64 {
    cx.update_window(window, |_, window, _| window.layout_stats().frames)
        .unwrap()
}

/// Steps the scenario and lets the frame it causes be drawn. See the module
/// documentation for what is counted.
fn frame(
    cx: &mut HeadlessAppContext,
    window: AnyWindowHandle,
    scenario: &dyn Scenario,
    root: &AnyView,
    frame: usize,
) -> FrameSample {
    let draws_before = draws_so_far(cx, window);

    let allocations_before = allocations();
    let start = Stamp::now();
    let (step, step_wall, step_allocations, step_instructions) = cx
        .update_window(window, |_, window, cx| {
            let allocations_before = allocations();
            let start = Stamp::now();
            scenario.step(root, frame, window, cx);
            let step_instructions = start.instructions_since();
            let (step, step_wall) = start.elapsed();
            let allocations_after = allocations();
            (
                step,
                step_wall,
                (
                    allocations_after.0 - allocations_before.0,
                    allocations_after.1 - allocations_before.1,
                ),
                step_instructions,
            )
        })
        .unwrap();
    cx.run_until_parked();
    let (mut cost, mut cost_wall) = start.elapsed();
    let mut instructions = start
        .instructions_since()
        .map(|all| all.saturating_sub(step_instructions.unwrap_or(0)));
    cost = cost.saturating_sub(step);
    cost_wall = cost_wall.saturating_sub(step_wall);

    let forced = draws_so_far(cx, window) == draws_before;
    if forced {
        let start = Stamp::now();
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        cx.run_until_parked();
        let forced_instructions = start.instructions_since();
        let (cpu, wall) = start.elapsed();
        cost += cpu;
        cost_wall += wall;
        instructions = instructions.zip(forced_instructions).map(|(a, b)| a + b);
    }
    let allocations_after = allocations();

    FrameSample {
        cost,
        cost_wall,
        instructions,
        step,
        forced,
        allocations: (allocations_after.0 - allocations_before.0)
            .saturating_sub(step_allocations.0),
        allocated_bytes: (allocations_after.1 - allocations_before.1)
            .saturating_sub(step_allocations.1),
    }
}

/// Runs scenario `index` in one retention mode.
fn measure(index: usize, retention: bool, options: &Options) -> RunReport {
    let scenario = fresh_scenario(index);
    let mut cx = new_context();
    let (window, root) = open(&mut cx, &*scenario, retention, options);

    for n in 0..options.warmup {
        frame(&mut cx, window, &*scenario, &root, n);
    }

    cx.update_window(window, |_, window, _| window.reset_layout_stats())
        .unwrap();

    let mut costs = Vec::with_capacity(options.frames);
    let mut costs_wall = Vec::with_capacity(options.frames);
    let mut instructions = Some(Vec::with_capacity(options.frames));
    let mut steps = Vec::with_capacity(options.frames);
    let mut forced_draws = 0;
    let mut allocation_count = 0;
    let mut allocated_bytes = 0;
    for n in 0..options.frames {
        let sample = frame(&mut cx, window, &*scenario, &root, options.warmup + n);
        costs.push(sample.cost);
        costs_wall.push(sample.cost_wall);
        match (&mut instructions, sample.instructions) {
            (Some(all), Some(count)) => all.push(count),
            _ => instructions = None,
        }
        steps.push(sample.step);
        forced_draws += sample.forced as usize;
        allocation_count += sample.allocations;
        allocated_bytes += sample.allocated_bytes;
    }
    let frames = options.frames.max(1) as f64;

    let stats = cx
        .update_window(window, |_, window, _| window.layout_stats())
        .unwrap();

    RunReport {
        retention,
        frames: options.frames,
        warmup: options.warmup,
        cpu_clock: thread_cpu_time().is_some(),
        frame: Summary::of(&costs),
        frame_wall: Summary::of(&costs_wall),
        instructions_m: instructions
            .map(|all| all.iter().sum::<u64>() as f64 / all.len().max(1) as f64 / 1e6),
        step: Summary::of(&steps),
        phases: PhaseAverages::of(&stats, options.frames),
        allocations: allocation_count as f64 / frames,
        allocated_kib: allocated_bytes as f64 / 1024. / frames,
        forced_draws,
    }
}

/// A quad as text, without its draw order, which the two modes may number
/// differently while painting the same thing.
fn describe_quad(quad: &Quad) -> String {
    format!(
        "bounds {:?} mask {:?} background {:?} border {:?} {:?} {:?} radii {:?}",
        quad.bounds,
        quad.content_mask,
        quad.background,
        quad.border_style,
        quad.border_widths,
        quad.border_color,
        quad.corner_radii,
    )
}

/// What the window painted last: its quads, then its text, icons, images
/// and underlines.
fn painted_quads(cx: &mut HeadlessAppContext, window: AnyWindowHandle) -> Vec<String> {
    cx.update_window(window, |_, window, _| {
        let mut painted: Vec<String> = window.painted_quads().iter().map(describe_quad).collect();
        painted.extend(window.painted_sprites());
        painted
    })
    .unwrap()
}

/// How `on` differs from `off`, two windows' painted frames, which differ in
/// `what`: retention or scroll layers.
fn compare_quads(off: &[String], on: &[String], what: &str) -> Option<String> {
    if off == on {
        return None;
    }
    let mut counts: HashMap<&str, isize> = HashMap::new();
    for quad in off {
        *counts.entry(quad).or_default() += 1;
    }
    for quad in on {
        *counts.entry(quad).or_default() -= 1;
    }
    if off.len() != on.len() {
        // A primitive one window painted more often than the other, the
        // first in the drawing order of the window painting more.
        let (more, sign) = if off.len() > on.len() {
            (off, 1)
        } else {
            (on, -1)
        };
        let extra = more
            .iter()
            .find(|quad| counts[quad.as_str()] * sign > 0)
            .map_or("", String::as_str);
        return Some(format!(
            "{} primitives painted without {what}, {} with; only {} paints {extra}",
            off.len(),
            on.len(),
            if sign > 0 { "without" } else { "with" }
        ));
    }
    let first = off.iter().zip(on).position(|(a, b)| a != b).unwrap();
    if counts.values().all(|&count| count == 0) {
        Some(format!(
            "the same {} primitives, in a different order (first at {first}: {} vs {})",
            off.len(),
            off[first],
            on[first]
        ))
    } else {
        Some(format!(
            "primitive {first} differs: without {what} {}, with {}",
            off[first], on[first]
        ))
    }
}

/// Turns scroll layers on or off in `window` and draws it again.
fn set_scroll_layers(cx: &mut HeadlessAppContext, window: AnyWindowHandle, enabled: bool) {
    cx.update_window(window, |_, window, cx| {
        window.set_scroll_layers(enabled);
        window.draw(cx).clear(cx);
    })
    .unwrap();
    cx.run_until_parked();
}

/// Runs scenario `index` with and without retention in lockstep, in one app,
/// with scroll layers off in both, and compares what each frame painted:
/// quads, and text, icons, images and underlines, each with its bounds, clip
/// and colour.
fn verify(index: usize, options: &Options) -> VerifyReport {
    let off_scenario = fresh_scenario(index);
    let on_scenario = fresh_scenario(index);
    let mut cx = new_context();
    let (off_window, off_root) = open(&mut cx, &*off_scenario, false, options);
    let (on_window, on_root) = open(&mut cx, &*on_scenario, true, options);
    set_scroll_layers(&mut cx, off_window, false);
    set_scroll_layers(&mut cx, on_window, false);
    lockstep(
        &mut cx,
        [
            (off_window, &*off_scenario, &off_root),
            (on_window, &*on_scenario, &on_root),
        ],
        options,
        "retention",
        painted_quads,
    )
}

/// What the window painted last, scroll layers expanded into the content
/// they composite, in a canonical drawing order.
fn painted_primitives(cx: &mut HeadlessAppContext, window: AnyWindowHandle) -> Vec<String> {
    cx.update_window(window, |_, window, _| window.painted_primitives())
        .unwrap()
}

/// Runs scenario `index` with scroll layers on and off in lockstep, in one
/// app, with retention on in both, and compares what each frame painted,
/// the layer window's composited tiles replaced by the content they were
/// rasterized from.
fn verify_layers(index: usize, options: &Options) -> VerifyReport {
    let off_scenario = fresh_scenario(index);
    let on_scenario = fresh_scenario(index);
    let mut cx = new_context();
    let (off_window, off_root) = open(&mut cx, &*off_scenario, true, options);
    let (on_window, on_root) = open(&mut cx, &*on_scenario, true, options);
    set_scroll_layers(&mut cx, off_window, false);
    set_scroll_layers(&mut cx, on_window, true);
    lockstep(
        &mut cx,
        [
            (off_window, &*off_scenario, &off_root),
            (on_window, &*on_scenario, &on_root),
        ],
        options,
        "scroll layers",
        painted_primitives,
    )
}

/// One window of a lockstep run: the window, its scenario and the scenario's
/// root view.
type Lane<'a> = (AnyWindowHandle, &'a dyn Scenario, &'a AnyView);

/// Steps two windows, the first without `what` and the second with it,
/// through the same frames and compares what `painted` says each frame of
/// theirs painted.
fn lockstep(
    cx: &mut HeadlessAppContext,
    [
        (off_window, off_scenario, off_root),
        (on_window, on_scenario, on_root),
    ]: [Lane; 2],
    options: &Options,
    what: &str,
    painted: fn(&mut HeadlessAppContext, AnyWindowHandle) -> Vec<String>,
) -> VerifyReport {
    let total = options.warmup + options.frames;
    let mut first_mismatch = compare_quads(&painted(cx, off_window), &painted(cx, on_window), what)
        .map(|detail| format!("first frame: {detail}"));
    let mut frames_compared = 1;
    if first_mismatch.is_none() {
        for n in 0..total {
            frame(cx, off_window, off_scenario, off_root, n);
            frame(cx, on_window, on_scenario, on_root, n);
            frames_compared += 1;
            if let Some(detail) =
                compare_quads(&painted(cx, off_window), &painted(cx, on_window), what)
            {
                first_mismatch = Some(format!("frame {n}: {detail}"));
                break;
            }
        }
    }

    VerifyReport {
        frames_compared,
        passed: first_mismatch.is_none(),
        first_mismatch,
    }
}

fn change(off: f64, on: f64) -> String {
    if off <= 0. {
        return String::from("-");
    }
    format!("{:+.1}%", (on / off - 1.) * 100.)
}

/// Formats the reports as tables: one per scenario, then a summary.
pub fn format_reports(reports: &[ScenarioReport]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for report in reports {
        let _ = writeln!(out, "\n{} — {}", report.name, report.description);
        let runs: Vec<&RunReport> = report.runs.iter().collect();
        if runs.is_empty() {
            continue;
        }
        let first = runs[0];
        let clock = if first.cpu_clock { "cpu" } else { "wall" };
        let _ = write!(out, "  {:<24}", format!("({clock} time, per frame)"));
        for run in &runs {
            let _ = write!(
                out,
                "{:>12}",
                if run.retention {
                    "retained"
                } else {
                    "from scratch"
                }
            );
        }
        let both = report.run(false).zip(report.run(true));
        if both.is_some() {
            let _ = write!(out, "{:>10}", "change");
        }
        let _ = writeln!(out);

        type Row = (&'static str, fn(&RunReport) -> f64, usize);
        let rows: [Row; 26] = [
            ("frame mean ms", |r| r.frame.mean_ms, 3),
            ("frame p50 ms", |r| r.frame.p50_ms, 3),
            ("frame p95 ms", |r| r.frame.p95_ms, 3),
            ("frame max ms", |r| r.frame.max_ms, 3),
            ("wall mean ms", |r| r.frame_wall.mean_ms, 3),
            ("instructions M", |r| r.instructions_m.unwrap_or(0.), 2),
            ("  build ms", |r| r.phases.build_ms, 3),
            ("  prepaint ms", |r| r.phases.prepaint_ms, 3),
            ("    layout ms", |r| r.phases.compute_layout_ms, 3),
            ("  paint ms", |r| r.phases.paint_ms, 3),
            ("  shaping ms", |r| r.phases.shape_ms, 3),
            ("lines shaped", |r| r.phases.lines_shaped, 1),
            ("nodes created", |r| r.phases.nodes_created, 1),
            ("nodes reused", |r| r.phases.nodes_reused, 1),
            ("style writes", |r| r.phases.style_writes, 1),
            ("measure calls", |r| r.phases.measure_calls, 1),
            ("measure rebinds", |r| r.phases.measure_rebinds, 1),
            ("layout computes", |r| r.phases.compute_layout_calls, 1),
            ("draws", |r| r.phases.draws, 2),
            (
                "layer frames composited",
                |r| r.phases.layer_frames_composited,
                2,
            ),
            (
                "layer frames repainted",
                |r| r.phases.layer_frames_repainted,
                2,
            ),
            ("tiles dirtied", |r| r.phases.tiles_dirtied, 2),
            (
                "layer rebuilds for input",
                |r| r.phases.layer_rebuilds_for_input,
                2,
            ),
            ("allocations", |r| r.allocations, 1),
            ("allocated KiB", |r| r.allocated_kib, 1),
            ("step ms (not counted)", |r| r.step.mean_ms, 3),
        ];
        for (label, value, precision) in rows {
            let _ = write!(out, "  {label:<24}");
            for run in &runs {
                let _ = write!(out, "{:>12.*}", precision, value(run));
            }
            if let Some((off, on)) = both {
                let _ = write!(out, "{:>10}", change(value(off), value(on)));
            }
            let _ = writeln!(out);
        }
        for run in &runs {
            if run.forced_draws > 0 {
                let _ = writeln!(
                    out,
                    "  note: {} of {} frames drew nothing by themselves and were drawn explicitly ({})",
                    run.forced_draws,
                    run.frames,
                    if run.retention {
                        "retained"
                    } else {
                        "from scratch"
                    }
                );
            }
        }
        for (label, verify) in [
            ("verify", &report.verify),
            ("verify layers", &report.verify_layers),
        ] {
            let Some(verify) = verify else {
                continue;
            };
            match &verify.first_mismatch {
                None => {
                    let _ = writeln!(
                        out,
                        "  {label}: painted frames identical over {} frames",
                        verify.frames_compared
                    );
                }
                Some(detail) => {
                    let _ = writeln!(out, "  {label}: MISMATCH at {detail}");
                }
            }
        }
    }

    let compared: Vec<_> = reports
        .iter()
        .filter_map(|report| Some((report, report.run(false)?, report.run(true)?)))
        .collect();
    if !compared.is_empty() {
        let _ = writeln!(
            out,
            "\n{:<32}{:>14}{:>14}{:>10}{:>10}",
            "scenario", "scratch ms", "retained ms", "change", "speedup"
        );
        for (report, off, on) in compared {
            let speedup = if on.frame.mean_ms > 0. {
                format!("{:.2}x", off.frame.mean_ms / on.frame.mean_ms)
            } else {
                String::from("-")
            };
            let _ = writeln!(
                out,
                "{:<32}{:>14.3}{:>14.3}{:>10}{:>10}",
                report.name,
                off.frame.mean_ms,
                on.frame.mean_ms,
                change(off.frame.mean_ms, on.frame.mean_ms),
                speedup
            );
        }
    }
    out
}

/// The reports and the options that produced them, as written by `--json`.
#[derive(Serialize)]
pub struct JsonOutput<'a> {
    pub frames: usize,
    pub warmup: usize,
    pub window_size: (f32, f32),
    pub scenarios: &'a [ScenarioReport],
}

pub fn to_json(options: &Options, reports: &[ScenarioReport]) -> String {
    serde_json::to_string_pretty(&JsonOutput {
        frames: options.frames,
        warmup: options.warmup,
        window_size: options.window_size,
        scenarios: reports,
    })
    .expect("reports serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, SharedString, div, prelude::*};

    /// A frame one window painted more primitives in names one of them.
    #[test]
    fn a_mismatch_in_count_names_a_primitive_only_one_window_painted() {
        let strings = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let detail =
            compare_quads(&strings(&["a", "b"]), &strings(&["a", "c", "b"]), "layers").unwrap();
        assert!(detail.ends_with("only with paints c"), "{detail}");
        let detail =
            compare_quads(&strings(&["a", "d", "b"]), &strings(&["a", "b"]), "layers").unwrap();
        assert!(detail.ends_with("only without paints d"), "{detail}");
    }

    struct Counter {
        ticks: usize,
    }

    impl Render for Counter {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .flex_col()
                .child(SharedString::from(format!("ticks {}", self.ticks)))
                .children(
                    (0..50).map(|i| div().h(px(10.)).child(SharedString::from(i.to_string()))),
                )
        }
    }

    struct Tiny;

    impl Scenario for Tiny {
        fn name(&self) -> &'static str {
            "tiny"
        }
        fn description(&self) -> &'static str {
            "a counter ticking"
        }
        fn build(&self, _: &mut Window, cx: &mut App) -> AnyView {
            cx.new(|_| Counter { ticks: 0 }).into()
        }
        fn step(&self, root: &AnyView, _: usize, _: &mut Window, cx: &mut App) {
            let counter: Entity<Counter> = root.clone().downcast().unwrap();
            counter.update(cx, |counter, cx| {
                counter.ticks += 1;
                cx.notify();
            });
        }
    }

    #[test]
    fn tiny_scenario_runs() {
        let options = Options {
            frames: 5,
            warmup: 2,
            ..Default::default()
        };
        for retention in [false, true] {
            let scenario = Tiny;
            let mut cx = new_context();
            let (window, root) = open(&mut cx, &scenario, retention, &options);
            cx.update_window(window, |_, window, _| window.reset_layout_stats())
                .unwrap();
            for n in 0..options.frames {
                let sample = frame(&mut cx, window, &scenario, &root, n);
                assert!(!sample.forced, "notifying the root should draw a frame");
            }
            let stats = cx
                .update_window(window, |_, window, _| window.layout_stats())
                .unwrap();
            assert_eq!(stats.frames, options.frames as u64);
            assert!(!painted_quads(&mut cx, window).is_empty() || stats.lines_shaped > 0);
        }
    }

    /// `--verify` also runs scroll scenarios with scroll layers on and off,
    /// and the report shows what the layers did per frame.
    #[test]
    fn verify_compares_scroll_scenarios_with_scroll_layers_on_and_off() {
        let options = Options {
            frames: 6,
            warmup: 2,
            verify: true,
            filters: vec!["scroll-child-view".into(), "form-typing".into()],
            ..Default::default()
        };
        let reports = run(&options);
        let scroll = reports
            .iter()
            .find(|report| report.name == "scroll-child-view")
            .unwrap();
        let layers = scroll
            .verify_layers
            .as_ref()
            .expect("a scroll scenario is verified with layers on and off");
        assert!(layers.passed, "{:?}", layers.first_mismatch);
        assert_eq!(layers.frames_compared, 1 + options.warmup + options.frames);
        let form = reports
            .iter()
            .find(|report| report.name == "form-typing")
            .unwrap();
        assert!(form.verify_layers.is_none(), "only scroll scenarios");
        let text = format_reports(&reports);
        for row in [
            "layer frames composited",
            "tiles dirtied",
            "layer rebuilds for input",
            "verify layers: painted frames identical",
        ] {
            assert!(text.contains(row), "the report has no {row:?}:\n{text}");
        }
    }

    /// The scroll scenarios scroll by dispatching wheel events over their
    /// content: every frame draws by itself, because the wheel notified the
    /// view, and what it paints moves.
    #[test]
    fn scroll_scenarios_scroll_with_the_wheel() {
        const NAMES: [&str; 17] = [
            "scroll-child-view",
            "scroll-same-view",
            "scroll-uniform-list",
            "scroll-list",
            "chat-scroll",
            "chat-scroll-no-button",
            "chat-scroll-plain",
            "chat-scroll-reads-offset",
            "chat-scroll-reads-at-end",
            "chat-scroll-notifies",
            "chat-scroll-animates",
            "chat-scroll-sticks",
            "chat-scroll-parent-writes",
            "chat-scroll-chrome",
            "chat-scroll-trackpad",
            "chat-scroll-allsum",
            "chat-scroll-allsum-wheel",
        ];
        let options = Options::default();
        for name in NAMES {
            let index = all_scenarios()
                .iter()
                .position(|scenario| scenario.name() == name)
                .unwrap_or_else(|| panic!("no scenario named {name}"));
            let scenario = fresh_scenario(index);
            let mut cx = new_context();
            let (window, root) = open(&mut cx, &*scenario, true, &options);
            assert!(
                painted_quads(&mut cx, window)
                    .iter()
                    .any(|line| line.starts_with("monochrome")),
                "{name}: no icon or text painted"
            );
            // Composited tiles are listed where the layer holds them, so
            // compare the content they composite.
            let mut painted = painted_primitives(&mut cx, window);
            for n in 0..6 {
                let sample = frame(&mut cx, window, &*scenario, &root, n);
                assert!(!sample.forced, "{name}: frame {n} drew nothing by itself");
                let now = painted_primitives(&mut cx, window);
                assert_ne!(
                    now, painted,
                    "{name}: frame {n} painted what frame {n} - 1 did"
                );
                painted = now;
            }
        }
    }
}
