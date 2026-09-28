//! `LayoutStats`: counters and timings describing the work a frame did, for benchmarks and profiling.

use crate::{TaffyLayoutEngine, Window};
use std::time::{Duration, Instant};

/// Counters describing the work the layout engine performed, for benchmarking
/// and profiling.
///
/// Counts accumulate across frames until [`TaffyLayoutEngine::reset_stats`] is
/// called; clearing the tree between frames does not reset them. Counting is
/// cheap enough to leave enabled in release builds: a few integer increments per
/// node. The times that would take a clock read per measurement or per shaped
/// line are kept only once [`TaffyLayoutEngine::reset_stats`] has been called,
/// which is how a benchmark asks for them, and are zero until then.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutStats {
    /// Frames laid out, counted once per `Window::draw`.
    pub frames: u64,
    /// Taffy nodes allocated.
    pub nodes_created: u64,
    /// Taffy nodes carried over from an earlier frame rather than allocated.
    pub nodes_reused: u64,
    /// Taffy nodes released back to the tree.
    pub nodes_freed: u64,
    /// Styles compared against the style a reused node already had.
    pub style_compares: u64,
    /// Styles written to a node. Each write dirties the node and its ancestors.
    pub style_writes: u64,
    /// Child lists written to a node. Each write dirties the node and its ancestors.
    pub children_writes: u64,
    /// Measure closures bound onto a node in a way that dirties it.
    pub measure_rebinds: u64,
    /// Measured nodes whose element took over last frame's measurement, and
    /// were left clean rather than measured again.
    pub measurements_kept: u64,
    /// Times Taffy actually invoked a measurement. A node can be measured more
    /// than once in a layout — for its intrinsic size and then for its final
    /// one — so this runs ahead of the number of measured nodes.
    pub measure_calls: u64,
    /// Time spent inside those measurements, which is time `compute_layout_time`
    /// also counts. The difference between the two is Taffy's own solving.
    /// Kept only once the stats have been reset.
    pub measure_time: Duration,
    /// Calls to [`TaffyLayoutEngine::compute_layout`].
    pub compute_layout_calls: u64,
    /// Time spent building the element tree: rendering every view and
    /// registering the nodes their elements ask for.
    pub build_time: Duration,
    /// Time spent in the prepaint walk, which is where layout is computed and
    /// where elements decide their bounds, hitboxes and dispatch nodes.
    /// `compute_layout_time` is part of this.
    pub prepaint_time: Duration,
    /// Time spent in the paint walk, turning laid-out elements into the scene.
    pub paint_time: Duration,
    /// Time spent inside Taffy's own layout computation. Kept only once the
    /// stats have been reset.
    pub compute_layout_time: Duration,
    /// Lines of text handed to the platform to be shaped. A line the text
    /// cache still held from this frame or the last one is not counted, so this
    /// is the shaping the cache did not save.
    pub lines_shaped: u64,
    /// Time spent shaping those lines. Shaping done while measuring is also
    /// part of `measure_time`; shaping done while painting is part of
    /// `paint_time`. Kept only once the stats have been reset.
    pub shape_time: Duration,
    /// Views and cached views built, whether rendered, or laid out again
    /// where they moved to.
    pub views_built: u64,
    /// Views and cached views drawn again from what they drew on the last
    /// frame, without being built.
    pub views_reused: u64,
}

/// How long each phase of the frame took, waiting to be folded into the
/// layout engine's statistics. Kept on the window because the phases are
/// driven from the window, not from the engine.
#[derive(Default)]
pub(crate) struct FramePhaseTimes {
    build: Duration,
    prepaint: Duration,
    paint: Duration,
    /// When the phase being timed began.
    phase_started_at: Option<scheduler::Instant>,
}

impl FramePhaseTimes {
    /// Starts timing the first phase of a frame.
    pub(crate) fn begin(&mut self) {
        self.phase_started_at = Some(scheduler::Instant::now());
    }

    /// Ends the build phase, which [`LayoutStats::build_time`] reports, and
    /// starts timing the prepaint phase.
    pub(crate) fn end_build(&mut self) {
        let lap = self.lap();
        self.build += lap;
    }

