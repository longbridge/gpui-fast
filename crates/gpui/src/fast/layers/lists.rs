//! Rendering only the rows of a `uniform_list` or `list` its layer lacks (M6).
//!
//! A virtual list renders only the rows it shows. Its layer holds more: the
//! rows its viewport shows and one viewport's worth of rows on each side
//! (the overscan), each painted into the layer's content scene at its place
//! in content space and kept there by row. On a frame that only scrolled
//! the list the rows the layer holds are neither rendered, laid out,
//! prepainted nor painted: only the rows the scroll brought into the
//! overscan are, and are added to the layer, whose tiles over them become
//! dirty; rows that left the overscan are dropped from it (spec §8). Any
//! other frame paints the rows afresh, as a div's layer is.
//!
//! The rows a layer keeps hand a frame that composites it nothing but their
//! tiles: no hitboxes, listeners, element states or dispatch nodes. A list
//! whose rows hand the frame any is demoted to today's path (see
//! [`holds_input`]) until those records are carried row by row.
//!
//! The list elements call in here from a few hooks:
//!
//! - `uniform_list`: [`measure_item`], [`snap_item_offset`],
//!   [`begin_uniform_list`], [`render_rows`], [`row_indices`] and
//!   [`end_rows`] where it prepaints its rows; [`begin_paint_rows`],
//!   [`paint_row`] and [`end_paint_rows`] where it paints them.
//! - `list`: [`begin_list`], [`keeps_row`], [`snap_item_origin`],
//!   [`place_list_item`] and [`end_list`] where it lays
//!   out and prepaints its rows; [`begin_paint_list`], [`paint_row`] and
//!   [`end_paint_list`] where it paints them. A `list` has no element id: its
//!   layer is known by the id of the elements around it and its state.
//!
//! Between a list's prepaint and its paint, what the frame does with its
//! rows lives in its layer's [`LayerRows`]. While its rows prepaint and
//! paint, `WindowLayers::painting` is set, as for a div's layer, so that
//! nested views and scroll containers are painted into the layer.

use crate::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, ElementId, EntityId, GlobalElementId,
    PaintIndex, Pixels, Point, PrepaintStateIndex, Rgba, ScaledPixels, Scene, Size, TextStyle,
    Window,
    fast::{
        dependencies::{DependencyRecording, RenderDependencies, StateVersion},
        layers::{
            COMPILED, active, invalidate,
            paint::{self, Painting},
            policy::{self, Decision},
            record::LayerRecord,
            scene::translate_primitive,
            tiles::{dirty_tiles, tile_hashes},
        },
    },
    point, px,
    scene::PaintOperation,
    size,
};
use collections::FxHashMap;
use smallvec::SmallVec;
use std::{
    collections::{BTreeMap, BTreeSet},
    mem,
    ops::Range,
    rc::Rc,
};

/// The rows of a list's layer, and what the frame being drawn does with
/// them.
#[derive(Default)]
pub(crate) struct LayerRows {
    /// The rows the layer holds, by index.
    pub(crate) painted: BTreeSet<usize>,
    /// Where each row the layer holds lies, in content space.
    pub(crate) row_origins: FxHashMap<usize, Point<Pixels>>,
    /// Whether the layer is a list's, which a scroll extends by the rows it
    /// uncovers instead of painting it again.
    pub(crate) list: bool,
    /// Whether a row the layer holds handed the frame, besides what it drew,
    /// records a frame that composites the layer would lose: hitboxes,
    /// mouse listeners, element states, focusable or listening dispatch
    /// nodes. Such rows are not carried through composited frames yet (a
    /// list's rows are painted over many frames at as many translations), so
    /// such a layer is demoted; see [`holds_input`].
    holds_input: bool,
    /// Whether the rows the list showed on the last frame it kept them off
    /// its layer handed the frame hitboxes or other records a composited
    /// frame would lose, as they would in the layer: such a list is not
    /// promoted to a layer only to be demoted again (see
    /// [`took_input_off_layer`]).
    took_input_off_layer: bool,
    /// Where the rows of a list kept off its layer this frame began to
    /// prepaint, and for a `list` its state's id, until they are prepainted.
    bypass: Option<(Option<usize>, PrepaintStateIndex)>,
    /// How many rows frames that kept the rows the layer held added to it
    /// since its rows were last painted afresh. What the rows read, their
    /// hovers and their views are kept for the whole layer, not by row, so
    /// they only grow on those frames: past [`REPAINT_AFTER_ADDED`] times
    /// the rows the layer is to hold, the rows are painted afresh.
    added_since_repaint: usize,
    /// The content of each row the layer holds.
    rows: BTreeMap<usize, Row>,
    /// A uniform list's measured item, as last measured.
    measured: Option<Measured>,
    /// What the frame being drawn does with the list's rows, from its
    /// prepaint to its paint.
    frame: Option<RowsFrame>,
}

impl LayerRows {
    /// Ends the frame being drawn.
    pub(crate) fn finish_frame(&mut self) {
        self.frame = None;
        self.bypass = None;
    }

    fn clear(&mut self) {
        self.painted.clear();
        self.row_origins.clear();
        self.rows.clear();
        self.holds_input = false;
        self.added_since_repaint = 0;
    }

    /// Whether the rows the layer holds, `needed` of them from now on, are
    /// to be painted afresh for having added too many rows since they last
    /// were (see [`LayerRows::added_since_repaint`]).
    fn due_for_repaint(&self, needed: &Range<usize>) -> bool {
        self.added_since_repaint > needed.len().max(1) * REPAINT_AFTER_ADDED
    }
}

/// How many times the rows a list's layer is to hold its frames that keep
/// rows may add before its rows are painted afresh.
const REPAINT_AFTER_ADDED: usize = 4;

/// Whether the layer `layer` is a list's whose rows hand the frame records
/// a composited frame would lose (see [`LayerRows::holds_input`]): such a
/// layer is demoted, the list kept on today's path.
pub(crate) fn holds_input(layer: &crate::fast::layers::Layer) -> bool {
    layer.rows.list && layer.rows.holds_input
}

/// Whether the list of `layer`, kept off its layer, showed rows that took
/// input on the last frame, which rows painted into the layer would do as
/// well: the layer would be demoted for it as soon as it was painted (see
/// [`holds_input`]), so the list is not promoted.
pub(crate) fn took_input_off_layer(layer: &crate::fast::layers::Layer) -> bool {
    layer.rows.took_input_off_layer
}

/// Notes where the rows of the list `id`, of the `list` whose state has id
/// `list` if it is one, begin to prepaint on a frame that keeps them off the
/// list's layer, to tell once they are whether they took input.
fn begin_bypass(window: &mut Window, id: &GlobalElementId, list: Option<usize>) -> bool {
    let start = window.prepaint_index();
    match window.fast_layers.layers.get_mut(id) {
        Some(layer) => {
            layer.rows.bypass = Some((list, start));
            true
        }
        None => false,
    }
}

