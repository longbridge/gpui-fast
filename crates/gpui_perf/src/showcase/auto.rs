//! `--auto`: every scenario with retained views on and then off, measured over
//! a fixed number of frames, then a report. Upstream GPUI has no retained
//! views, so built with the `upstream` feature it runs every scenario once.
//!
//! The trading workspace's scenarios stream quotes into it while the user
//! does nothing, scrolls the watchlist, or moves the pointer over its rows.
//!
//! A window cannot stop composing once it has started, so what window
//! composition costs is measured by two runs, one with `--composition`.

use std::time::Duration;

use std::cell::RefCell;

use gpui::{App, MouseMoveEvent, PlatformInput, Window, point, px};

use super::{
    BUTTON_PAGE, Driver, Handles, Scroll, backend,
    clock::ClockHold,
    list_page,
    metrics::{Cost, Sample, main_thread_cpu_time, main_thread_instructions},
    table_page,
    workspace::{QUOTES_PER_TICK, WATCHLIST_ROW_HEIGHT},
    workspace_page,
};

#[derive(Clone, Copy, Debug)]
enum Scenario {
    /// Nothing scrolls; a spinner in the toolbar animates every frame.
    Spinner,
    ScrollSidebar,
    ScrollPage,
    ScrollTable,
    RefreshTable,
    ScrollList,
    /// The trading workspace at rest while quotes stream in.
    WorkspaceQuotes,
    /// The workspace's watchlist scrolling while quotes stream in.
    WorkspaceScroll,
    /// The pointer moving over the workspace's watchlist rows while quotes
    /// stream in.
    WorkspaceHover,
}

impl Scenario {
    /// What the scenario shows, for `--list`.
    fn description(self) -> &'static str {
        match self {
            Scenario::Spinner => "idle: nothing scrolls; a spinner in the toolbar animates",
            Scenario::ScrollSidebar => "scrolling the sidebar",
            Scenario::ScrollPage => "scrolling a page of components",
            Scenario::ScrollTable => "scrolling the data table",
            Scenario::RefreshTable => "refreshing the data table's rows every 33 ms",
            Scenario::ScrollList => "scrolling a list of messages",
            Scenario::WorkspaceQuotes => {
                "workspace: quotes streaming, 16 every 16 ms, to every panel"
            }
            Scenario::WorkspaceScroll => {
                "workspace: scrolling the watchlist, with 8 quotes every 16 ms"
            }
            Scenario::WorkspaceHover => {
                "workspace: hovering rows of the watchlist, with 8 quotes every 16 ms"
            }
        }
    }

    fn is_workspace(self) -> bool {
        matches!(
            self,
            Scenario::WorkspaceQuotes | Scenario::WorkspaceScroll | Scenario::WorkspaceHover
        )
    }
}

/// Prints every scenario `--only` can pick.
pub fn list() {
    for scenario in SCENARIOS {
        println!("{:<16} {}", format!("{scenario:?}"), scenario.description());
    }
}

const SCENARIOS: [Scenario; 9] = [
    Scenario::Spinner,
    Scenario::ScrollSidebar,
    Scenario::ScrollPage,
    Scenario::ScrollTable,
    Scenario::RefreshTable,
    Scenario::ScrollList,
    Scenario::WorkspaceQuotes,
    Scenario::WorkspaceScroll,
    Scenario::WorkspaceHover,
];

const WARMUP_FRAMES: usize = 30;

/// A command-line flag's value.
fn flag(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|arg| arg == name)
        .and_then(|ix| args.get(ix + 1))
        .cloned()
}

struct Result {
    scenario: Scenario,
    retention: Option<bool>,
    cost: Cost,
    p50: f64,
    p95: f64,
    /// Main-thread instructions per frame, the median, where the platform
    /// counts them.
    instructions_p50: Option<f64>,
}

pub struct AutoRun {
    /// The scenarios to run, each with retention on or off, or neither
    /// where GPUI has no retained views.
    runs: Vec<(Scenario, Option<bool>)>,
    measured_frames: usize,
    index: usize,
    frame: usize,
    started: Option<Sample>,
    frame_cpu: Vec<f64>,
    last_cpu: Option<Duration>,
    frame_instructions: Vec<u64>,
    last_instructions: Option<u64>,
    results: Vec<Result>,
    /// Holds the CPU's clock up while the scenarios run.
    clock: ClockHold,
    /// Whether the window composes around a native surface, which the
    /// report says.
    composition: bool,
}

