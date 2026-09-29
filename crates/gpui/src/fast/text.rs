//! Where a reused range of shaped lines falls in a new frame, text measurements
//! carried from one frame to the next, and shaping statistics.

use crate::{
    App, AvailableSpace, FontRun, FrameCache, LayoutId, LineLayout, LineLayoutIndex, Pixels,
    PlatformTextSystem, SharedString, Size, Style, TextLayout, TextLayoutInner, TextRun, TextStyle,
    Window, WindowTextSystem, WrappedLine,
};
use collections::FxHashMap;
use scheduler::Instant;
use std::{
    any::Any,
    hash::Hash,
    mem,
    rc::Rc,
    sync::Arc,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

/// Everything a text element's measurement is taken from, and the layout it
/// keeps the measurement in. Two measurements taken from equal inputs come out
/// the same, whatever space they are given.
///
/// The element hands its text, runs and style over rather than copying them:
/// its measurement closure reads them from here, and the next frame's element
/// at the same place compares its own against them.
pub(crate) struct TextMeasureInputs {
    text: SharedString,
    runs: Vec<TextRun>,
    text_style: TextStyle,
    font_size: Pixels,
    line_height: Pixels,
    layout: TextLayout,
}

impl TextMeasureInputs {
    /// Takes over what `layout`'s element measures its text from. The sizes
    /// the element works out of `text_style` are worked out again here, so
    /// that the element hands over only what it would otherwise copy.
    pub(crate) fn new(
        text: SharedString,
        runs: Vec<TextRun>,
        text_style: TextStyle,
        layout: &TextLayout,
        window: &Window,
    ) -> Rc<Self> {
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        let line_height = window.pixel_snap(
            text_style
                .line_height
                .to_pixels(font_size.into(), window.rem_size()),
        );
        Rc::new(Self {
            text,
            runs,
            text_style,
            font_size,
            line_height,
            layout: layout.clone(),
        })
    }

    /// What the measurement closure reads.
    pub(crate) fn parts(&self) -> (&SharedString, Runs<'_>, &TextStyle, &TextLayout) {
        (&self.text, Runs(&self.runs), &self.text_style, &self.layout)
    }

    /// Whether a measurement of `self` would come out as one of `other`.
    fn measures_as(&self, other: &Self) -> bool {
        self.font_size == other.font_size
            && self.line_height == other.line_height
            && self.text == other.text
            && self.runs == other.runs
            && self.text_style == other.text_style
    }
}

/// The runs a measurement closure reads, which it borrows as the slice they
/// dereference to, as it did when it owned them.
#[derive(Clone, Copy)]
pub(crate) struct Runs<'a>(&'a [TextRun]);

impl std::ops::Deref for Runs<'_> {
    type Target = [TextRun];

    fn deref(&self) -> &[TextRun] {
        self.0
    }
}

/// Requests the layout of a text element whose measurement `measure` takes
/// from `inputs` and keeps in their layout.
///
/// A measured node is given a new closure every frame, and would be dirtied
/// for it, with every node above it: a view built again would have all of its
/// text measured and laid out again, though none of it changed. When last
/// frame's element at this place measured the same inputs, its measurement is
/// copied into this one's layout instead, and the node is left clean, keeping
/// what Taffy cached for it. The new closure is still installed, for when
/// Taffy measures it again under other constraints.
pub(crate) fn request_text_layout(
    inputs: Rc<TextMeasureInputs>,
    window: &mut Window,
    measure: impl Fn(Size<Option<Pixels>>, Size<AvailableSpace>, &mut Window, &mut App) -> Size<Pixels>
    + 'static,
) -> LayoutId {
    let adopt = {
        let inputs = inputs.clone();
        move |previous: &dyn Any| {
            let Some(previous) = previous.downcast_ref::<TextMeasureInputs>() else {
                return false;
            };
            if !previous.measures_as(&inputs) {
                return false;
            }
            let Some(inner) = previous.layout.0.borrow().as_ref().map(copy_measurement) else {
                return false;
            };
            *inputs.layout.0.borrow_mut() = Some(inner);
            true
        }
    };
    window.request_carried_measured_layout(inputs, adopt, measure)
}

/// A copy of what a measurement left, without where it was last painted.
fn copy_measurement(inner: &TextLayoutInner) -> TextLayoutInner {
    TextLayoutInner {
        len: inner.len,
        lines: inner
            .lines
            .iter()
            .map(|line| WrappedLine {
                layout: line.layout.clone(),
                text: line.text.clone(),
                decoration_runs: line.decoration_runs.clone(),
            })
            .collect(),
        line_height: inner.line_height,
        wrap_width: inner.wrap_width,
        truncate_width: inner.truncate_width,
        size: inner.size,
        bounds: None,
    }
}

impl Window {
    /// Requests a self-measuring leaf, as [`Window::request_measured_layout`]
    /// does, whose measurement can be carried over from the element at the
    /// same place last frame. `adopt` is given what that element left in
    /// `memo`, and takes its measurement over if it still stands.
    pub(crate) fn request_carried_measured_layout(
        &mut self,
        memo: Rc<dyn Any>,
        adopt: impl FnOnce(&dyn Any) -> bool,
        measure: impl Fn(
            Size<Option<Pixels>>,
            Size<AvailableSpace>,
            &mut Window,
            &mut App,
        ) -> Size<Pixels>
        + 'static,
    ) -> LayoutId {
        self.invalidator.debug_assert_prepaint();
        let rem_size = self.rem_size();
        let scale_factor = self.scale_factor();
        let key = self.layout_key();
        self.layout_engine
            .as_mut()
            .unwrap()
            .request_retained_carried_measured_layout(
                key,
                Style::default(),
                rem_size,
                scale_factor,
                memo,
                adopt,
                measure,
            )
    }
}

