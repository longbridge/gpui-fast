//! `--auto`: every scenario with retained views on and then off, measured over
//! a fixed number of frames, then a report.

use std::time::Duration;

use gpui::{Context, Window};

use super::{
    BUTTON_PAGE, Scroll, Showcase, list_page,
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
    retention: bool,
    cost: Cost,
    p50: f64,
    p95: f64,
}

pub struct AutoRun {
    /// The scenarios to run, each with retention on or off.
    runs: Vec<(Scenario, bool)>,
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
        let runs = [true, false]
            .into_iter()
            .filter(|on| {
                retention
                    .as_deref()
                    .is_none_or(|retention| (retention == "on") == *on)
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
        showcase: &mut Showcase,
        window: &mut Window,
        cx: &mut Context<Showcase>,
    ) -> bool {
        let Some(&(scenario, retention)) = self.runs.get(self.index) else {
            self.report();
            cx.quit();
            return false;
        };
        let idle = matches!(scenario, Scenario::Idle | Scenario::RefreshTable);
        if self.frame == 0 {
            window.set_view_retention(retention);
            showcase.scroll = Scroll::Off;
            showcase.container.update(cx, |container, cx| {
                if container.refreshing {
                    container.toggle_refresh(window, cx);
                }
            });
            match scenario {
                Scenario::Idle => showcase.select(BUTTON_PAGE, cx),
                Scenario::ScrollSidebar => {
                    showcase.select(BUTTON_PAGE, cx);
                    showcase.scroll = Scroll::Sidebar;
                }
                Scenario::ScrollPage => {
                    showcase.select(BUTTON_PAGE, cx);
                    showcase.scroll = Scroll::Page;
                }
                Scenario::ScrollTable => {
                    showcase.select(table_page(), cx);
                    showcase.scroll = Scroll::Table;
                }
                Scenario::ScrollList => {
                    showcase.select(list_page(), cx);
                    showcase.scroll = Scroll::List;
                }
                Scenario::RefreshTable => {
                    showcase.select(table_page(), cx);
                    showcase
                        .container
                        .update(cx, |container, cx| container.toggle_refresh(window, cx));
                }
            }
        }
        if idle {
            // Something has to ask for frames when nothing scrolls; a
            // notified status bar is the least a frame can do.
            showcase.status_bar.update(cx, |_, cx| cx.notify());
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
            "\n{:<16} {:>9} {:>6} {:>9} {:>9} {:>7} {:>8} {:>9} {:>8} {:>8} {:>6} {:>6}",
            "scenario",
            "retention",
            "fps",
            "cpu p50",
            "cpu p95",
            "proc",
            "build",
            "prepaint",
            "layout",
            "paint",
            "built",
            "reused"
        );
        for result in &self.results {
            let cost = &result.cost;
            println!(
                "{:<16} {:>9} {:>6.0} {:>7.2}ms {:>7.2}ms {:>6.0}% {:>6.2}ms {:>7.2}ms {:>6.2}ms {:>6.2}ms {:>6.1} {:>6.1}",
                format!("{:?}", result.scenario),
                if result.retention { "on" } else { "off" },
                cost.fps,
                result.p50,
                result.p95,
                cost.process_cpu_percent,
                cost.build_ms,
                cost.prepaint_ms,
                cost.layout_ms,
                cost.paint_ms,
                cost.views_built,
                cost.views_reused,
            );
        }
        println!(
            "\ncpu p50/p95: main thread CPU per frame. proc: the whole process, render threads \
             included. build, prepaint, paint: per frame; layout is Taffy's share of prepaint. \
             built, reused: views per frame."
        );
    }
}