/// Ends what [`begin_bypass`] began for the list `id`, once its rows are
/// prepainted.
fn finish_bypass(window: &mut Window, id: &GlobalElementId) {
    let Some((_, start)) = window
        .fast_layers
        .layers
        .get_mut(id)
        .and_then(|layer| layer.rows.bypass.take())
    else {
        return;
    };
    let paint = window.paint_index();
    let took_input = adds_input(
        window,
        &(start..window.prepaint_index()),
        &(paint.clone()..paint),
    );
    if let Some(layer) = window.fast_layers.layers.get_mut(id) {
        layer.rows.took_input_off_layer = took_input;
    }
}

/// Whether prepainting and painting a list's rows, over `prepaint` and
/// `paint`, handed the frame records besides the scene: hitboxes, tooltips,
/// element states, dispatch nodes that are focusable, have a key context or
/// listen, mouse listeners, cursor styles, input handlers or tab stops.
fn adds_input(
    window: &Window,
    prepaint: &Range<PrepaintStateIndex>,
    paint: &Range<PaintIndex>,
) -> bool {
    let (p, q) = (&prepaint.start, &prepaint.end);
    let (a, b) = (&paint.start, &paint.end);
    let nodes = &window.next_frame.dispatch_tree.nodes;
    let start = p.dispatch_tree_index.min(nodes.len());
    let end = q.dispatch_tree_index.clamp(start, nodes.len());
    p.hitboxes_index != q.hitboxes_index
        || p.tooltips_index != q.tooltips_index
        || p.accessed_element_states_index != q.accessed_element_states_index
        || a.fast_window_control_hitboxes_index != b.fast_window_control_hitboxes_index
        || a.mouse_listeners_index != b.mouse_listeners_index
        || a.cursor_styles_index != b.cursor_styles_index
        || a.input_handlers_index != b.input_handlers_index
        || a.accessed_element_states_index != b.accessed_element_states_index
        || a.tab_handle_index != b.tab_handle_index
        || nodes[start..end].iter().any(|node| {
            node.focus_id.is_some()
                || node.context.is_some()
                || !node.key_listeners.is_empty()
                || !node.action_listeners.is_empty()
                || !node.modifiers_changed_listeners.is_empty()
        })
}