/// Leaves in `previous` everything the next frame may ask for: what this
/// frame asked for, which is in `current`, and what it did not but something
/// still holds. `current` is left empty.
///
/// Whichever of the two is larger is kept and the other moved into it, so a
/// frame that asked for little costs little, and so does one that asked for
/// everything.
fn carry_over<K: Eq + Hash, V>(
    previous: &mut FxHashMap<Arc<K>, Arc<V>>,
    current: &mut FxHashMap<Arc<K>, Arc<V>>,
) {
    previous.retain(|_, layout| Arc::strong_count(layout) > 1);
    if previous.len() < current.len() {
        mem::swap(previous, current);
    }
    previous.extend(current.drain());
}

/// Ends a frame of the line layout cache: what it laid out, in `current`,
/// becomes what the next frame can reuse, in `previous`.
///
/// Upstream drops every line the frame did not ask for. A text node that
/// takes over last frame's measurement answers from the lines it holds
/// without asking the cache for them, so its lines would be dropped, and its
/// text, sliding onto another node with the rows around it, shaped again
/// there. A line something still holds is kept.
pub(crate) fn carry_over_line_layouts(previous: &mut FrameCache, current: &mut FrameCache) {
    // Wrapped lines hold the lines they were wrapped from, so they are swept
    // first, letting a line they were the last to hold go with them.
    carry_over(&mut previous.wrapped_lines, &mut current.wrapped_lines);
    carry_over(
        &mut previous.wrapped_lines_by_hash,
        &mut current.wrapped_lines_by_hash,
    );
    carry_over(&mut previous.lines, &mut current.lines);
    carry_over(&mut previous.lines_by_hash, &mut current.lines_by_hash);

    // The used lists index what this frame laid out, which is what a view
    // reused next frame looks its lines up by.
    mem::swap(&mut previous.used_lines, &mut current.used_lines);
    mem::swap(
        &mut previous.used_wrapped_lines,
        &mut current.used_wrapped_lines,
    );
    mem::swap(
        &mut previous.used_lines_by_hash,
        &mut current.used_lines_by_hash,
    );
    mem::swap(
        &mut previous.used_wrapped_lines_by_hash,
        &mut current.used_wrapped_lines_by_hash,
    );
    current.used_lines.clear();
    current.used_wrapped_lines.clear();
    current.used_lines_by_hash.clear();
    current.used_wrapped_lines_by_hash.clear();
}

impl LineLayoutIndex {
    /// This index, taken from a range that started at `from`, as it falls in
    /// a copy of that range starting at `to`.
    pub(crate) fn shifted(&self, from: &Self, to: &Self) -> Self {
        LineLayoutIndex {
            lines_index: self.lines_index - from.lines_index + to.lines_index,
            wrapped_lines_index: self.wrapped_lines_index - from.wrapped_lines_index
                + to.wrapped_lines_index,
            lines_by_hash_index: self.lines_by_hash_index - from.lines_by_hash_index
                + to.lines_by_hash_index,
            wrapped_lines_by_hash_index: self.wrapped_lines_by_hash_index
                - from.wrapped_lines_by_hash_index
                + to.wrapped_lines_by_hash_index,
        }
    }
}

/// Counts the lines the line layout cache hands to the platform to be shaped,
/// because neither this frame nor the last one had them, and times them.
#[derive(Default)]
pub(crate) struct LineShaping {
    /// Lines handed to the platform to be shaped. See [`LineShaping::stats`].
    lines_shaped: AtomicU64,
    /// Time spent in those calls, in nanoseconds.
    shape_nanos: AtomicU64,
    /// Whether to time shaping, which it does once the stats have been reset.
    shape_timed: AtomicBool,
}

impl LineShaping {
    /// How many lines have been shaped, and how long that took, since the last
    /// [`LineShaping::reset`]. A line answered from the cache is not counted,
    /// so this is the text work the cache failed to save.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn stats(&self) -> (u64, std::time::Duration) {
        (
            self.lines_shaped.load(Ordering::Relaxed),
            std::time::Duration::from_nanos(self.shape_nanos.load(Ordering::Relaxed)),
        )
    }

    /// Zeroes the counters reported by [`LineShaping::stats`], and from then
    /// on times shaping too.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn reset(&self) {
        self.lines_shaped.store(0, Ordering::Relaxed);
        self.shape_nanos.store(0, Ordering::Relaxed);
        self.shape_timed.store(true, Ordering::Relaxed);
    }

    /// Shapes a line the cache does not have, counting it.
    pub(crate) fn shape_line(
        &self,
        platform_text_system: &dyn PlatformTextSystem,
        text: &str,
        font_size: Pixels,
        runs: &[FontRun],
    ) -> LineLayout {
        let started_at = self.shape_timed.load(Ordering::Relaxed).then(Instant::now);
        let layout = platform_text_system.layout_line(text, font_size, runs);
        self.lines_shaped.fetch_add(1, Ordering::Relaxed);
        if let Some(started_at) = started_at {
            self.shape_nanos
                .fetch_add(started_at.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        layout
    }
}

impl WindowTextSystem {
    /// Lines shaped by the platform, and the time that took, since the last
    /// [`Self::reset_shaping_stats`]. Lines answered from the cache do not count.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn shaping_stats(&self) -> (u64, std::time::Duration) {
        self.line_layout_cache.shaping.stats()
    }

    /// Zeroes the counters reported by [`Self::shaping_stats`].
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn reset_shaping_stats(&self) {
        self.line_layout_cache.shaping.reset()
    }
}
