//! Carrying a layer's non-scene records (hitboxes, listeners, dispatch
//! nodes) through frames that composite it (M5).
//!
//! On a frame that composites a layer, its content is neither prepainted nor
//! painted, but what prepainting and painting it added to the last frame
//! besides the scene is still wanted: its hitboxes, for hit testing, its
//! dispatch nodes, for focus and key and action dispatch, and its mouse
//! listeners, cursor styles and tab stops. [`carry_prepaint`] and
//! [`carry_paint`] copy them from the rendered frame where the container's
//! children would add them, as [`Window::reuse_prepaint`] and
//! [`Window::reuse_paint`] do for a reused view, but without replaying the
//! scene, which the layer's tiles stand for, and with the hitboxes moved by
//! the scroll since the content was painted and clipped to the viewport.

use std::{ops::Range, rc::Rc};

use crate::{
    App, Bounds, EntityId, GlobalElementId, Hitbox, LayoutId, PaintIndex, Pixels, Point,
    PrepaintStateIndex, Window,
    fast::{dependencies::RenderDependencies, layers::input::LayerInput, retained::RetainedLayout},
};

/// Carries the prepaint records of the content of the layer of the
/// container `id`, composited at `scroll_offset` in `viewport`, into the
/// frame being drawn, where prepainting the content would add them.
pub(crate) fn carry_prepaint(
    window: &mut Window,
    id: &GlobalElementId,
    viewport: Bounds<Pixels>,
    scroll_offset: Point<Pixels>,
) {
    let Some(layer) = window.fast_layers.layers.get_mut(id) else {
        return;
    };
    let Some(record) = layer.record.as_mut() else {
        return;
    };
    let delta = scroll_offset - record.scroll_offset;
    let range = record.prepaint_range.clone();
    let input = &mut layer.input;
    let moved = delta != input.stale;
    input.stale = delta;
    input.viewport = viewport;
    input.handle_offset.set(delta);
    // The hitboxes are the layer's, lent to the carry while it runs.
    let hitboxes = std::mem::take(&mut input.hitboxes);
    let carried = carry_prepaint_records(window, &range, &hitboxes, delta, viewport, !moved, true);
    if let Some(layer) = window.fast_layers.layers.get_mut(id) {
        layer.input.hitboxes = hitboxes;
        if let Some(record) = layer.record.as_mut() {
            record.prepaint_range = carried;
        }
    }
}

/// Carries the prepaint records the rendered frame holds over `range` into
/// the frame being drawn, where prepainting what added them would add them
/// again, returning where they lie in it: `hitboxes`, which stand for those
/// of the range, moved by `delta` and clipped to `viewport`; its tooltips if
/// `tooltips`, a tooltip showing where its element was when it was
/// requested; its element states if `element_states`, which are dropped
/// otherwise; its line layouts and dispatch nodes.
#[inline]
pub(crate) fn carry_prepaint_records(
    window: &mut Window,
    range: &Range<PrepaintStateIndex>,
    hitboxes: &[Hitbox],
    delta: Point<Pixels>,
    viewport: Bounds<Pixels>,
    tooltips: bool,
    element_states: bool,
) -> Range<PrepaintStateIndex> {
    let start = window.prepaint_index();
    let next = &mut window.next_frame;
    let rendered = &mut window.rendered_frame;
    next.hitboxes
        .extend(LayerInput::hitboxes_at(hitboxes, delta, viewport));
    // A scroll hides tooltips (spec §7, rule 5).
    let requests =
        &mut rendered.tooltip_requests[range.start.tooltips_index..range.end.tooltips_index];
    if tooltips {
        next.tooltip_requests
            .extend(requests.iter_mut().map(|request| request.take()));
    }
    if element_states {
        next.accessed_element_states.extend(
            rendered.accessed_element_states[range.start.accessed_element_states_index
                ..range.end.accessed_element_states_index]
                .iter()
                .cloned(),
        );
    }
    window
        .text_system
        .reuse_layouts(range.start.line_layout_index.clone()..range.end.line_layout_index.clone());
    let subtree = next.dispatch_tree.reuse_subtree(
        range.start.dispatch_tree_index..range.end.dispatch_tree_index,
        &mut rendered.dispatch_tree,
        window.focus,
    );
    if subtree.contains_focus() {
        next.focus = window.focus;
    }
    // Content that deferred draws is never composited (spec §6.5).
    debug_assert_eq!(
        range.start.deferred_draws_index,
        range.end.deferred_draws_index
    );
    start..window.prepaint_index()
}

/// Carries the paint records of the content of the layer of the container
/// `id` into the frame being drawn, where painting the content would add
/// them: what [`carry_prepaint`] carried the prepaint records of.
pub(crate) fn carry_paint(window: &mut Window, id: &GlobalElementId) {
    let frame = window.fast_layers.frame;
    let Some(layer) = window.fast_layers.layers.get_mut(id) else {
        return;
    };
    let Some(record) = layer.record.as_ref() else {
        return;
    };
    let range = record.paint_range.clone();
    let delta = layer.input.stale;
    let viewport = layer.input.viewport;
    let mut carried = carry_paint_records(window, &range, delta, viewport, true);
    let Some(layer) = window.fast_layers.layers.get_mut(id) else {
        return;
    };
    if let Some(record) = layer.record.as_mut() {
        // The content's scene is the layer's, which is not carried.
        carried.start.scene_index = record.paint_range.start.scene_index;
        carried.end.scene_index = record.paint_range.end.scene_index;
        record.paint_range = carried;
    }
    layer.input.ranges_frame = Some(frame);
}

