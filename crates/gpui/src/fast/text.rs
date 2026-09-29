//! Where a reused range of shaped lines falls in a new frame, text measurements
//! carried from one frame to the next, and shaping statistics.

use crate::{
    App, AvailableSpace, DecorationRun, FontRun, FrameCache, Hsla, LayoutId, LineLayout,
    LineLayoutIndex, Pixels, PlatformTextSystem, SharedString, Size, StrikethroughStyle, Style,
    TextLayout, TextLayoutInner, TextOverflow, TextRun, TextStyle, TruncateFrom, UnderlineStyle,
    WhiteSpace, Window, WindowTextSystem, WrappedLine,
};
use collections::FxHashMap;
use gpui_util::ResultExt as _;
use scheduler::Instant;
use smallvec::SmallVec;
use std::{
    any::Any,
    borrow::Cow,
    cmp,
    hash::Hash,
    mem,
    rc::Rc,
    sync::Arc,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

/// Everything a text element's measurement is taken from, and the layout it
/// keeps the measurement in.
///
/// The element hands its text, runs and style over rather than copying them:
/// its measurement closure reads them from here, and the next frame's element
/// at the same place compares its own against them.
pub(crate) struct TextMeasureInputs {
    text: SharedString,
    /// Plain text is one run, kept inline.
    runs: SmallVec<[TextRun; 1]>,
    text_style: TextStyle,
    font_size: Pixels,
    line_height: Pixels,
    layout: TextLayout,
}

/// The decorations of a run: what it is painted with, and what shaping splits
/// font runs on.
fn decoration_of(
    run: &TextRun,
) -> (
    Hsla,
    Option<Hsla>,
    Option<UnderlineStyle>,
    Option<StrikethroughStyle>,
) {
    (
        run.color,
        run.background_color,
        run.underline,
        run.strikethrough,
    )
}

impl TextMeasureInputs {
    /// Whether text truncates, in which case it is shaped from a rewritten
    /// string whose runs no longer line up with these.
    fn truncates(&self) -> bool {
        self.text_style.text_overflow.is_some()
    }

    /// Whether `self` is shaped as `other` is: the same text, sizes, fonts and
    /// wrapping, and decoration changing in the same places, since shaping
    /// splits font runs wherever it changes. What it is painted with may
    /// differ; see [`Self::decorated_as`].
    fn shapes_as(&self, other: &Self) -> bool {
        fn runs(inputs: &TextMeasureInputs) -> impl Iterator<Item = &TextRun> {
            inputs.runs.iter().filter(|run| run.len > 0)
        }
        fn joins_previous<'a>(
            runs: impl Iterator<Item = &'a TextRun>,
        ) -> impl Iterator<Item = bool> {
            let mut previous = None;
            runs.map(move |run| {
                previous
                    .replace(decoration_of(run))
                    .is_some_and(|previous| previous == decoration_of(run))
            })
        }
        let (style, other_style) = (&self.text_style, &other.text_style);
        self.text == other.text
            && self.font_size == other.font_size
            && self.line_height == other.line_height
            && style.white_space == other_style.white_space
            && style.line_clamp == other_style.line_clamp
            && style.text_overflow == other_style.text_overflow
            // Only truncation reads the style's own font.
            && (!self.truncates() || style.font() == other_style.font())
            && runs(self).count() == runs(other).count()
            && runs(self)
                .zip(runs(other))
                .all(|(run, other)| run.len == other.len && run.font == other.font)
            && joins_previous(runs(self)).eq(joins_previous(runs(other)))
    }

    /// Whether `self` is painted with what `other` is.
    fn decorated_as(&self, other: &Self) -> bool {
        self.runs
            .iter()
            .filter(|run| run.len > 0)
            .map(decoration_of)
            .eq(other
                .runs
                .iter()
                .filter(|run| run.len > 0)
                .map(decoration_of))
    }
}