impl AutoRun {
    /// Every scenario with retention on, then off; `--only <scenario>` and
    /// `--retention on|off` narrow that down, and `--frames <n>` sets how
    /// many frames each is measured over. `composition` is whether the
    /// window composes, for the report.
    pub fn new(composition: bool) -> Self {
        let only = flag("--only").map(|only| only.to_lowercase());
        let retention = flag("--retention").map(|retention| retention.to_lowercase());
        let modes: &[Option<bool>] = if cfg!(feature = "upstream") {
            &[None]
        } else {
            &[Some(true), Some(false)]
        };
        let runs = modes
            .iter()
            .copied()
            .filter(|on| {
                on.is_none_or(|on| {
                    retention
                        .as_deref()
                        .is_none_or(|retention| (retention == "on") == on)
                })
            })
            .flat_map(|on| SCENARIOS.map(|scenario| (scenario, on)))
            .filter(|(scenario, _)| {
                only.as_deref()
                    .is_none_or(|only| format!("{scenario:?}").to_lowercase() == only)
            })
            .collect::<Vec<_>>();
        if runs.is_empty() {
            eprintln!("no scenario matches; `--auto --list` prints them");
        }
        Self {
            runs,
            measured_frames: flag("--frames")
                .and_then(|frames| frames.parse().ok())
                .unwrap_or(240),
            index: 0,
            frame: 0,
            started: None,
            frame_cpu: Vec::new(),
            last_cpu: None,
            frame_instructions: Vec::new(),
            last_instructions: None,
            results: Vec::new(),
            clock: ClockHold::start(),
            composition,
        }
    }

    /// Runs before each frame. Returns whether to keep asking for frames.
    pub fn step(
        &mut self,
        driver: &RefCell<Driver>,
        handles: &Handles,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let Some(&(scenario, retention)) = self.runs.get(self.index) else {
            // The platform window holds the focused search box's input
            // handler until a frame is drawn without it, and on Wayland a
            // closed window's state, handler included, is only dropped by a
            // task that never runs once the app quits. So the search box is
            // blurred and one more frame drawn, which takes the handler back,
            // before the window is closed and the app checks for leaked
            // entities as it quits.
            if window.focused(cx).is_some() {
                window.blur(cx);
                return true;
            }
            let clock_held = self.clock.is_holding();
            self.clock.stop();
            self.report(clock_held);
            window.remove_window();
            cx.quit();
            return false;
        };
        if self.frame == 0 {
            if let Some(retention) = retention {
                backend::set_view_retention(window, retention);
            }
            let (page, scroll) = match scenario {
                Scenario::Spinner => (BUTTON_PAGE, Scroll::Off),
                Scenario::ScrollSidebar => (BUTTON_PAGE, Scroll::Sidebar),
                Scenario::ScrollPage => (BUTTON_PAGE, Scroll::Page),
                Scenario::ScrollTable => (table_page(), Scroll::Table),
                Scenario::ScrollList => (list_page(), Scroll::List),
                Scenario::RefreshTable => (table_page(), Scroll::Off),
                Scenario::WorkspaceQuotes | Scenario::WorkspaceHover => {
                    (workspace_page(), Scroll::Off)
                }
                Scenario::WorkspaceScroll => (workspace_page(), Scroll::Watchlist),
            };
            driver.borrow_mut().scroll = scroll;
            let refresh = matches!(scenario, Scenario::RefreshTable);
            let streaming = scenario.is_workspace();
            handles
                .showcase
                .update(cx, |showcase, cx| {
                    showcase.scroll = scroll;
                    showcase.spinning = matches!(scenario, Scenario::Spinner);
                    // Toggling the refresh shows the table, so it goes first.
                    if showcase.container.read(cx).refreshing != refresh {
                        showcase.toggle_refresh(window, cx);
                    }
                    // So does toggling the quote stream, the workspace.
                    if showcase.container.read(cx).streaming != streaming {
                        showcase.toggle_streaming(window, cx);
                    }
                    if let Some(workspace) = showcase.container.read(cx).workspace.clone() {
                        let quotes = match scenario {
                            Scenario::WorkspaceScroll | Scenario::WorkspaceHover => {
                                QUOTES_PER_TICK / 2
                            }
                            _ => QUOTES_PER_TICK,
                        };
                        workspace.update(cx, |workspace, _| workspace.quotes_per_tick = quotes);
                    }
                    showcase.select(page, cx);
                    showcase.focus_page(window, cx);
                })
                .ok();
        }

        if matches!(scenario, Scenario::WorkspaceHover) {
            hover_watchlist(self.frame, handles, window, cx);
        }

        let cpu = main_thread_cpu_time();
        let instructions = main_thread_instructions();
        if self.frame == WARMUP_FRAMES {
            self.started = Some(Sample::take(window));
            self.frame_cpu.clear();
            self.frame_instructions.clear();
        } else if self.frame > WARMUP_FRAMES {
            if let Some(last) = self.last_cpu {
                self.frame_cpu.push((cpu - last).as_secs_f64() * 1e3);
            }
            if let Some((last, now)) = self.last_instructions.zip(instructions) {
                self.frame_instructions.push(now - last);
            }
        }
        self.last_cpu = Some(cpu);
        self.last_instructions = instructions;

        self.frame += 1;
        if self.frame > WARMUP_FRAMES + self.measured_frames {
            let cost = Cost::between(self.started.as_ref().unwrap(), &Sample::take(window));
            let mut frame_cpu = std::mem::take(&mut self.frame_cpu);
            frame_cpu.sort_by(f64::total_cmp);
            let percentile = |p: f64| {
                frame_cpu
                    .get(((frame_cpu.len() as f64 - 1.) * p).round() as usize)
                    .copied()
                    .unwrap_or(0.)
            };
            let mut frame_instructions = std::mem::take(&mut self.frame_instructions);
            frame_instructions.sort_unstable();
            self.results.push(Result {
                scenario,
                retention,
                cost,
                p50: percentile(0.5),
                p95: percentile(0.95),
                instructions_p50: frame_instructions
                    .get(frame_instructions.len() / 2)
                    .map(|&n| n as f64),
            });
            self.index += 1;
            self.frame = 0;
            self.last_cpu = None;
            self.last_instructions = None;
        }
        true
    }