#[cfg(test)]
thread_local! {
    static EXTENDED_FRAMES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many frames on this thread composited a list's layer keeping the
/// rows it held.
#[cfg(test)]
pub(crate) fn extended_frames() -> usize {
    EXTENDED_FRAMES.with(|frames| frames.get())
}

/// Counts a frame that composited a list's layer keeping the rows it held.
fn count_extended_frame() {
    #[cfg(test)]
    EXTENDED_FRAMES.with(|frames| frames.set(frames.get() + 1));
}

/// A row as the layer holds it.
struct Row {
    /// The row's slot, as wide as the viewport, in content space.
    slot: Bounds<ScaledPixels>,
    /// What painting the row drew, in content space.
    operations: Vec<PaintOperation>,
}

/// A uniform list's measured item, and what it was measured with besides
/// the item itself.
struct Measured {
    size: Size<Pixels>,
    rem_size: Pixels,
    text_style: TextStyle,
}

/// The rows a list renders into its layer this frame, and those it keeps.
pub(crate) struct RowPlan {
    /// The rows to render, prepaint and paint into the layer, in order.
    pub(crate) render: Vec<Range<usize>>,
    /// The rows the layer holds that stay, which are not rendered.
    pub(crate) keep: Range<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// The rows are painted afresh.
    Repaint,
    /// The rows the layer holds stay; those it lacks are added.
    Extend,
}

/// What the frame being drawn does with a list's rows.
struct RowsFrame {
    mode: Mode,
    /// For a `list`, its state, to tell its hooks from a nested list's.
    list: Option<usize>,
    /// Whether the list leaves out the rows the layer holds when it lays
    /// out the rows it shows.
    skip_held: bool,
    /// The rows the layer is to hold after the frame.
    needed: Range<usize>,
    /// The rows the layer holds that stay.
    keep: Range<usize>,
    /// Where each row painted into the layer this frame lies, in window
    /// space.
    slots: BTreeMap<usize, Bounds<Pixels>>,
    /// The list's clip rect, in window space.
    viewport: Bounds<Pixels>,
    /// The part of the content the frame paints, in window space: the
    /// viewport and the rows around it.
    painted_region: Bounds<Pixels>,
    /// The offset the rows are painted at, snapped.
    scroll_offset: Point<Pixels>,
    /// How far the layer's content is moved into window space this frame.
    translation: Point<ScaledPixels>,
    prepaint_start: PrepaintStateIndex,
    prepaint_range: Range<PrepaintStateIndex>,
    /// The recording of what rendering and prepainting the rows reads.
    recording: Option<DependencyRecording>,
    dependencies: RenderDependencies,
    /// A uniform list's rows in the order it paints them, and how many it
    /// painted.
    order: Vec<usize>,
    next: usize,
    /// A `list`'s first row shown and where it lies, as it prepaints its rows.
    anchor: Option<(usize, Point<Pixels>)>,
    /// The rows a `list` prepainted itself.
    prepainted: Vec<usize>,
    /// The rows rendered and prepainted here for a `list`, painted after its
    /// own.
    extra: Vec<(usize, AnyElement)>,
    paint: Option<PaintState>,
}

/// A list's rows being painted.
struct PaintState {
    /// The opaque colour under the viewport, if there is one; the rows are
    /// painted into the frame otherwise.
    background: Option<Rgba>,
    /// Whether rows are painted into the layer's scene, which the frame's is
    /// then swapped out for.
    swapped: bool,
    /// The frame's scene, swapped out for the layer's.
    scene: Scene,
    paint_start: PaintIndex,
    hovers_start: usize,
    recording: Option<DependencyRecording>,
    /// The row being painted.
    current: Option<usize>,
    /// What each row painted into the layer's scene.
    spans: Vec<(usize, Range<usize>)>,
}

/// What a `uniform_list` does with its rows this frame: nothing new, or
/// render the plan's rows into its layer.
/// On a frame that keeps the list off its layer, the list's id, if it has
/// a layer.
pub(crate) struct Rows(Option<RowPlan>, Option<GlobalElementId>);

/// The rows to render into the layer of the list `id` showing the rows
/// `visible` of its `item_count`, with `overscan` rows around them, when
/// the rows the layer holds stay.
pub(crate) fn rows_to_render(
    window: &Window,
    id: &GlobalElementId,
    visible: Range<usize>,
    overscan: usize,
    item_count: usize,
) -> RowPlan {
    let needed = needed_rows(&visible, overscan, item_count);
    let held = window
        .fast_layers
        .layers
        .get(id)
        .map(|layer| &layer.rows.painted);
    let empty = BTreeSet::new();
    let held = held.unwrap_or(&empty);
    plan(held, needed)
}

/// The rows `visible` and `overscan` rows on each side, of `item_count`.
fn needed_rows(visible: &Range<usize>, overscan: usize, item_count: usize) -> Range<usize> {
    visible.start.saturating_sub(overscan).min(item_count)..(visible.end + overscan).min(item_count)
}

/// The rows of `needed` that `held` lacks, and those it holds.
fn plan(held: &BTreeSet<usize>, needed: Range<usize>) -> RowPlan {
    let mut render: Vec<Range<usize>> = Vec::new();
    let mut keep = needed.start..needed.start;
    for row in needed {
        if held.contains(&row) {
            if keep.is_empty() {
                keep = row..row + 1;
            } else {
                keep.end = row + 1;
            }
        } else {
            match render.last_mut() {
                Some(run) if run.end == row => run.end = row + 1,
                _ => render.push(row..row + 1),
            }
        }
    }
    RowPlan { render, keep }
}

/// The size of a uniform list's measured item: `measure`d, unless the
/// frame only scrolls the list's layer, whose content, the measured item
/// included, is then as it was (spec §8).
pub(crate) fn measure_item(
    window: &mut Window,
    cx: &mut App,
    id: Option<&GlobalElementId>,
    measure: impl FnOnce(&mut Window, &mut App) -> Size<Pixels>,
) -> Size<Pixels> {
    if !LIST_LAYERS {
        return measure(window, cx);
    }
    if !COMPILED || window.fast_layers.layers.is_empty() {
        return measure(window, cx);
    }
    let Some(id) = id else {
        return measure(window, cx);
    };
    if let Some(size) = kept_item_size(window, cx, id) {
        return size;
    }
    let size = measure(window, cx);
    let rem_size = window.rem_size();
    let text_style = window.text_style();
    if let Some(layer) = window.fast_layers.layers.get_mut(id) {
        layer.rows.measured = Some(Measured {
            size,
            rem_size,
            text_style,
        });
    }
    size
}

/// The measured item's size as the layer of the list `id` keeps it, if the
/// frame only scrolls the layer.
fn kept_item_size(window: &Window, cx: &App, id: &GlobalElementId) -> Option<Size<Pixels>> {
    if paint::inside_layer(window) || !active(window, cx) {
        return None;
    }
    let layer = window.fast_layers.layers.get(id)?;
    let record = layer.record.as_ref()?;
    let measured = layer.rows.measured.as_ref()?;
    if !layer.rows.list
        || measured.rem_size != window.rem_size()
        || measured.text_style != window.text_style()
    {
        return None;
    }
    #[cfg(any(test, feature = "test-support"))]
    if let Some(decision) = window.fast_layers.forced_decision {
        return (decision == Decision::Composite).then_some(measured.size);
    }
    invalidate::scroll_only(window, cx, id, record).then_some(measured.size)
}

/// `scroll_offset`, a uniform list's offset about to place its rows, moved
/// to whole device pixels where layers are compiled, so that rows painted
/// into a layer and rows drawn without one land on the same pixels. See
/// [`paint::snap_scroll_offset`].
pub(crate) fn snap_item_offset(window: &Window, scroll_offset: Point<Pixels>) -> Point<Pixels> {
    if !LIST_LAYERS {
        return scroll_offset;
    }
    paint::snap_scroll_offset(window, scroll_offset)
}

/// Decides what the uniform list `id` does with its rows this frame, and
/// sets up rendering and prepainting them. The list's rows are
/// `item_height` tall, `item_count` of them from the top of
/// `padded_bounds`, scrolled by `scroll_offset`; it shows the rows
/// `visible`. A list flipped vertically keeps today's path.
#[allow(clippy::too_many_arguments)]
/// Whether virtual lists get scroll layers. Off: measured on `gpui_perf`'s
/// list scenarios, list layers composited about 1 % of scrolled frames and
/// cost more than they saved, so lists draw as they do without layers until
/// that is fixed.
pub(crate) const LIST_LAYERS: bool = false;

pub(crate) fn begin_uniform_list(
    window: &mut Window,
    cx: &mut App,
    id: Option<&GlobalElementId>,
    padded_bounds: Bounds<Pixels>,
    scroll_offset: Point<Pixels>,
    item_height: Pixels,
    item_count: usize,
    visible: &Range<usize>,
    y_flipped: bool,
) -> Rows {
    if !COMPILED || !LIST_LAYERS || y_flipped || item_height <= Pixels::ZERO {
        return Rows(None, None);
    }
    let Some(id) = id else {
        return Rows(None, None);
    };
    if paint::inside_layer(window) || !active(window, cx) {
        return Rows(None, None);
    }
    let viewport = window.content_mask().bounds;
    let content_size = size(padded_bounds.size.width, item_height * item_count);
    let decision = policy::decide(window, cx, id, padded_bounds, content_size, scroll_offset);
    if decision == Decision::Bypass {
        let bypassed = begin_bypass(window, id, None).then(|| id.clone());
        return Rows(None, bypassed);
    }
    let overscan = (viewport.size.height * paint::OVERSCAN_VIEWPORTS / item_height)
        .ceil()
        .max(1.) as usize;
    let needed = needed_rows(visible, overscan, item_count);
    let layer = paint::layer_mut(window, id);
    let extends = decision == Decision::Composite
        && layer.rows.list
        && !layer.rows.due_for_repaint(&needed)
        && layer
            .record
            .as_ref()
            .is_some_and(|record| record.scroll_offset.x == scroll_offset.x);
    let mode = if extends { Mode::Extend } else { Mode::Repaint };

    let plan = match mode {
        Mode::Extend => rows_to_render(window, id, visible.clone(), overscan, item_count),
        Mode::Repaint => RowPlan {
            render: vec![needed.clone()],
            keep: needed.start..needed.start,
        },
    };
    let slot = |row: usize| Bounds {
        origin: point(
            viewport.origin.x,
            padded_bounds.origin.y + scroll_offset.y + item_height * row,
        ),
        size: size(viewport.size.width, item_height),
    };
    let slots: BTreeMap<usize, Bounds<Pixels>> = plan
        .render
        .iter()
        .flat_map(|run| run.clone())
        .map(|row| (row, slot(row)))
        .collect();
    let mut painted_region = viewport;
    if !needed.is_empty() {
        painted_region = painted_region
            .union(&slot(needed.start))
            .union(&slot(needed.end - 1));
    }
    let translation = paint::translation(window, scroll_offset);
    let order = slots.keys().copied().collect();
    let frame = RowsFrame {
        mode,
        list: None,
        skip_held: false,
        needed,
        keep: plan.keep.clone(),
        slots,
        viewport,
        painted_region,
        scroll_offset,
        translation,
        prepaint_start: window.prepaint_index(),
        prepaint_range: window.prepaint_index()..window.prepaint_index(),
        recording: Some(cx.begin_recording_dependencies()),
        dependencies: RenderDependencies::default(),
        order,
        next: 0,
        anchor: None,
        prepainted: Vec::new(),
        extra: Vec::new(),
        paint: None,
    };
    window.fast_layers.painting = Some(marker(id, &frame));
    paint::layer_mut(window, id).rows.frame = Some(frame);
    if mode == Mode::Extend {
        keep_reads(window, cx, id);
    }
    Rows(Some(plan), None)
}

/// Tells the window that what the rows held by the layer of the list `id`
/// read is read again this frame, which renders them not: so that the
/// window stays told of their changes, a row view's notifications included,
/// as it is on today's path, where the list renders the rows it shows every
/// frame.
fn keep_reads(window: &Window, cx: &mut App, id: &GlobalElementId) {
    let Some(record) = window
        .fast_layers
        .layers
        .get(id)
        .and_then(|layer| layer.record.as_ref())
    else {
        return;
    };
    cx.replay_dependencies(&record.dependencies);
    cx.entities.extend_accessed(record.views.iter());
}

/// What marks the rows of the list `id` as painting into its layer, for
/// nested views and scroll containers to tell.
fn marker(id: &GlobalElementId, frame: &RowsFrame) -> Painting {
    Painting {
        id: id.clone(),
        viewport: frame.viewport,
        painted_region: frame.painted_region,
        scene: Scene::default(),
        scroll_offset: frame.scroll_offset,
        translation: frame.translation,
        prepaint_range: frame.prepaint_start.clone()..frame.prepaint_start.clone(),
        recording: None,
        dependencies: RenderDependencies::default(),
        input: Default::default(),
        view_layouts: FxHashMap::default(),
    }
}

/// Renders the rows a uniform list renders this frame with `render`: those
/// it shows, `visible`, on today's path, and the rows its layer lacks
/// otherwise, in order.
pub(crate) fn render_rows(
    rows: &Rows,
    visible: Range<usize>,
    mut render: impl FnMut(Range<usize>) -> SmallVec<[AnyElement; 64]>,
) -> SmallVec<[AnyElement; 64]> {
    let Some(plan) = &rows.0 else {
        return render(visible);
    };
    let mut items = SmallVec::new();
    for run in &plan.render {
        items.extend(render(run.clone()));
    }
    items
}

/// The indices of the rows [`render_rows`] rendered, in order.
pub(crate) fn row_indices(rows: &Rows, visible: Range<usize>) -> RowIndices {
    match &rows.0 {
        None => RowIndices::Visible(visible),
        Some(plan) => RowIndices::Planned(
            plan.render
                .iter()
                .flat_map(|run| run.clone())
                .collect::<Vec<_>>()
                .into_iter(),
        ),
    }
}

/// See [`row_indices`].
pub(crate) enum RowIndices {
    Visible(Range<usize>),
    Planned(std::vec::IntoIter<usize>),
}

impl Iterator for RowIndices {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        match self {
            RowIndices::Visible(range) => range.next(),
            RowIndices::Planned(rows) => rows.next(),
        }
    }
}