/// Carries the paint records the rendered frame holds over `range` into
/// the frame being drawn, where painting what added them would add them
/// again, returning where they lie in it: its window control hitboxes,
/// moved by `delta` and clipped to `viewport`, cursor styles, input
/// handlers, mouse listeners, element states if `element_states`, tab stops
/// and line layouts. The scene is not carried.
#[inline]
pub(crate) fn carry_paint_records(
    window: &mut Window,
    range: &Range<PaintIndex>,
    delta: Point<Pixels>,
    viewport: Bounds<Pixels>,
    element_states: bool,
) -> Range<PaintIndex> {
    let start = window.paint_index();
    let next = &mut window.next_frame;
    let rendered = &mut window.rendered_frame;
    next.window_control_hitboxes.extend(
        rendered.window_control_hitboxes[range.start.fast_window_control_hitboxes_index
            ..range.end.fast_window_control_hitboxes_index]
            .iter()
            .map(|(area, hitbox)| {
                let moved = LayerInput::hitboxes_at(std::slice::from_ref(hitbox), delta, viewport)
                    .next()
                    .expect("one hitbox");
                (*area, moved)
            }),
    );
    next.cursor_styles.extend(
        rendered.cursor_styles[range.start.cursor_styles_index..range.end.cursor_styles_index]
            .iter()
            .cloned(),
    );
    next.input_handlers.extend(
        rendered.input_handlers[range.start.input_handlers_index..range.end.input_handlers_index]
            .iter_mut()
            .map(|handler| handler.take()),
    );
    next.mouse_listeners.extend(
        rendered.mouse_listeners
            [range.start.mouse_listeners_index..range.end.mouse_listeners_index]
            .iter_mut()
            .map(|listener| listener.take()),
    );
    if element_states {
        next.accessed_element_states.extend(
            rendered.accessed_element_states[range.start.accessed_element_states_index
                ..range.end.accessed_element_states_index]
                .iter()
                .cloned(),
        );
    }
    next.tab_stops.replay(
        &rendered.tab_stops.insertion_history
            [range.start.tab_handle_index..range.end.tab_handle_index],
    );
    window
        .text_system
        .reuse_layouts(range.start.line_layout_index.clone()..range.end.line_layout_index.clone());
    start..window.paint_index()
}

/// How a view drawn inside a layer's content was laid out when the content
/// was painted, for frames that composite the layer to lay it out again
/// without rendering it.
///
/// Views inside a layer's content keep no retained record of their own (the
/// layer is their retention, see [`crate::fast::layers::paint::inside_layer`]),
/// and a frame that composites the layer neither prepaints nor paints them;
/// but the view holding the container renders it again, and lays out its
/// children, a child view among them (pattern A). Without a record to reuse,
/// the child view would render every frame; its layer keeps its layout
/// instead.
pub(crate) struct KeptLayout {
    pub(crate) layout: Rc<RetainedLayout>,
    /// Everything laying the view out and prepainting it read.
    pub(crate) dependencies: RenderDependencies,
}

/// Keeps how the view `id`, prepainted inside the content of the layer being
/// painted, was laid out, and what laying it out and prepainting it read.
pub(crate) fn keep_view_layout(
    window: &mut Window,
    id: GlobalElementId,
    layout: Rc<RetainedLayout>,
    dependencies: RenderDependencies,
) {
    if let Some(painting) = window.fast_layers.painting.as_mut() {
        painting.view_layouts.insert(
            id,
            KeptLayout {
                layout,
                dependencies,
            },
        );
    }
}

/// Lays out the view `id`, of entity `entity`, which is not dirty, as the
/// layer whose content it is drawn in kept it, if one kept it and nothing it
/// read changed since, returning its node and the layout: the frame is then
/// expected to composite the layer, and if it paints the content afresh
/// after all, the view is rendered at that layout where it is prepainted.
pub(crate) fn reuse_kept_layout(
    window: &mut Window,
    id: &GlobalElementId,
    entity: EntityId,
    cx: &mut App,
) -> Option<(LayoutId, Rc<RetainedLayout>)> {
    if window.fast_layers.layers.is_empty()
        || crate::fast::layers::paint::inside_layer(window)
        || !crate::fast::layers::active(window, cx)
        || window.retained_state.notified_entities.contains(&entity)
        || window.retained_state.dirty_subtrees.contains(id)
    {
        return None;
    }
    let kept = window.fast_layers.layers.values().find_map(|layer| {
        let record = layer.record.as_ref()?;
        record.view_layouts.get(id)
    })?;
    let layout = kept.layout.clone();
    if layout.rem_size != window.rem_size()
        || layout.text_style != window.text_style()
        || cx.dependencies_changed(&kept.dependencies, window.inside_notified_view())
        || crate::fast::layers::invalidate::offset_read_changed(window, &kept.dependencies)
    {
        return None;
    }
    let dependencies = kept.dependencies.clone();
    if !window
        .layout_engine
        .as_mut()
        .unwrap()
        .try_keep_retained(&layout.keys)
    {
        return None;
    }
    window
        .next_frame
        .accessed_element_states
        .extend(layout.element_states.iter().cloned());
    cx.replay_dependencies(&dependencies);
    Some((layout.root, layout))
}