    fn report(&self, clock_held: bool) {
        if self.results.is_empty() {
            return;
        }
        println!(
            "\nwindow composition: {}",
            if self.composition {
                "on, around an empty native surface (--composition)"
            } else {
                "off"
            }
        );
        println!(
            "\n{:<16} {:>9} {:>6} {:>9} {:>9} {:>9} {:>7} {:>7} {:>8} {:>8} {:>9} {:>8} {:>8} {:>8} {:>6} {:>6} {:>6} {:>6} {:>8}",
            "scenario",
            "retention",
            "fps",
            "cpu p50",
            "cpu p95",
            "instr p50",
            "proc",
            "p-cores",
            "memory",
            "build",
            "prepaint",
            "layout",
            "paint",
            "present",
            "built",
            "reused",
            "layers",
            "tiles",
            "rebuilds"
        );
        let ms = |value: Option<f64>| value.map_or("-".to_string(), |v| format!("{v:.2}ms"));
        let count = |value: Option<f64>| value.map_or("-".to_string(), |v| format!("{v:.1}"));
        let count2 = |value: Option<f64>| value.map_or("-".to_string(), |v| format!("{v:.2}"));
        for result in &self.results {
            let cost = &result.cost;
            let phases = cost.phases;
            println!(
                "{:<16} {:>9} {:>6.0} {:>7.2}ms {:>7.2}ms {:>9} {:>6.0}% {:>7} {:>8} {:>8} {:>9} {:>8} {:>8} {:>8} {:>6} {:>6} {:>6} {:>6} {:>8}",
                format!("{:?}", result.scenario),
                match result.retention {
                    Some(true) => "on",
                    Some(false) => "off",
                    None => "upstream",
                },
                cost.fps,
                result.p50,
                result.p95,
                result
                    .instructions_p50
                    .map_or("-".to_string(), |n| format!("{:.1}M", n / 1e6)),
                cost.process_cpu_percent,
                cost.performance_core_percent
                    .map_or("-".to_string(), |percent| format!("{percent:.0}%")),
                cost.memory_mib
                    .map_or("-".to_string(), |mib| format!("{mib:.0}MB")),
                ms(phases.map(|p| p.build_ms)),
                ms(phases.map(|p| p.prepaint_ms)),
                ms(phases.map(|p| p.layout_ms)),
                ms(phases.map(|p| p.paint_ms)),
                ms(phases.map(|p| p.present_ms)),
                count(phases.map(|p| p.views_built)),
                count(phases.map(|p| p.views_reused)),
                count2(phases.map(|p| p.layer_frames_composited)),
                count2(phases.map(|p| p.tiles_dirtied)),
                count2(phases.map(|p| p.layer_rebuilds_for_input)),
            );
        }
        println!(
            "\ncpu p50/p95: main thread CPU per frame. instr p50: main thread instructions per \
             frame, which unlike CPU time do not depend on the core or the clock the thread got. \
             proc: the whole process, render threads included. p-cores: the share of the \
             process's CPU time on performance cores. memory: the process's memory at the end, resident on Linux, its footprint on macOS. build, prepaint, paint: per frame; layout is Taffy's share of prepaint. \
             present: handing the frame to the platform, per frame, which with window composition \
             splits it into a scene per surface; wall time, so a present that waits for the GPU \
             counts that wait. \
             built, reused: views per frame. layers: scroll containers drawn from a scroll \
             layer's cached tiles, tiles: scroll layer tiles repainted, rebuilds: scroll layers \
             painted again before an input event, per frame. \"-\": not counted by upstream GPUI."
        );
        if clock_held {
            println!(
                "Measured with a helper process holding the CPU's clock up; \
                 --no-hold-clock measures without it."
            );
        }
    }
}

/// Moves the pointer one watchlist row a frame, down the rows in view and
/// back up, as a mouse moved over a table does.
fn hover_watchlist(frame: usize, handles: &Handles, window: &mut Window, cx: &mut App) {
    let Some(workspace) = handles.container.read(cx).workspace.clone() else {
        return;
    };
    let bounds = workspace.read(cx).watchlist.read(cx).rows_bounds.get();
    let rows = (f32::from(bounds.size.height) / f32::from(WATCHLIST_ROW_HEIGHT)) as usize;
    if rows == 0 {
        return;
    }
    let row = frame % (rows * 2);
    let row = if row < rows { row } else { rows * 2 - 1 - row };
    let position = point(
        bounds.origin.x + px(120.),
        bounds.origin.y + WATCHLIST_ROW_HEIGHT * (row as f32 + 0.5),
    );
    window.dispatch_event(
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers: Default::default(),
        }),
        cx,
    );
}