/// Ends what [`begin_uniform_list`] began, once the rows are prepainted.
pub(crate) fn end_rows(window: &mut Window, cx: &mut App, rows: Rows) {
    if rows.0.is_none() {
        if let Some(id) = rows.1 {
            finish_bypass(window, &id);
        }
        return;
    }
    let Some(painting) = window.fast_layers.painting.take() else {
        debug_assert!(false, "a list's rows ended without beginning");
        return;
    };
    finish_prepaint(window, cx, &painting.id);
}

/// Ends the prepaint of the rows of the list `id`: what it added to the
/// frame and read is the frame's.
fn finish_prepaint(window: &mut Window, cx: &mut App, id: &GlobalElementId) {
    let end = window.prepaint_index();
    let Some(frame) = frame_mut(window, id) else {
        return;
    };
    frame.prepaint_range = frame.prepaint_start.clone()..end;
    let recording = frame.recording.take();
    if let Some(recording) = recording {
        let dependencies = without_owner(window, cx.finish_recording_dependencies(recording).all);
        if let Some(frame) = frame_mut(window, id) {
            frame.dependencies = dependencies;
        }
    }
}

/// `dependencies`, read by a list's rows, without the view holding the
/// list: a list renders its rows as that view, whose own changes, and
/// whether it was notified for anything but a scroll, are judged apart (see
/// [`invalidate::scroll_only`]).
fn without_owner(window: &Window, dependencies: RenderDependencies) -> RenderDependencies {
    invalidate::without_entity(&dependencies, invalidate::owner_view(window))
        .unwrap_or(dependencies)
}

fn frame_mut<'a>(window: &'a mut Window, id: &GlobalElementId) -> Option<&'a mut RowsFrame> {
    window
        .fast_layers
        .layers
        .get_mut(id)
        .and_then(|layer| layer.rows.frame.as_mut())
}

/// The id a `list` whose state `version` counts changes of has: the id of
/// the elements around it, and its state.
fn list_id(window: &Window, version: &StateVersion) -> GlobalElementId {
    let mut path: Vec<ElementId> = window.element_id_stack.to_vec();
    path.push(ElementId::NamedInteger(
        LIST_ID_NAME.into(),
        version.id() as u64,
    ));
    GlobalElementId::new(path.into())
}

/// What the ids of the elements and views inside the scroll container `id`
/// start with: `id`, or for a `list`, which has no id of its own, the id of
/// the elements around it (see [`list_id`]).
pub(crate) fn content_prefix(id: &GlobalElementId) -> &[ElementId] {
    match id.split_last() {
        Some((ElementId::NamedInteger(name, _), around)) if name.as_ref() == LIST_ID_NAME => around,
        _ => id,
    }
}

/// Whether a view drawn into the rows the layer of the list `id` holds is
/// one `f` picks. A list's rows are rendered as it prepaints, not as the view
/// holding it renders, so their views are not among that view's nested ones;
/// the layer remembers them.
pub(crate) fn any_held_view(
    window: &Window,
    id: &GlobalElementId,
    f: impl Fn(EntityId) -> bool,
) -> bool {
    window
        .fast_layers
        .layers
        .get(id)
        .filter(|layer| layer.rows.list)
        .and_then(|layer| layer.record.as_ref())
        .is_some_and(|record| record.views.iter().copied().any(f))
}

/// The name of the last part of a `list`'s id.
const LIST_ID_NAME: &str = "fast-list";

/// The frame of the `list` whose rows are prepainting, if it is the one
/// whose state `version` counts changes of.
fn list_frame<'a>(window: &'a mut Window, version: &StateVersion) -> Option<&'a mut RowsFrame> {
    let id = window.fast_layers.painting.as_ref()?.id.clone();
    frame_mut(window, &id).filter(|frame| frame.list == Some(version.id()))
}