    /// Ends the prepaint phase, which [`LayoutStats::prepaint_time`]
    /// reports, and starts timing the paint phase.
    pub(crate) fn end_prepaint(&mut self) {
        let lap = self.lap();
        self.prepaint += lap;
    }

    /// Ends the paint phase, which [`LayoutStats::paint_time`] reports.
    pub(crate) fn end_paint(&mut self) {
        let lap = self.lap();
        self.paint += lap;
    }

    /// The time since the phase being timed began, starting the next one.
    fn lap(&mut self) -> Duration {
        let now = scheduler::Instant::now();
        let started_at = self.phase_started_at.replace(now).unwrap_or(now);
        now - started_at
    }

    /// Zeroes the times, leaving a phase being timed to go on being timed.
    #[cfg(any(test, feature = "test-support"))]
    fn reset(&mut self) {
        self.build = Duration::ZERO;
        self.prepaint = Duration::ZERO;
        self.paint = Duration::ZERO;
    }
}

/// Counts and times the measurements Taffy makes during one call to
/// [`TaffyLayoutEngine::compute_layout`]. Accumulated outside the engine,
/// because the measurement closure borrows the tree.
pub(crate) struct MeasureTally {
    timed: bool,
    calls: u64,
    time: Duration,
    compute_started_at: Option<Instant>,
}

impl MeasureTally {
    /// Starts timing one measurement, if measurements are timed.
    pub(crate) fn start(&self) -> Option<Instant> {
        self.timed.then(Instant::now)
    }

    /// Counts a measurement started by [`Self::start`].
    pub(crate) fn finish(&mut self, started_at: Option<Instant>) {
        self.calls += 1;
        if let Some(started_at) = started_at {
            self.time += started_at.elapsed();
        }
    }
}

impl TaffyLayoutEngine {
    /// Counters for the work performed since the last call to [`Self::reset_stats`].
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn stats(&self) -> LayoutStats {
        self.retention.stats
    }

    /// Zeroes the counters returned by [`Self::stats`].
    /// From then on the times are kept too.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn reset_stats(&mut self) {
        self.retention.stats = LayoutStats::default();
        self.retention.timed = true;
    }

    /// Starts counting the measurements of a layout computation.
    pub(crate) fn begin_measure_tally(&self) -> MeasureTally {
        let timed = self.retention.timed;
        MeasureTally {
            timed,
            calls: 0,
            time: Duration::ZERO,
            compute_started_at: timed.then(Instant::now),
        }
    }

    /// Folds what a layout computation measured into [`Self::stats`].
    pub(crate) fn finish_measure_tally(&mut self, tally: MeasureTally) {
        let stats = &mut self.retention.stats;
        stats.compute_layout_calls += 1;
        if let Some(started_at) = tally.compute_started_at {
            stats.compute_layout_time += started_at.elapsed();
        }
        stats.measure_calls += tally.calls;
        stats.measure_time += tally.time;
    }
}

impl Window {
    /// Counters describing the work the layout engine has performed since the
    /// last call to [`Window::reset_layout_stats`].
    ///
    /// Useful for confirming that a change actually reduced layout work rather
    /// than only moving it around: `nodes_reused` against `nodes_created` shows
    /// how much of the tree survived the frame, and `style_writes` shows how
    /// much of it was dirtied again anyway.
    #[cfg(any(test, feature = "test-support"))]
    pub fn layout_stats(&self) -> LayoutStats {
        let phases = &self.fast_layout.phase_times;
        let (lines_shaped, shape_time) = self.text_system().shaping_stats();
        LayoutStats {
            build_time: phases.build,
            prepaint_time: phases.prepaint,
            paint_time: phases.paint,
            lines_shaped,
            shape_time,
            ..self.layout_engine.as_ref().unwrap().stats()
        }
    }

    /// Zeroes the counters reported by [`Window::layout_stats`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn reset_layout_stats(&mut self) {
        self.fast_layout.phase_times.reset();
        self.layout_engine.as_mut().unwrap().reset_stats();
        self.text_system().reset_shaping_stats();
    }

    /// How many layout nodes this window is currently holding on to.
    #[cfg(any(test, feature = "test-support"))]
    pub fn layout_node_count(&self) -> usize {
        self.layout_engine.as_ref().unwrap().node_count()
    }
}