/// Requests the layout of a text element, whose measurement it keeps in
/// `layout`: what [`TextLayout`]'s layout does upstream.
///
/// A measured node is given a new closure every frame, and would be dirtied
/// for it, with every node above it: a view built again would have all of its
/// text measured and laid out again, though none of it changed. When last
/// frame's element at this place measured text shaped the same way, its
/// measurement is copied into this one's layout instead, repainted with this
/// one's decorations if only they changed, and the node is left clean,
/// keeping what Taffy cached for it. The new closure is still installed, for
/// when Taffy measures it again under other constraints.
pub(crate) fn layout_text(
    layout: &TextLayout,
    text: SharedString,
    runs: Option<Vec<TextRun>>,
    window: &mut Window,
) -> LayoutId {
    let text_style = window.text_style();
    let font_size = text_style.font_size.to_pixels(window.rem_size());
    let line_height = window.pixel_snap(
        text_style
            .line_height
            .to_pixels(font_size.into(), window.rem_size()),
    );
    let runs = match runs {
        Some(runs) => SmallVec::from_vec(runs),
        None => SmallVec::from_buf([text_style.to_run(text.len())]),
    };
    let inputs = Rc::new(TextMeasureInputs {
        text,
        runs,
        text_style,
        font_size,
        line_height,
        layout: layout.clone(),
    });
    let adopt = {
        let inputs = inputs.clone();
        move |previous: &dyn Any| {
            let Some(previous) = previous.downcast_ref::<TextMeasureInputs>() else {
                return false;
            };
            if !previous.shapes_as(&inputs) {
                return false;
            }
            let recolored = !previous.decorated_as(&inputs);
            if recolored && inputs.truncates() {
                return false;
            }
            let Some(mut inner) = take_measurement(&previous.layout) else {
                return false;
            };
            if recolored {
                update_decoration_runs(&mut inner.lines, &inputs.runs);
            }
            *inputs.layout.0.borrow_mut() = Some(inner);
            true
        }
    };
    let measure = {
        let inputs = inputs.clone();
        move |known_dimensions, available_space, window: &mut Window, cx: &mut App| {
            measure_text(&inputs, known_dimensions, available_space, window, cx)
        }
    };
    window.request_carried_measured_layout(inputs, adopt, measure)
}

/// Measures text under the constraints Taffy offers, keeping the result in
/// its layout: upstream's measurement, doing less of its work.
///
/// - Taffy asks a node for its intrinsic size before it lays the node out, so
///   a wrapping leaf is measured unconstrained and then again at the width it
///   ends up with. Text that came out narrower than the width now on offer
///   wraps nowhere, and the lines shaped without a wrap width are the same
///   lines, so that is answered from what is already there.
/// - Which affix to truncate with, and the line wrapper truncation needs, are
///   worked out only when the text truncates and has to be shaped: resolving
///   the font and borrowing a wrapper from the pool on every measurement was
///   for nothing on text that does not truncate.
fn measure_text(
    inputs: &TextMeasureInputs,
    known_dimensions: Size<Option<Pixels>>,
    available_space: Size<AvailableSpace>,
    window: &mut Window,
    cx: &mut App,
) -> Size<Pixels> {
    let TextMeasureInputs {
        text,
        runs,
        text_style,
        font_size,
        line_height,
        layout,
    } = inputs;
    let (font_size, line_height) = (*font_size, *line_height);
    let wrap_width = if text_style.white_space == WhiteSpace::Normal {
        known_dimensions.width.or(match available_space.width {
            AvailableSpace::Definite(x) => Some(x),
            _ => None,
        })
    } else {
        None
    };
    let truncate_width = text_style.text_overflow.as_ref().and_then(|_| {
        known_dimensions.width.or(match available_space.width {
            AvailableSpace::Definite(x) => match text_style.line_clamp {
                Some(max_lines) => Some(x * max_lines),
                None => Some(x),
            },
            _ => None,
        })
    });

    // A kept measurement answers when the wrap width is one it was taken at,
    // or one it fits within unwrapped, unless truncation is involved either
    // way: a truncated layout would answer an unconstrained probe with the
    // truncated size.
    if let Some(text_layout) = layout.0.borrow().as_ref()
        && let Some(size) = text_layout.size
        && (wrap_width.is_none()
            || wrap_width == text_layout.wrap_width
            || (text_layout.wrap_width.is_none()
                && wrap_width.is_some_and(|wrap_width| size.width <= wrap_width)))
        && truncate_width.is_none()
        && text_layout.truncate_width.is_none()
    {
        return size;
    }

    let (text, runs) = if let Some(truncate_width) = truncate_width {
        let (truncation_affix, truncate_from) = match text_style.text_overflow.clone() {
            Some(TextOverflow::Truncate(affix)) => (affix, TruncateFrom::End),
            Some(TextOverflow::TruncateStart(affix)) => (affix, TruncateFrom::Start),
            Some(TextOverflow::TruncateMiddle(affix)) => (affix, TruncateFrom::Middle),
            None => (SharedString::default(), TruncateFrom::End),
        };
        let mut line_wrapper = cx.text_system().line_wrapper(text_style.font(), font_size);
        if let Some(max_lines) = text_style.line_clamp
            && let Some(wrap_width) = wrap_width
        {
            line_wrapper.truncate_wrapped_line(
                text.clone(),
                wrap_width,
                max_lines,
                &truncation_affix,
                runs,
                truncate_from,
            )
        } else if let Some(unclipped) = window
            .text_system()
            .shape_text(text.clone(), font_size, runs, None, None)
            .log_err()
            && unclipped
                .iter()
                .all(|line| line.size(line_height).width <= truncate_width)
        {
            // Truncation sums per-character advances, which overestimates the
            // shaped width, so text that fits once shaped is not truncated.
            (text.clone(), Cow::Borrowed(&runs[..]))
        } else {
            line_wrapper.truncate_line(
                text.clone(),
                truncate_width,
                &truncation_affix,
                runs,
                truncate_from,
            )
        }
    } else {
        (text.clone(), Cow::Borrowed(&runs[..]))
    };
    let len = text.len();

    let Some(lines) = window
        .text_system()
        .shape_text(text, font_size, &runs, wrap_width, text_style.line_clamp)
        .log_err()
    else {
        layout.0.borrow_mut().replace(TextLayoutInner {
            lines: Default::default(),
            len: 0,
            line_height,
            wrap_width,
            truncate_width,
            size: Some(Size::default()),
            bounds: None,
        });
        return Size::default();
    };

    let mut size: Size<Pixels> = Size::default();
    for line in &lines {
        let line_size = line.size(line_height);
        size.height += line_size.height;
        size.width = size.width.max(line_size.width).ceil();
    }
    layout.0.borrow_mut().replace(TextLayoutInner {
        lines,
        len,
        line_height,
        wrap_width,
        truncate_width,
        size: Some(size),
        bounds: None,
    });
    size
}