/// Decides what the `list` of `state`, laid out at `bounds`, does with its
/// rows this frame, before it lays them out and prepaints them.
pub(crate) fn begin_list(
    window: &mut Window,
    cx: &mut App,
    state: &crate::StateInner,
    bounds: Bounds<Pixels>,
) {
    if !COMPILED || !LIST_LAYERS || paint::inside_layer(window) || !active(window, cx) {
        return;
    }
    let version = state.version.clone();
    let id = list_id(window, &version);
    let viewport = window.content_mask().bounds.intersect(&bounds);
    let scroll_top = state.scroll_top(&state.logical_scroll_top());
    let scroll_offset = paint::snap_scroll_offset(window, point(px(0.), -scroll_top));
    // A list lays out its rows itself, and is not taken for changed when a
    // row is measured for the first time: its rows are checked where they
    // are placed.
    let decision = policy::decide(window, cx, &id, bounds, bounds.size, scroll_offset);
    // Remembered as painted, for its scrolls to be told apart.
    invalidate::painted_list(window, &id, &version);
    if decision == Decision::Bypass {
        begin_bypass(window, &id, Some(version.id()));
        return;
    }
    let layer = paint::layer_mut(window, &id);
    let mode = if decision == Decision::Composite && layer.rows.list && layer.record.is_some() {
        Mode::Extend
    } else {
        Mode::Repaint
    };
    let frame = RowsFrame {
        mode,
        list: Some(version.id()),
        skip_held: mode == Mode::Extend && state.pending_scroll.is_none(),
        needed: 0..0,
        keep: 0..0,
        slots: BTreeMap::new(),
        viewport,
        painted_region: viewport,
        scroll_offset,
        translation: Point::default(),
        prepaint_start: window.prepaint_index(),
        prepaint_range: window.prepaint_index()..window.prepaint_index(),
        recording: Some(cx.begin_recording_dependencies()),
        dependencies: RenderDependencies::default(),
        order: Vec::new(),
        next: 0,
        anchor: None,
        prepainted: Vec::new(),
        extra: Vec::new(),
        paint: None,
    };
    window.fast_layers.painting = Some(marker(&id, &frame));
    paint::layer_mut(window, &id).rows.frame = Some(frame);
    if mode == Mode::Extend {
        keep_reads(window, cx, &id);
    }
}

/// Whether a `list`, whose state `version` counts changes of, laying out the
/// rows it shows, leaves out its row `ix`, whose size it knows when
/// `measured`: the frame only scrolled its layer, which holds the row.
pub(crate) fn keeps_row(
    window: &Window,
    version: &StateVersion,
    ix: usize,
    measured: bool,
) -> bool {
    if !LIST_LAYERS {
        return false;
    }
    if !measured {
        return false;
    }
    let Some(painting) = window.fast_layers.painting.as_ref() else {
        return false;
    };
    let Some(layer) = window.fast_layers.layers.get(&painting.id) else {
        return false;
    };
    layer.rows.frame.as_ref().is_some_and(|frame| {
        frame.list == Some(version.id()) && frame.skip_held && layer.rows.painted.contains(&ix)
    })
}

/// Moves `origin`, where a `list` of `state` scrolled to `scroll_top` is
/// about to place its first row shown, up by how far it is scrolled into
/// that row, as the list does, but so that the list's offset from its first
/// row is a whole number of device pixels where layers are compiled (see
/// [`paint::snap_scroll_offset`]); and notes where the row lies, for the
/// list's layer.
pub(crate) fn snap_item_origin(
    window: &mut Window,
    state: &crate::StateInner,
    scroll_top: &crate::ListOffset,
    origin: &mut Point<Pixels>,
) {
    if !LIST_LAYERS {
        origin.y -= scroll_top.offset_in_item;
        return;
    }
    let offset = state.scroll_top(scroll_top);
    let snapped = paint::snap_scroll_offset(window, point(px(0.), offset)).y;
    origin.y -= scroll_top.offset_in_item + (snapped - offset);
    if let Some(frame) = list_frame(window, &state.version) {
        frame.anchor = Some((scroll_top.item_ix, *origin));
        frame.prepainted.clear();
    }
}

/// Moves `origin`, where a `list` is about to prepaint its row `ix`, to
/// where the row lies, when it leaves out the rows its layer holds and so
/// cannot add up the rows before it.
pub(crate) fn place_list_item(
    window: &mut Window,
    state: &crate::StateInner,
    ix: usize,
    origin: &mut Point<Pixels>,
) {
    if !LIST_LAYERS {
        return;
    }
    let Some(id) = window.fast_layers.painting.as_ref().map(|p| p.id.clone()) else {
        return;
    };
    let scale_factor = window.scale_factor();
    let Some(layer) = window.fast_layers.layers.get_mut(&id) else {
        return;
    };
    let rows = &mut layer.rows;
    let Some(frame) = rows.frame.as_mut() else {
        return;
    };
    if frame.list != Some(state.version.id()) {
        return;
    }
    frame.prepainted.push(ix);
    let Some((anchor, anchor_origin)) = frame.anchor else {
        return;
    };
    let heights = |row: usize| row_height(state, &rows.rows, row, scale_factor);
    if let Some(y) = row_top(anchor, anchor_origin.y, ix, heights) {
        origin.y = y;
    }
}

/// The height of a `list`'s row `row`: as the list measured it, or as the
/// layer holds it.
fn row_height(
    state: &crate::StateInner,
    rows: &BTreeMap<usize, Row>,
    row: usize,
    scale_factor: f32,
) -> Option<Pixels> {
    let mut cursor = state.items.cursor::<crate::Count>(());
    cursor.seek(&crate::Count(row), sum_tree::Bias::Right);
    cursor
        .item()
        .and_then(|item| item.size())
        .map(|size| size.height)
        .or_else(|| {
            rows.get(&row)
                .map(|held| px(held.slot.size.height.0 / scale_factor))
        })
}

/// The top of row `ix` of a list whose row `anchor` lies at `anchor_top`,
/// adding up the heights of the rows between them in order, as the list
/// does.
fn row_top(
    anchor: usize,
    anchor_top: Pixels,
    ix: usize,
    mut height: impl FnMut(usize) -> Option<Pixels>,
) -> Option<Pixels> {
    let mut top = anchor_top;
    if ix >= anchor {
        for row in anchor..ix {
            top += height(row)?;
        }
    } else {
        for row in (ix..anchor).rev() {
            top -= height(row)?;
        }
    }
    Some(top)
}

