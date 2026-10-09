//! What differs between the two GPUIs the showcase runs on: this
//! repository's, and upstream's `gpui-pre` snapshot with the `upstream`
//! feature. Upstream has neither gpui-fast's frame counters nor retained views
//! to switch, so it counts frames itself, and reports only what the process's
//! CPU clocks say.

use std::time::Duration;

pub use imp::{
    GPUI, UPSTREAM, enable_composition, frame_counter, frame_times, frames, reset_stats,
    set_view_retention, view_retention,
};

/// What gpui-fast's counters say the frames drawn so far took.
#[derive(Clone, Copy, Default)]
pub struct FrameTimes {
    pub build: Duration,
    pub prepaint: Duration,
    pub layout: Duration,
    pub paint: Duration,
    /// Handing frames to the platform, which splits them per surface once
    /// the window composes.
    pub present: Duration,
    pub views_built: u64,
    pub views_reused: u64,
    /// Scroll containers drawn from a scroll layer's cached tiles.
    pub layer_frames_composited: u64,
    /// Scroll layer tiles repaints changed.
    pub tiles_dirtied: u64,
    /// Scroll layers painted again before an input event.
    pub layer_rebuilds_for_input: u64,
}

#[cfg(not(feature = "upstream"))]
mod imp {
    use std::any::Any;

    use gpui::{AnyElement, Bounds, Pixels, Window};

    use super::FrameTimes;

    /// The GPUI the showcase was built against, as the toolbar names it.
    pub const GPUI: &str = "GPUI Fast with Retained Mode";

    /// Whether it is upstream GPUI, which the toolbar marks in red rather
    /// than green.
    pub const UPSTREAM: bool = false;

    /// Starts gpui-fast's counters, which time the phases only once reset.
    pub fn reset_stats(window: &mut Window) {
        window.reset_layout_stats();
    }

    /// How many frames the window has drawn.
    pub fn frames(window: &Window) -> u64 {
        window.layout_stats().frames
    }

    /// What the frames drawn so far took, where GPUI counts it.
    pub fn frame_times(window: &Window) -> Option<FrameTimes> {
        let stats = window.layout_stats();
        Some(FrameTimes {
            build: stats.build_time,
            prepaint: stats.prepaint_time,
            layout: stats.compute_layout_time,
            paint: stats.paint_time,
            present: stats.present_time,
            views_built: stats.views_built,
            views_reused: stats.views_reused,
            layer_frames_composited: stats.layer_frames_composited,
            tiles_dirtied: stats.tiles_dirtied,
            layer_rebuilds_for_input: stats.layer_rebuilds_for_input,
        })
    }

    /// Whether retained views are on, where GPUI has them.
    pub fn view_retention(window: &Window) -> Option<bool> {
        Some(window.view_retention())
    }

    /// Turns retained views on or off, where GPUI has them.
    pub fn set_view_retention(window: &mut Window, enabled: bool) {
        window.set_view_retention(enabled);
    }

    /// Composes the window as one embedding a webview does: an empty native
    /// surface at `bounds` between GPUI's content and its overlays. Returns
    /// what keeps the surface, to be held as long as the window.
    pub fn enable_composition(
        window: &mut Window,
        bounds: Bounds<Pixels>,
    ) -> Result<Box<dyn Any>, String> {
        let bounds = bounds.to_device_pixels(window.scale_factor());
        let composition = window
            .enable_window_composition()
            .map_err(|error| format!("{error:#}"))?;
        let surface = composition
            .create_native_surface()
            .map_err(|error| format!("{error:#}"))?;
        surface
            .platform_surface()
            .and_then(|platform_surface| platform_surface.set_bounds(bounds))
            .map_err(|error| format!("{error:#}"))?;
        Ok(Box::new(surface))
    }

    /// An element counting the frames drawn, where GPUI does not count them:
    /// gpui-fast does.
    pub fn frame_counter() -> Option<AnyElement> {
        None
    }
}

#[cfg(feature = "upstream")]
mod imp {
    use std::{
        any::Any,
        sync::atomic::{AtomicU64, Ordering},
    };

    use gpui::{AnyElement, Bounds, IntoElement as _, Pixels, Styled as _, Window, canvas};

    use super::FrameTimes;

    /// The GPUI the showcase was built against, as the toolbar names it.
    pub const GPUI: &str = "GPUI Upstream";

    /// Whether it is upstream GPUI, which the toolbar marks in red rather
    /// than green.
    pub const UPSTREAM: bool = true;

    /// Frames drawn, counted by [`frame_counter`].
    static FRAMES: AtomicU64 = AtomicU64::new(0);

    /// Upstream has no counters to start.
    pub fn reset_stats(_: &mut Window) {}

    /// How many frames the window has drawn.
    pub fn frames(_: &Window) -> u64 {
        FRAMES.load(Ordering::Relaxed)
    }

    /// Upstream does not time the phases of a frame.
    pub fn frame_times(_: &Window) -> Option<FrameTimes> {
        None
    }

    /// Upstream has no retained views.
    pub fn view_retention(_: &Window) -> Option<bool> {
        None
    }

    /// Upstream has no retained views to switch.
    pub fn set_view_retention(_: &mut Window, _: bool) {}

    /// Upstream has no window composition.
    pub fn enable_composition(_: &mut Window, _: Bounds<Pixels>) -> Result<Box<dyn Any>, String> {
        Err("upstream GPUI has no window composition".into())
    }

    /// An element for the root view that counts the frames drawn. Upstream
    /// builds the whole window every frame it draws, so an element in the
    /// root view is painted exactly once per frame.
    pub fn frame_counter() -> Option<AnyElement> {
        Some(
            canvas(
                |_, _, _| (),
                |_, _, _, _| {
                    FRAMES.fetch_add(1, Ordering::Relaxed);
                },
            )
            .absolute()
            .size_0()
            .into_any_element(),
        )
    }
}