/// Rewrites the decorations of lines already shaped, leaving the shaping
/// alone: recoloring text is a matter of replacing what is painted over it.
///
/// `runs` must split the lines as the runs they were shaped with did: the same
/// lengths, the same fonts, and decoration changing in the same places; see
/// [`TextMeasureInputs::shapes_as`].
pub(crate) fn update_decoration_runs(lines: &mut [WrappedLine], runs: &[TextRun]) {
    let mut runs = runs.iter().filter(|run| run.len > 0).cloned().peekable();
    for line in lines.iter_mut() {
        let line_len = line.text.len();
        line.decoration_runs.clear();
        let mut offset = 0;
        while offset < line_len {
            let Some(run) = runs.peek_mut() else {
                log::warn!("`TextRun`s do not cover the entire shaped text");
                break;
            };
            let len_within_line = cmp::min(line_len - offset, run.len);
            if let Some(last_run) = line.decoration_runs.last_mut()
                && last_run.color == run.color
                && last_run.underline == run.underline
                && last_run.strikethrough == run.strikethrough
                && last_run.background_color == run.background_color
            {
                last_run.len += len_within_line as u32;
            } else {
                line.decoration_runs.push(DecorationRun {
                    len: len_within_line as u32,
                    color: run.color,
                    background_color: run.background_color,
                    underline: run.underline,
                    strikethrough: run.strikethrough,
                });
            }
            run.len -= len_within_line;
            if run.len == 0 {
                runs.next();
            }
            offset += len_within_line;
        }
        // Skip the `\n` that separated this line from the next.
        if let Some(run) = runs.peek_mut() {
            run.len -= 1;
            if run.len == 0 {
                runs.next();
            }
        }
    }
}

/// What `layout`'s measurement left, without where it was last painted, for
/// this frame's element to take over. Last frame's element is gone, so when
/// nothing but its measurement holds its layout any more, the measurement is
/// moved out rather than copied, line by line; otherwise it is copied, and
/// whatever holds the layout still finds it.
fn take_measurement(layout: &TextLayout) -> Option<TextLayoutInner> {
    if Rc::strong_count(&layout.0) == 1 {
        let mut inner = layout.0.borrow_mut().take()?;
        inner.bounds = None;
        return Some(inner);
    }
    layout.0.borrow().as_ref().map(copy_measurement)
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
