//! gpui-fast's performance tools.
//!
//! With no arguments it opens the showcase, a component gallery that scrolls
//! itself and shows what each frame costs; see `showcase.rs`:
//!
//! ```text
//! cargo run -p gpui_perf --release
//! cargo run -p gpui_perf --release -- --auto
//! ```
//!
//! With the `upstream` feature the showcase runs on upstream GPUI instead, the
//! `gpui-pre` snapshot GPUI Kit pins, to compare the two:
//!
//! ```text
//! cargo run -p gpui_perf --release --features upstream -- --auto
//! ```
//!
//! With `--headless` it measures what simulated application screens cost to
//! draw, frame by frame, with and without retained views, without a window:
//!
//! ```text
//! cargo run -p gpui_perf --release -- --headless --frames 200
//! ```
//!
//! Showcase flags:
//!
//! - `--demo`: scroll the sidebar, a page, the table and the list in turn,
//!   for as long as it is open, to watch or record two GPUIs side by side.
//! - `--auto`: run every scenario with retention on and off, print what each
//!   cost, and quit.
//! - `--only <scenario>`, `--retention on|off`, `--frames N`: narrow `--auto`
//!   down.
//! - `--no-hold-clock`: on macOS, measure `--auto` without holding the CPU's
//!   clock up; see `showcase/clock.rs`.
//!
//! Headless flags:
//!
//! - `--scenario <substring>`: run only scenarios whose name contains it;
//!   repeatable.
//! - `--frames N`: frames measured per run (default 200).
//! - `--warmup N`: frames drawn before measuring (default 30).
//! - `--retention on|off|both`: which modes to run (default both).
//! - `--json PATH`: also write every result as JSON.
//! - `--verify`: also run both modes in lockstep and check they paint the
//!   same quads every frame.
//! - `--list`: print the scenarios and exit.

// The GPUI the showcase runs on, named `gpui` and `gpui_platform` either way:
// upstream's `gpui-pre` snapshot with the `upstream` feature, this
// repository's otherwise.
#[cfg(all(feature = "fast", not(feature = "upstream")))]
extern crate gpui_fast as gpui;
#[cfg(all(feature = "fast", not(feature = "upstream")))]
extern crate gpui_platform_fast as gpui_platform;
#[cfg(feature = "upstream")]
extern crate gpui_pre as gpui;
#[cfg(feature = "upstream")]
extern crate gpui_pre_platform as gpui_platform;
#[cfg(not(any(feature = "fast", feature = "upstream")))]
compile_error!("gpui_perf needs the `fast` feature (the default) or `upstream`");

mod showcase;

use std::process::ExitCode;

use gpui_perf::alloc::CountingAllocator;
#[cfg(not(feature = "upstream"))]
use gpui_perf::runner::{self, Options, RetentionModes};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

const USAGE: &str = "usage: gpui_perf [--demo | --auto [--only SCENARIO] [--retention on|off] [--frames N] [--no-hold-clock]]\n       \
gpui_perf --headless [--scenario SUBSTRING]... [--frames N] [--warmup N] \
[--retention on|off|both] [--json PATH] [--verify] [--list]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args
        .iter()
        .any(|arg| arg == showcase::clock::HOLD_CLOCK_FLAG)
    {
        showcase::clock::hold();
    }
    if !args.iter().any(|arg| arg == "--headless") {
        showcase::run(
            args.iter().any(|arg| arg == "--auto"),
            args.iter().any(|arg| arg == "--demo"),
        );
        return ExitCode::SUCCESS;
    }
    headless()
}

#[cfg(feature = "upstream")]
fn headless() -> ExitCode {
    eprintln!(
        "--headless measures gpui-fast's own counters and runs only on gpui-fast; \
         build without the `upstream` feature"
    );
    ExitCode::from(2)
}

#[cfg(not(feature = "upstream"))]
fn headless() -> ExitCode {
    let mut options = Options::default();
    let mut json_path = None;
    let mut list = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next().unwrap_or_else(|| {
                eprintln!("{name} needs a value\n{USAGE}");
                std::process::exit(2);
            })
        };
        let number = |name: &str, value: String| {
            value.parse::<usize>().unwrap_or_else(|_| {
                eprintln!("{name} needs a number, got {value:?}");
                std::process::exit(2);
            })
        };
        match arg.as_str() {
            "--scenario" => options.filters.push(value("--scenario")),
            "--frames" => options.frames = number("--frames", value("--frames")),
            "--warmup" => options.warmup = number("--warmup", value("--warmup")),
            "--retention" => {
                options.retention = match value("--retention").as_str() {
                    "on" => RetentionModes::On,
                    "off" => RetentionModes::Off,
                    "both" => RetentionModes::Both,
                    other => {
                        eprintln!("--retention takes on, off or both, got {other:?}");
                        return ExitCode::from(2);
                    }
                }
            }
            "--json" => json_path = Some(value("--json")),
            "--verify" => options.verify = true,
            "--list" => list = true,
            "--headless" => {}
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument {other:?}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    let selected = runner::selected_scenarios(&options);
    if list {
        for (_, name, description) in &selected {
            println!("{name:<32}{description}");
        }
        return ExitCode::SUCCESS;
    }
    if selected.is_empty() {
        eprintln!("no scenario matches");
        return ExitCode::FAILURE;
    }

    let reports = runner::run(&options);
    print!("{}", runner::format_reports(&reports));

    if let Some(path) = json_path {
        if let Err(error) = std::fs::write(&path, runner::to_json(&options, &reports)) {
            eprintln!("failed to write {path}: {error}");
            return ExitCode::FAILURE;
        }
        eprintln!("wrote {path}");
    }

    if reports
        .iter()
        .any(|report| report.verify.as_ref().is_some_and(|verify| !verify.passed))
    {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
