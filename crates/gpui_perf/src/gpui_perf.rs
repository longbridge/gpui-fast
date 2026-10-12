//! Simulated application scenarios for measuring what a frame costs.
//!
//! Each [`Scenario`] builds a window's worth of UI shaped like a common
//! application screen — a long form, a large list, a data table, a settings
//! page — and then, frame after frame, changes it the way a user or live data
//! would: typing into a field, toggling a checkbox, scrolling, selecting a
//! row, a value ticking. The runner in `runner.rs` drives every scenario
//! headlessly, with real text shaping, once with retained views and once
//! without, and reports what each frame cost.

// This repository's GPUI, named `gpui`. The headless scenarios measure
// gpui-fast's own counters, so they are built only against it.
#[cfg(feature = "fast")]
extern crate gpui_fast as gpui;

pub mod alloc;
pub mod instructions;
#[cfg(feature = "fast")]
pub mod runner;
#[cfg(feature = "fast")]
pub mod scenarios;

#[cfg(feature = "fast")]
use gpui::{AnyView, App, Window};

/// One simulated workload.
#[cfg(feature = "fast")]
///
/// `build` creates the root view once. `step` is then called before every
/// frame with the frame's number, and changes whatever this frame changes,
/// the way the application would: updating entities and notifying them, or
/// dispatching input events to the window. It must be deterministic, so two
/// runs draw the same frames.
pub trait Scenario {
    /// A short, unique, kebab-case name, e.g. `form-typing`.
    fn name(&self) -> &'static str;

    /// One sentence on what the scenario simulates.
    fn description(&self) -> &'static str;

    /// Builds the scenario's root view.
    fn build(&self, window: &mut Window, cx: &mut App) -> AnyView;

    /// Changes what frame `frame` changes. `root` is the view `build`
    /// returned.
    fn step(&self, root: &AnyView, frame: usize, window: &mut Window, cx: &mut App);
}

/// Every scenario, in the order they are reported.
#[cfg(feature = "fast")]
pub fn all_scenarios() -> Vec<Box<dyn Scenario>> {
    let mut scenarios = Vec::new();
    scenarios.extend(scenarios::form::scenarios());
    scenarios.extend(scenarios::list::scenarios());
    scenarios.extend(scenarios::table::scenarios());
    scenarios.extend(scenarios::settings::scenarios());
    scenarios.extend(scenarios::layout::scenarios());
    scenarios.extend(scenarios::workspace::scenarios());
    scenarios.extend(scenarios::scroll::scenarios());
    scenarios.extend(scenarios::chat::scenarios());
    scenarios.extend(scenarios::chat_patterns::scenarios());
    scenarios
}