/// Ends the layout and prepaint of the rows of the `list` of `state`, laid
/// out at `bounds`: the rows around those it showed are rendered and
/// prepainted into its layer with `render_item`, as far as the layer is to
/// hold them.
pub(crate) fn end_list(
    window: &mut Window,
    cx: &mut App,
    state: &crate::StateInner,
    render_item: &mut crate::RenderItemFn,
    bounds: Bounds<Pixels>,
) {
    if !LIST_LAYERS {
        return;
    }
    let bypassed = window.fast_layers.layers.iter().find_map(|(id, layer)| {
        let (list, _) = layer.rows.bypass.as_ref()?;
        (*list == Some(state.version.id())).then(|| id.clone())
    });
    if let Some(id) = bypassed {
        finish_bypass(window, &id);
    }
    let Some(id) = window.fast_layers.painting.as_ref().map(|p| p.id.clone()) else {
        return;
    };
    let scale_factor = window.scale_factor();
    let Some(layer) = window.fast_layers.layers.get_mut(&id) else {
        return;
    };
    let rows = &mut layer.rows;
    let Some(frame) = rows.frame.as_mut() else {
        return;
    };
    if frame.list != Some(state.version.id()) {
        return;
    }
    let Some((anchor, anchor_origin)) = frame.anchor else {
        // No row was shown: nothing is painted into the layer.
        window.fast_layers.painting = None;
        finish_prepaint(window, cx, &id);
        if let Some(layer) = window.fast_layers.layers.get_mut(&id) {
            layer.rows.frame = None;
        }
        return;
    };
    let item_count = state.items.summary().count;
    let viewport = frame.viewport;

    // The rows the layer holds stay where they are only if the first row
    // shown lies at a whole number of device pixels from where the layer
    // holds it, and every row it holds is as tall as the list measures it;
    // and, once the rows around it are placed, if each row it keeps lands on
    // the pixels it would painted afresh (see [`held_rows_land_alike`]).
    let mut translation = None;
    if frame.mode == Mode::Extend
        && let Some(held) = rows.rows.get(&anchor)
    {
        let x = anchor_origin.x.0 * scale_factor - held.slot.origin.x.0;
        let y = anchor_origin.y.0 * scale_factor - held.slot.origin.y.0;
        let whole = |value: f32| (value - value.round()).abs() < 0.01;
        let heights_hold = rows.rows.iter().all(|(row, held)| {
            let mut cursor = state.items.cursor::<crate::Count>(());
            cursor.seek(&crate::Count(*row), sum_tree::Bias::Right);
            cursor
                .item()
                .and_then(|item| item.size())
                .is_none_or(|size| {
                    (size.height.0 * scale_factor - held.slot.size.height.0).abs() < 0.01
                })
        });
        let in_list = rows.painted.last().is_none_or(|last| *last < item_count);
        if whole(x) && whole(y) && heights_hold && in_list {
            translation = Some(point(ScaledPixels(x.round()), ScaledPixels(y.round())));
        }
    }
    let frame = rows.frame.as_mut().unwrap();
    if translation.is_none() {
        frame.mode = Mode::Repaint;
    }
    frame.translation = translation.unwrap_or_default();

    // The rows shown and a viewport's height of rows on each side.
    let extent = viewport.size.height * paint::OVERSCAN_VIEWPORTS;
    let (top, bottom) = (viewport.top() - extent, viewport.bottom() + extent);
    let available = crate::size(
        AvailableSpace::Definite(bounds.size.width),
        AvailableSpace::MinContent,
    );
    let mut rendered: FxHashMap<usize, AnyElement> = FxHashMap::default();
    let mut tops: BTreeMap<usize, (Pixels, Pixels)> = BTreeMap::new();
    let mut height_of = |row: usize,
                         window: &mut Window,
                         cx: &mut App,
                         rendered: &mut FxHashMap<usize, AnyElement>|
     -> Pixels {
        let rows = &window.fast_layers.layers[&id].rows.rows;
        if let Some(height) = row_height(state, rows, row, scale_factor) {
            return height;
        }
        let mut element = render_item(row, window, cx);
        let size =
            crate::fast::layout_key::layout_as_list_item(&mut element, row, available, window, cx);
        rendered.insert(row, element);
        size.height
    };
    let mut y = anchor_origin.y;
    let mut row = anchor;
    while row < item_count && y < bottom {
        let height = height_of(row, window, cx, &mut rendered);
        tops.insert(row, (y, height));
        y += height;
        row += 1;
    }
    let end = row;
    let mut y = anchor_origin.y;
    let mut row = anchor;
    while row > 0 && y > top {
        row -= 1;
        let height = height_of(row, window, cx, &mut rendered);
        y -= height;
        tops.insert(row, (y, height));
    }
    let needed = row..end;

    let layer = &mut window.fast_layers.layers.get_mut(&id).unwrap().rows;
    let due_for_repaint = layer.due_for_repaint(&needed);
    let frame = layer.frame.as_mut().unwrap();
    if frame.mode == Mode::Extend
        && (due_for_repaint
            || !held_rows_land_alike(&layer.rows, &needed, &tops, frame.translation, scale_factor))
    {
        frame.mode = Mode::Repaint;
    }
    let held = match frame.mode {
        Mode::Extend => layer.painted.clone(),
        Mode::Repaint => BTreeSet::new(),
    };
    let plan = plan(&held, needed.clone());
    let slot = |(top, height): (Pixels, Pixels)| Bounds {
        origin: point(anchor_origin.x, top),
        size: crate::size(bounds.size.width, height),
    };
    frame.slots = plan
        .render
        .iter()
        .flat_map(|run| run.clone())
        .map(|row| (row, slot(tops[&row])))
        .collect();
    frame.keep = plan.keep;
    frame.needed = needed;
    let mut painted_region = viewport;
    if let (Some(first), Some(last)) = (tops.values().next(), tops.values().last()) {
        painted_region = painted_region.union(&slot(*first)).union(&slot(*last));
    }
    frame.painted_region = painted_region;
    if frame.mode == Mode::Repaint {
        // Content space is window space as the rows are painted now.
        frame.translation = Point::default();
    }
    let prepainted: BTreeSet<usize> = frame.prepainted.iter().copied().collect();
    let to_prepaint: Vec<(usize, Pixels)> = frame
        .slots
        .iter()
        .filter(|(row, _)| !prepainted.contains(row))
        .map(|(row, slot)| (*row, slot.origin.y))
        .collect();
    if let Some(painting) = window.fast_layers.painting.as_mut() {
        painting.painted_region = painted_region;
    }

    let mut extra = Vec::with_capacity(to_prepaint.len());
    let mut unplaced = Vec::new();
    for (row, top) in to_prepaint {
        let mut element = match rendered.remove(&row) {
            Some(element) => element,
            None => {
                let mut element = render_item(row, window, cx);
                let size = crate::fast::layout_key::layout_as_list_item(
                    &mut element,
                    row,
                    available,
                    window,
                    cx,
                );
                let (_, height) = tops[&row];
                let hidden = top + height <= viewport.top() || top >= viewport.bottom();
                if hidden && (size.height.0 - height.0).abs() * scale_factor >= 0.01 {
                    // The list's size for the row is out of date (it was
                    // measured at another scale, say), and the list corrects
                    // it when it shows the row: the layer does not hold a row
                    // where it would not be then.
                    unplaced.push(row);
                    continue;
                }
                element
            }
        };
        let origin = point(anchor_origin.x, top);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            element.prepaint_at(origin, window, cx);
        });
        extra.push((row, element));
    }
    if !extra.is_empty() {
        // Rows the list does not show ask for no autoscroll.
        window.take_autoscroll();
    }
    window.fast_layers.painting = None;
    finish_prepaint(window, cx, &id);
    if let Some(frame) = frame_mut(window, &id) {
        frame.extra = extra;
        for row in unplaced {
            frame.slots.remove(&row);
        }
    }
}

