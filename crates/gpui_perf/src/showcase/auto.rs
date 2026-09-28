//! `--auto`: every scenario with retained views on and then off, measured over
//! a fixed number of frames, then a report. Upstream GPUI has no retained
//! views, so built with the `upstream` feature it runs every scenario once.

use std::time::Duration;

use std::cell::RefCell;

use gpui::{App, Window};

use super::{
    BUTTON_PAGE, Driver, Handles, Scroll, backend, list_page,
    metrics::{Cost, Sample, main_thread_cpu_time},
    table_page,
};

#[derive(Clone, Copy, Debug)]
enum Scenario {
    Idle,
    ScrollSidebar,
    ScrollPage,
    ScrollTable,
    RefreshTable,
    ScrollList,
}

const SCENARIOS: [Scenario; 6] = [
    Scenario::Idle,
    Scenario::ScrollSidebar,
    Scenario::ScrollPage,
    Scenario::ScrollTable,
    Scenario::RefreshTable,
    Scenario::ScrollList,
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
    results: Vec<Result>,
}

impl AutoRun {
    /// Every scenario with retention on, then off; `--only <scenario>` and
    /// `--retention on|off` narrow that down, and `--frames <n>` sets how
    /// many frames each is measured over.
    pub fn new() -> Self {
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
            .collect();
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
            results: Vec::new(),
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
            self.report();
            cx.quit();
            return false;
        };
        let idle = matches!(scenario, Scenario::Idle | Scenario::RefreshTable);
        if self.frame == 0 {
            if let Some(retention) = retention {
                backend::set_view_retention(window, retention);
            }
            let (page, scroll) = match scenario {
                Scenario::Idle => (BUTTON_PAGE, Scroll::Off),
                Scenario::ScrollSidebar => (BUTTON_PAGE, Scroll::Sidebar),
                Scenario::ScrollPage => (BUTTON_PAGE, Scroll::Page),
                Scenario::ScrollTable => (table_page(), Scroll::Table),
                Scenario::ScrollList => (list_page(), Scroll::List),
                Scenario::RefreshTable => (table_page(), Scroll::Off),
            };
            driver.borrow_mut().scroll = scroll;
            let refresh = matches!(scenario, Scenario::RefreshTable);
            handles
                .showcase
                .update(cx, |showcase, cx| {
                    showcase.scroll = scroll;
                    // Toggling the refresh shows the table, so it goes first.
                    if showcase.refreshing != refresh {
                        showcase.toggle_refresh(window, cx);
                    }
                    showcase.select(page, cx);
                })
                .ok();
        }
        if idle {
            // Something has to ask for frames when nothing scrolls; a
            // notified status bar is the least a frame can do.
            cx.notify(handles.status_bar.entity_id());
        }

        let cpu = main_thread_cpu_time();
        if self.frame == WARMUP_FRAMES {
            self.started = Some(Sample::take(window));
            self.frame_cpu.clear();
        } else if self.frame > WARMUP_FRAMES
            && let Some(last) = self.last_cpu
        {
            self.frame_cpu.push((cpu - last).as_secs_f64() * 1e3);
        }
        self.last_cpu = Some(cpu);

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
            self.results.push(Result {
                scenario,
                retention,
                cost,
                p50: percentile(0.5),
                p95: percentile(0.95),
            });
            self.index += 1;
            self.frame = 0;
            self.last_cpu = None;
        }
        true
    }

    fn report(&self) {
        println!(
            "\n{:<16} {:>9} {:>6} {:>9} {:>9} {:>7} {:>8} {:>8} {:>9} {:>8} {:>8} {:>6} {:>6}",
            "scenario",
            "retention",
            "fps",
            "cpu p50",
            "cpu p95",
            "proc",
            "memory",
            "build",
            "prepaint",
            "layout",
            "paint",
            "built",
            "reused"
        );
        let ms = |value: Option<f64>| value.map_or("-".to_string(), |v| format!("{v:.2}ms"));
        let count = |value: Option<f64>| value.map_or("-".to_string(), |v| format!("{v:.1}"));
        for result in &self.results {
            let cost = &result.cost;
            let phases = cost.phases;
            println!(
                "{:<16} {:>9} {:>6.0} {:>7.2}ms {:>7.2}ms {:>6.0}% {:>8} {:>8} {:>9} {:>8} {:>8} {:>6} {:>6}",
                format!("{:?}", result.scenario),
                match result.retention {
                    Some(true) => "on",
                    Some(false) => "off",
                    None => "upstream",
                },
                cost.fps,
                result.p50,
                result.p95,
                cost.process_cpu_percent,
                cost.memory_mib
                    .map_or("-".to_string(), |mib| format!("{mib:.0}MB")),
                ms(phases.map(|p| p.build_ms)),
                ms(phases.map(|p| p.prepaint_ms)),
                ms(phases.map(|p| p.layout_ms)),
                ms(phases.map(|p| p.paint_ms)),
                count(phases.map(|p| p.views_built)),
                count(phases.map(|p| p.views_reused)),
            );
        }
        println!(
            "\ncpu p50/p95: main thread CPU per frame. proc: the whole process, render threads \
             included. memory: the process's resident memory at the end. build, prepaint, paint: per frame; layout is Taffy's share of prepaint. \
             built, reused: views per frame. \"-\": not counted by upstream GPUI."
        );
    }
}