/// Whether each row of `held` that a `list` keeps, those of `needed`, lands
/// on the pixels it would if painted afresh when its content is moved by
/// `translation`: the row lies at `tops` (its top and height), at the same
/// fraction of a device pixel as it does moved, and neither its top nor its
/// bottom lies half way between device pixels.
///
/// A row's edges are rounded to device pixels as the row is painted, half
/// way toward zero. Rounding a position and then moving it by whole device
/// pixels lands where rounding the moved position does, unless the position
/// is half way: its rounding then depends on its sign, which the move can
/// flip, and on the last bits of how the list added up the rows' heights,
/// which differ from frame to frame. Rows whose height is not a whole number
/// of device pixels (30 px at a scale of 1.25) put edges there.
fn held_rows_land_alike(
    held: &BTreeMap<usize, Row>,
    needed: &Range<usize>,
    tops: &BTreeMap<usize, (Pixels, Pixels)>,
    translation: Point<ScaledPixels>,
    scale_factor: f32,
) -> bool {
    const EPSILON: f32 = 0.01;
    let half_way = |value: f32| ((value - value.floor()) - 0.5).abs() < EPSILON;
    held.range(needed.clone()).all(|(row, held)| {
        let Some((top, height)) = tops.get(row) else {
            return false;
        };
        let top = top.0 * scale_factor;
        let bottom = top + height.0 * scale_factor;
        let moved = held.slot.origin.y.0 + translation.y.0;
        (moved - top).abs() < EPSILON && !half_way(top) && !half_way(bottom)
    })
}

/// Starts painting the rows of the `list` of `state`.
pub(crate) fn begin_paint_list(window: &mut Window, cx: &mut App, state: &crate::ListState) {
    if !LIST_LAYERS {
        return;
    }
    if !COMPILED || window.fast_layers.layers.is_empty() {
        return;
    }
    let version = state.0.borrow().version.clone();
    let id = list_id(window, &version);
    begin_paint_rows(window, cx, Some(&id));
}

/// Ends painting the rows of the `list` of `state`: the rows rendered for
/// its layer alone are painted, and the layer is composited.
pub(crate) fn end_paint_list(window: &mut Window, cx: &mut App, state: &crate::ListState) {
    if !LIST_LAYERS {
        return;
    }
    if !COMPILED || window.fast_layers.layers.is_empty() {
        return;
    }
    let version = state.0.borrow().version.clone();
    let id = list_id(window, &version);
    let Some(frame) = frame_mut(window, &id) else {
        return;
    };
    let extra = mem::take(&mut frame.extra);
    for (row, mut element) in extra {
        paint_row(window, cx, Some(row), |window, cx| {
            element.paint(window, cx)
        });
    }
    end_paint_rows(window, cx, Some(&id));
}

/// Starts painting the rows of the list `id` as its prepaint decided.
pub(crate) fn begin_paint_rows(window: &mut Window, cx: &mut App, id: Option<&GlobalElementId>) {
    if !LIST_LAYERS {
        return;
    }
    let Some(id) = id else {
        return;
    };
    if window.fast_layers.layers.is_empty() {
        return;
    }
    let Some(frame) = frame_mut(window, id) else {
        return;
    };
    if frame.paint.is_some() {
        return;
    }
    let painted_region = frame.painted_region;
    let has_rows = !frame.slots.is_empty();
    let background = if has_rows {
        paint::bake_background(window)
    } else {
        None
    };
    let mut state = PaintState {
        background,
        swapped: background.is_some(),
        scene: Scene::default(),
        paint_start: window.paint_index(),
        hovers_start: 0,
        recording: None,
        current: None,
        spans: Vec::new(),
    };
    if state.swapped {
        window.content_mask_stack.push(ContentMask {
            bounds: painted_region,
        });
        mem::swap(&mut window.next_frame.scene, &mut state.scene);
        state.paint_start = window.paint_index();
        window.take_hover_reads();
        state.hovers_start = window.retained_state.hover_dependencies.len();
        state.recording = Some(cx.begin_recording_dependencies());
    }
    let frame = frame_mut(window, id).unwrap();
    let marker = marker(id, frame);
    frame.paint = Some(state);
    window.fast_layers.painting = Some(marker);
}

/// Paints a row of a list with `f`: into the list's layer if its prepaint
/// rendered the row for it, not at all if the layer holds the row, into the
/// frame otherwise. `ix` is the row, or, for a uniform list, the next row it
/// rendered.
pub(crate) fn paint_row(
    window: &mut Window,
    cx: &mut App,
    ix: Option<usize>,
    f: impl FnOnce(&mut Window, &mut App),
) {
    if !LIST_LAYERS {
        return f(window, cx);
    }
    let Some(id) = window.fast_layers.painting.as_ref().map(|p| p.id.clone()) else {
        return f(window, cx);
    };
    let Some(frame) = frame_mut(window, &id) else {
        return f(window, cx);
    };
    let Some(paint) = frame.paint.as_ref() else {
        return f(window, cx);
    };
    if paint.current.is_some() {
        // A row of a list nested in a row being painted.
        return f(window, cx);
    }
    let swapped = paint.swapped;
    let row = match ix {
        Some(row) => row,
        None => {
            let Some(row) = frame.order.get(frame.next).copied() else {
                return f(window, cx);
            };
            frame.next += 1;
            row
        }
    };
    if frame.mode == Mode::Extend && frame.keep.contains(&row) {
        // The layer holds the row as it is.
        return;
    }
    if !swapped {
        return f(window, cx);
    }
    if !frame.slots.contains_key(&row) {
        // Into the frame, around the layer's scene and painted region.
        let region = window.content_mask_stack.pop();
        swap_scenes(window, &id);
        f(window, cx);
        swap_scenes(window, &id);
        window.content_mask_stack.extend(region);
        return;
    }
    let start = window.next_frame.scene.paint_operations.len();
    set_current_row(window, &id, Some(row));
    f(window, cx);
    let end = window.next_frame.scene.paint_operations.len();
    set_current_row(window, &id, None);
    if let Some(paint) = frame_mut(window, &id).and_then(|frame| frame.paint.as_mut()) {
        paint.spans.push((row, start..end));
    }
}

fn swap_scenes(window: &mut Window, id: &GlobalElementId) {
    let Some(layer) = window.fast_layers.layers.get_mut(id) else {
        return;
    };
    if let Some(paint) = layer
        .rows
        .frame
        .as_mut()
        .and_then(|frame| frame.paint.as_mut())
    {
        mem::swap(&mut window.next_frame.scene, &mut paint.scene);
    }
}

fn set_current_row(window: &mut Window, id: &GlobalElementId, row: Option<usize>) {
    if let Some(paint) = frame_mut(window, id).and_then(|frame| frame.paint.as_mut()) {
        paint.current = row;
    }
}

/// Ends painting the rows of the list `id`: the rows painted into its layer
/// are recorded and the layer is composited.
pub(crate) fn end_paint_rows(window: &mut Window, cx: &mut App, id: Option<&GlobalElementId>) {
    if !LIST_LAYERS {
        return;
    }
    let Some(id) = id else {
        return;
    };
    if window.fast_layers.layers.is_empty() {
        return;
    }
    let Some(layer) = window.fast_layers.layers.get_mut(id) else {
        return;
    };
    let Some(mut frame) = layer.rows.frame.take() else {
        return;
    };
    let Some(mut paint) = frame.paint.take() else {
        return;
    };
    window.fast_layers.painting = None;
    if frame.slots.is_empty() {
        match frame.mode {
            // Nothing new: the layer's content stands.
            Mode::Extend => {
                count_extended_frame();
                paint::composite_at(window, id, frame.translation)
            }
            // No row to paint: the layer holds nothing.
            Mode::Repaint => {
                if let Some(layer) = window.fast_layers.layers.get_mut(id) {
                    layer.record = None;
                }
            }
        }
        if window
            .fast_layers
            .layers
            .get(id)
            .is_none_or(|layer| layer.record.is_none())
        {
            clear_rows(window, id);
        }
        return;
    }
    let Some(background) = paint.background else {
        // The rows were painted into the frame: so is what the layer held.
        let layer = window.fast_layers.layers.get_mut(id).unwrap();
        if let Some(record) = layer.record.take()
            && frame.mode == Mode::Extend
        {
            paint::draw_into_frame(window, record.content.operations(), frame.translation);
        }
        clear_rows(window, id);
        return;
    };

    let paint_dependencies = paint
        .recording
        .take()
        .map(|recording| without_owner(window, cx.finish_recording_dependencies(recording).all))
        .unwrap_or_default();
    window.take_hover_reads();
    let new_hovers = window.retained_state.hover_dependencies[paint.hovers_start..].to_vec();
    let paint_end = window.paint_index();
    mem::swap(&mut window.next_frame.scene, &mut paint.scene);
    window.content_mask_stack.pop();
    let painted = paint.scene;

    let scale_factor = window.scale_factor();
    let translation = frame.translation;
    let to_content = point(
        ScaledPixels(-translation.x.0),
        ScaledPixels(-translation.y.0),
    );
    let views = invalidate::content_views(window, &frame.prepaint_range);
    let viewport = window.snapped_content_mask().bounds;
    let paint_range = paint.paint_start..paint_end;
    let adds_input = adds_input(window, &frame.prepaint_range, &paint_range);

    let layer = window.fast_layers.layers.get_mut(id).unwrap();
    let rows = &mut layer.rows;
    match frame.mode {
        Mode::Repaint => rows.clear(),
        Mode::Extend => {
            let needed = frame.needed.clone();
            rows.rows.retain(|row, _| needed.contains(row));
            rows.painted.retain(|row| needed.contains(row));
            rows.row_origins.retain(|row, _| needed.contains(row));
            rows.added_since_repaint += paint.spans.len();
            count_extended_frame();
        }
    }
    rows.holds_input |= adds_input;
    let mut operations: Vec<Option<PaintOperation>> =
        painted.paint_operations.into_iter().map(Some).collect();
    for (row, span) in paint.spans {
        let Some(slot) = frame.slots.get(&row) else {
            continue;
        };
        let slot = slot.scale(scale_factor);
        let slot = Bounds {
            origin: slot.origin + to_content,
            size: slot.size,
        };
        let row_operations = operations[span]
            .iter_mut()
            .filter_map(Option::take)
            .map(|operation| match operation {
                PaintOperation::Primitive(primitive) => {
                    PaintOperation::Primitive(translate_primitive(&primitive, to_content))
                }
                PaintOperation::StartLayer(bounds) => PaintOperation::StartLayer(Bounds {
                    origin: bounds.origin + to_content,
                    size: bounds.size,
                }),
                PaintOperation::EndLayer => PaintOperation::EndLayer,
            })
            .collect();
        rows.painted.insert(row);
        rows.row_origins.insert(
            row,
            point(
                px(slot.origin.x.0 / scale_factor),
                px(slot.origin.y.0 / scale_factor),
            ),
        );
        rows.rows.insert(
            row,
            Row {
                slot,
                operations: row_operations,
            },
        );
    }
    rows.list = true;

    // The content: the rows in order.
    let mut content = Scene::default();
    let mut region = Bounds {
        origin: viewport.origin + to_content,
        size: viewport.size,
    };
    for row in rows.rows.values() {
        region = region.union(&row.slot);
        for operation in &row.operations {
            match operation {
                PaintOperation::Primitive(primitive) => content.insert_primitive(primitive.clone()),
                PaintOperation::StartLayer(bounds) => content.push_layer(*bounds),
                PaintOperation::EndLayer => content.pop_layer(),
            }
        }
    }
    content.finish();
    let has_paths = !content.paths.is_empty();
    let hashes = tile_hashes(&content, paint::TILE_SIZE, region);

    let old = layer.record.take();
    // `rows` borrows the layer's rows: its generation is taken field by
    // field, as `Layer::next_generation` does.
    layer.generation += 1;
    let generation = layer.generation;
    let dirty = match &old {
        Some(old) if old.background == background => dirty_tiles(&old.tile_hashes, &hashes),
        _ => paint::all_tiles(&hashes),
    };
    let (dependencies, hovers, views) = match (&old, frame.mode) {
        (Some(old), Mode::Extend) => {
            let mut hovers = old.hovers.to_vec();
            hovers.extend(new_hovers);
            let mut all_views = old.views.to_vec();
            all_views.extend(views.iter().copied());
            (
                old.dependencies
                    .union(&frame.dependencies)
                    .union(&paint_dependencies),
                hovers.into(),
                all_views.into(),
            )
        }
        _ => (
            frame.dependencies.union(&paint_dependencies),
            new_hovers.into(),
            views,
        ),
    };
    let painted_region = rows.rows.values().fold(frame.viewport, |region, row| {
        let slot = Bounds {
            origin: row.slot.origin - to_content,
            size: row.slot.size,
        };
        region.union(&Bounds {
            origin: point(
                px(slot.origin.x.0 / scale_factor),
                px(slot.origin.y.0 / scale_factor),
            ),
            size: crate::size(
                px(slot.size.width.0 / scale_factor),
                px(slot.size.height.0 / scale_factor),
            ),
        })
    });
    let dirtied = dirty.len();
    layer.record = Some(LayerRecord {
        content: content.into(),
        generation,
        painted_region,
        viewport: frame.viewport,
        scroll_offset: frame.scroll_offset,
        translation,
        prepaint_range: frame.prepaint_range,
        paint_range,
        tile_hashes: hashes,
        dirty_tiles: dirty,
        background,
        hovers,
        dependencies,
        views,
        has_paths,
        paths: Rc::from([]),
        view_layouts: Rc::default(),
    });
    if frame.mode == Mode::Repaint && !has_paths {
        window
            .layout_engine
            .as_mut()
            .unwrap()
            .retention
            .stats
            .layer_frames_repainted += 1;
    }
    paint::insert_layer(window, id, translation, dirtied);
}

/// Forgets the rows of the layer of the list `id`, whose content is gone.
fn clear_rows(window: &mut Window, id: &GlobalElementId) {
    if let Some(layer) = window.fast_layers.layers.get_mut(id) {
        layer.rows.clear();
    }
}
