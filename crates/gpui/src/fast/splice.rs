//! Drawing a view again from last frame around the views nested in it that
//! have to be built again.
//!
//! Notifying a view marks every view around it dirty, because they have to be
//! walked to reach it. Upstream builds them all again; a retained view that is
//! dirty only because something nested in it is — it was not notified, and
//! nothing it read itself changed — is instead drawn again from last frame,
//! stretch by stretch, with the nested views that changed built again in the
//! gaps where they were drawn:
//!
//! - its layout is last frame's, and each nested view that changed is laid
//!   out again at its own nodes. If one of them asks for another layout — a
//!   node created, a style or a child list rewritten, a measurement that no
//!   longer stands — the view around it is built after all, as before;
//! - its prepaint and paint are copied from last frame up to where each
//!   nested view began, the nested view is prepainted and painted where it
//!   was, with what it inherited there, and the copy goes on after it.
//!
//! A nested view can be built on its own only if it can be rendered without
//! the view around it: [`RebuildHandle`] holds it as an [`AnyView`] when it
//! was placed as an entity or an `AnyView`, and [`Rebuild`] what it inherited.

use crate::fast::dependencies::RenderDependencies;
use crate::fast::retained::{EnclosingRetained, OpenPaint};
use crate::fast::retained::{
    PaintStatus, RetainedSubtree, ViewLayoutState, ViewPrepaint, ViewPrepaintState,
};
use crate::key_dispatch::{DispatchNodeId, DispatchTree};
use crate::window::DeferredDraw;
use crate::window::{PaintIndex, PrepaintStateIndex};
use crate::{
    AnyView, App, ContentMask, ElementId, EntityId, FocusId, GlobalElementId, HitboxId, LayoutId,
    Pixels, Point, TextStyleRefinement, View, ViewElement, Window,
};
use collections::FxHashSet;
use smallvec::SmallVec;
use std::{mem, ops::Range};

/// The view a [`ViewElement`] renders, kept so that it can be built again on
/// its own. Empty for a view that is not an entity or an [`AnyView`].
#[derive(Default)]
pub(crate) struct RebuildHandle(Option<AnyView>);

impl<V: View> ViewElement<V> {
    /// Keeps `view`, which renders what this element does, to build it again
    /// on its own. See [`crate::fast::splice`].
    pub(crate) fn rebuildable(mut self, view: AnyView) -> Self {
        self.rebuild = RebuildHandle(Some(view));
        self
    }
}

/// What it takes to build a view again on its own, where it was drawn.
pub(crate) struct Rebuild {
    view: AnyView,
    /// The layout key its element was requested under, which its own key,
    /// and the keys of its nodes, are derived from.
    parent_layout_key: u64,
    text_style_stack: Vec<TextStyleRefinement>,
    element_offset: Point<Pixels>,
    rem_size: Pixels,
}

impl Rebuild {
    /// The layout key the view's element was requested under.
    pub(crate) fn parent_layout_key(&self) -> u64 {
        self.parent_layout_key
    }
}

impl Window {
    /// How the view whose [`RebuildHandle`] is `handle` can be built again on
    /// its own, from here, if it can: its element was requested under
    /// `parent_layout_key`, and nothing it inherits is out of reach.
    pub(crate) fn rebuild_here(
        &self,
        handle: &RebuildHandle,
        parent_layout_key: Option<u64>,
    ) -> Option<Rebuild> {
        if !self.image_cache_stack.is_empty() {
            return None;
        }
        Some(Rebuild {
            view: handle.0.clone()?,
            parent_layout_key: parent_layout_key?,
            text_style_stack: self.text_style_stack.clone(),
            element_offset: self.element_offset(),
            rem_size: self.rem_size(),
        })
    }
}

/// A nested view built again inside a view drawn from last frame, drawn the
/// way its element would be inside the view around it.
pub(crate) struct Gap {
    /// Its record in the last frame.
    record: usize,
    global_id: GlobalElementId,
    view: ViewElement<AnyView>,
    layout: ViewLayoutState,
    layout_id: LayoutId,
    layout_key: u64,
    /// The layout nodes it claimed.
    claimed: Vec<u64>,
    /// The element states it used and what it read while its layout was
    /// requested, which the view around it takes over if it is built after
    /// all.
    element_states: Vec<(GlobalElementId, std::any::TypeId)>,
    dependencies: RenderDependencies,
    /// Its dispatch node and what its prepaint left, once prepainted.
    prepainted: Option<(DispatchNodeId, ViewPrepaintState)>,
}

impl Gap {
    fn element_id(&self) -> ElementId {
        self.global_id.0.last().cloned().expect("a view has an id")
    }
}

/// A nested view built for a splice that did not happen, with what the view
/// around it takes over from it. See [`Window::take_prebuilt`].
pub(crate) struct PrebuiltGap {
    prebuilt: Prebuilt,
    layout_id: LayoutId,
    claimed: Vec<u64>,
    element_states: Vec<(GlobalElementId, std::any::TypeId)>,
    dependencies: RenderDependencies,
}

/// A nested view built for a view that could not be drawn around it after
/// all, taken over by its element when the view around it is built.
pub(crate) struct Prebuilt {
    view: ViewElement<AnyView>,
    layout: ViewLayoutState,
}

impl Prebuilt {
    pub(crate) fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        bounds: crate::Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> ViewPrepaintState {
        self.view
            .prepaint_view(global_id, bounds, &mut self.layout, window, cx)
    }

    pub(crate) fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        prepaint: &mut ViewPrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.view.paint_view(global_id, prepaint, window, cx)
    }
}

/// A view drawn from last frame around the nested views built again in it.
pub(crate) struct Splice {
    previous: usize,
    gaps: Vec<Gap>,
}

impl Splice {
    /// The record last frame's view is drawn again from.
    pub(crate) fn previous(&self) -> usize {
        self.previous
    }
}

/// What [`Window::splice_prepaint`] leaves for [`Window::splice_paint`].
pub(crate) struct SplicedPrepaint {
    previous: usize,
    /// Its record in this frame.
    index: usize,
    gaps: Vec<Gap>,
    /// The records copied along with it, in this frame, each with the
    /// stretch of last frame's range, between gaps, it was copied from.
    copied: Vec<(usize, usize)>,
}

/// What the window inherits at some point of the element tree, set aside while
/// a nested view is built where it was.
struct Inherited {
    element_id_stack: SmallVec<[ElementId; 32]>,
    text_style_stack: Vec<TextStyleRefinement>,
    content_mask_stack: Vec<ContentMask<Pixels>>,
    element_offset_stack: Vec<Point<Pixels>>,
    rem_size_override_stack: SmallVec<[Pixels; 8]>,
    element_opacity: f32,
}

impl Window {
    /// Puts the window where the view with id `id` was drawn last frame,
    /// returning what to put back with [`Window::leave_gap`].
    fn enter_gap(
        &mut self,
        id: &GlobalElementId,
        rebuild: &Rebuild,
        content_mask: ContentMask<Pixels>,
        opacity: f32,
    ) -> Inherited {
        let parent_ids = &id.0[..id.0.len() - 1];
        Inherited {
            element_id_stack: mem::replace(
                &mut self.element_id_stack,
                parent_ids.iter().cloned().collect(),
            ),
            text_style_stack: mem::replace(
                &mut self.text_style_stack,
                rebuild.text_style_stack.clone(),
            ),
            content_mask_stack: mem::replace(&mut self.content_mask_stack, vec![content_mask]),
            element_offset_stack: mem::replace(
                &mut self.element_offset_stack,
                vec![rebuild.element_offset],
            ),
            rem_size_override_stack: mem::replace(
                &mut self.rem_size_override_stack,
                SmallVec::from_slice(&[rebuild.rem_size]),
            ),
            element_opacity: mem::replace(&mut self.element_opacity, opacity),
        }
    }

    fn leave_gap(&mut self, inherited: Inherited) {
        self.element_id_stack = inherited.element_id_stack;
        self.text_style_stack = inherited.text_style_stack;
        self.content_mask_stack = inherited.content_mask_stack;
        self.element_offset_stack = inherited.element_offset_stack;
        self.rem_size_override_stack = inherited.rem_size_override_stack;
        self.element_opacity = inherited.element_opacity;
    }

    /// Lays out the view `id`, dirty only because views nested in it are, as
    /// it was laid out last frame, with those views built again at their own
    /// nodes, if it can be: nothing it read itself changed, it is hovered as
    /// it was, and the nested views ask for the layout they had.
    pub(crate) fn splice_layout(&mut self, id: &GlobalElementId, cx: &mut App) -> Option<Splice> {
        if self.refreshing
            || cx.has_active_drag()
            || self.a11y.is_active()
            || self.is_inspector_picking(cx)
            || self.retained_state.dirty_subtrees.contains(id)
            || self.next_frame.retained.by_id.contains_key(id)
        {
            return None;
        }
        let entity = view_entity(id)?;
        if self.retained_state.notified_entities.contains(&entity) {
            return None;
        }
        let previous = self.rendered_frame.retained.find(id)?;
        let records = &self.rendered_frame.retained.records;
        let record = &records[previous];
        let layout = record.layout.as_ref()?;
        if layout.rem_size != self.rem_size()
            || layout.text_style != self.text_style()
            || cx.dependencies_changed(
                &record.own_dependencies,
                &self.retained_state.notified_entities,
            )
            || !self.hovers_unchanged(&record.own_hovers)
        {
            return None;
        }

        // The outermost dirty views nested in it are the gaps. Every view
        // around a dirty one is dirty too, so a clean one holds none.
        let mut gaps = Vec::new();
        let mut index = previous + 1;
        while index <= previous + record.nested {
            let nested = &records[index];
            if view_entity(&nested.id).is_some_and(|entity| self.dirty_views.contains(&entity)) {
                if nested.rebuild.is_none()
                    || nested.layout.is_none()
                    || !matches!(nested.paint, PaintStatus::Painted { .. })
                {
                    return None;
                }
                gaps.push(index);
            }
            index += nested.nested + 1;
        }
        if gaps.is_empty() {
            return None;
        }

        let gap_keys: FxHashSet<u64> = gaps
            .iter()
            .flat_map(|&gap| records[gap].layout.as_ref().unwrap().keys.iter().copied())
            .collect();
        let kept: Vec<u64> = layout
            .keys
            .iter()
            .copied()
            .filter(|key| !gap_keys.contains(key))
            .collect();
        let element_states = layout.element_states.clone();
        let dependencies = record.dependencies.clone();
        let engine = self.layout_engine.as_mut().unwrap();
        if !engine.try_keep_retained(&kept) {
            return None;
        }

        let mut built = Vec::with_capacity(gaps.len());
        let mut unchanged = true;
        for gap in gaps {
            let (gap, gap_unchanged) = self.lay_out_gap(gap, cx)?;
            built.push(gap);
            if !gap_unchanged {
                unchanged = false;
                break;
            }
        }
        if !unchanged {
            // The view around them is built after all; the views built so
            // far are taken over by their elements there, not built twice.
            self.layout_engine.as_mut().unwrap().release_kept(&kept);
            self.stash_gaps(built);
            return None;
        }

        self.next_frame
            .accessed_element_states
            .extend(element_states);
        cx.replay_dependencies(&dependencies);
        Some(Splice {
            previous,
            gaps: built,
        })
    }

    /// Builds the view last frame's record `record` stands for again, where
    /// it was, and lays it out, returning it and whether it asked for the
    /// layout it had.
    fn lay_out_gap(&mut self, record: usize, cx: &mut App) -> Option<(Gap, bool)> {
        let nested = &self.rendered_frame.retained.records[record];
        let rebuild = nested.rebuild.clone()?;
        let root = nested.layout.as_ref()?.root;
        let global_id = nested.id.clone();
        let context = nested.context.clone();
        let element_id = global_id.0.last().cloned()?;

        let engine = self.layout_engine.as_ref().unwrap();
        let (changes, remeasures, transient) = (
            engine.layout_changes(),
            engine.remeasures(),
            engine.transient_count(),
        );
        let recording = self.record_claimed_layout_keys();
        let element_states_start = self.next_frame.accessed_element_states.len();
        let dependency_recording = cx.begin_recording_dependencies();
        let inherited = self.enter_gap(&global_id, &rebuild, context.content_mask, context.opacity);
        let mut view = ViewElement::new(rebuild.view.clone()).rebuildable(rebuild.view.clone());
        // What its element's request for layout does, inside the view around
        // it.
        let (layout_id, layout, layout_key) =
            self.with_parent_layout_key(rebuild.parent_layout_key, |window| {
                let layout_key = window.push_layout_key(Some(&element_id));
                window.element_id_stack.push(element_id.clone());
                let (layout_id, layout) = view.request_view_layout(Some(&global_id), window, cx);
                window.element_id_stack.pop();
                window.pop_layout_key();
                (layout_id, layout, layout_key)
            });
        self.leave_gap(inherited);
        let dependencies = cx.finish_recording_dependencies(dependency_recording).all;
        let element_states =
            self.next_frame.accessed_element_states[element_states_start..].to_vec();
        let claimed = self.finish_recording_claimed_layout_keys(recording);

        let engine = self.layout_engine.as_ref().unwrap();
        let unchanged = layout_id == root
            && engine.layout_changes() == changes
            && engine.remeasures() == remeasures
            && engine.transient_count() == transient;
        Some((
            Gap {
                record,
                global_id,
                view,
                layout,
                layout_id,
                layout_key,
                claimed,
                element_states,
                dependencies,
                prepainted: None,
            },
            unchanged,
        ))
    }

    /// Keeps views built for a splice that did not happen, for their elements
    /// to take over. See [`Window::take_prebuilt`].
    fn stash_gaps(&mut self, gaps: Vec<Gap>) {
        for gap in gaps {
            self.retained_state.prebuilt.insert(
                gap.global_id,
                PrebuiltGap {
                    prebuilt: Prebuilt {
                        view: gap.view,
                        layout: gap.layout,
                    },
                    layout_id: gap.layout_id,
                    claimed: gap.claimed,
                    element_states: gap.element_states,
                    dependencies: gap.dependencies,
                },
            );
        }
    }

    /// The view built for the element `id` by a splice that did not happen,
    /// if there is one, with the layout it requested. The view being built
    /// around it takes over its nodes, the element states it used and what it
    /// read, as though it had built it itself.
    pub(crate) fn take_prebuilt(
        &mut self,
        id: &GlobalElementId,
        cx: &mut App,
    ) -> Option<(Prebuilt, LayoutId)> {
        let gap = self.retained_state.prebuilt.remove(id)?;
        self.layout_engine
            .as_mut()
            .unwrap()
            .adopt_claimed(&gap.claimed);
        self.next_frame
            .accessed_element_states
            .extend(gap.element_states);
        cx.replay_dependencies(&gap.dependencies);
        Some((gap.prebuilt, gap.layout_id))
    }

    /// Hands the views [`Window::splice_layout`] built over to their elements,
    /// for a view that has to be built after all.
    pub(crate) fn abandon_splice(&mut self, splice: Splice) {
        self.stash_gaps(splice.gaps);
    }

    /// Prepaints the view `id` that [`Window::splice_layout`] laid out: last
    /// frame's prepaint, copied around the gaps, and the gaps prepainted where
    /// they were.
    pub(crate) fn splice_prepaint(
        &mut self,
        id: &GlobalElementId,
        splice: Splice,
        cx: &mut App,
    ) -> ViewPrepaint {
        let Splice { previous, mut gaps } = splice;
        let source = &self.rendered_frame.retained;
        let record = &source.records[previous];
        let prepaint_range = record.prepaint_range.clone();
        let last = previous + record.nested;
        let gap_layout_keys: FxHashSet<u64> = gaps
            .iter()
            .flat_map(|gap| source.records[gap.record].layout_keys.iter().copied())
            .collect();
        let kept_layout_keys: Vec<u64> = record
            .layout_keys
            .iter()
            .copied()
            .filter(|key| !gap_layout_keys.contains(key))
            .collect();
        let mut own = copy_record(record, prepaint_range.clone(), 0);
        // Painted by `splice_paint`, if at all.
        own.paint = PaintStatus::Unpainted;
        self.keep_retained_layout(&kept_layout_keys);
        self.layout_engine
            .as_mut()
            .unwrap()
            .retention
            .stats
            .views_reused += 1;

        let target = &mut self.next_frame.retained;
        target.reused_any = true;
        let index = target.push(own);
        target.open.push(index);
        self.retained_state.subtree_stack.push(id.clone());

        let start = self.prepaint_index();
        let mut dispatch = OpenDispatchCopy::default();
        let mut copied = Vec::new();
        let mut cursor = prepaint_range.start.clone();
        let mut next_record = previous + 1;
        for (segment, gap) in gaps.iter_mut().enumerate() {
            let gap_record = &self.rendered_frame.retained.records[gap.record];
            let gap_range = gap_record.prepaint_range.clone();
            let gap_nested = gap_record.nested;
            let context = gap_record.context.clone();
            let rebuild = gap_record.rebuild.clone().unwrap();
            let gap_id = gap_record.id.clone();

            let segment_start = self.prepaint_index();
            self.copy_prepaint_segment(cursor.clone()..gap_range.start.clone(), &mut dispatch);
            copied.extend(self.copy_records(
                next_record..gap.record,
                &cursor,
                &segment_start,
                segment,
                index,
            ));

            // The gap hangs off the dispatch node it hung off last frame.
            let parent =
                self.rendered_frame.dispatch_tree.nodes[gap_range.start.dispatch_tree_index].parent;
            dispatch.unwind_to(parent, &mut self.next_frame.dispatch_tree);
            let inherited =
                self.enter_gap(&gap_id, &rebuild, context.content_mask, context.opacity);
            // What its element's prepaint does, inside the view around it.
            let bounds = self.layout_bounds(gap.layout_id);
            self.element_id_stack.push(gap.element_id());
            let node = self.next_frame.dispatch_tree.push_node();
            let scope = self.enter_prepaint_layout_scope(gap.layout_key);
            let prepaint =
                gap.view
                    .prepaint_view(Some(&gap.global_id), bounds, &mut gap.layout, self, cx);
            self.exit_prepaint_layout_scope(scope);
            self.next_frame.dispatch_tree.pop_node();
            self.element_id_stack.pop();
            gap.prepainted = Some((node, prepaint));
            self.leave_gap(inherited);

            cursor = gap_range.end.clone();
            next_record = gap.record + gap_nested + 1;
        }
        let segment = gaps.len();
        let segment_start = self.prepaint_index();
        self.copy_prepaint_segment(cursor.clone()..prepaint_range.end, &mut dispatch);
        copied.extend(self.copy_records(
            next_record..last + 1,
            &cursor,
            &segment_start,
            segment,
            index,
        ));
        dispatch.close(&mut self.next_frame.dispatch_tree);
        let end = self.prepaint_index();

        self.retained_state.subtree_stack.pop();
        let target = &mut self.next_frame.retained;
        debug_assert_eq!(target.open.last(), Some(&index));
        target.open.pop();
        let nested = target.records.len() - index - 1;

        // The gaps' layout keys, dependencies and hovers are this frame's.
        let mut layout_keys = kept_layout_keys;
        let mut dependencies = self.rendered_frame.retained.records[previous]
            .dependencies
            .clone();
        for gap in &gaps {
            let gap_id = &self.rendered_frame.retained.records[gap.record].id;
            if let Some(&gap_index) = target.by_id.get(gap_id) {
                let gap_record = &target.records[gap_index];
                layout_keys.extend(gap_record.layout_keys.iter().copied());
                dependencies = dependencies.union(&gap_record.dependencies);
            }
        }
        let record = &mut target.records[index];
        record.prepaint_range = start..end;
        record.nested = nested;
        record.layout_keys = layout_keys.into();
        record.dependencies = dependencies;

        ViewPrepaint::Spliced(SplicedPrepaint {
            previous,
            index,
            gaps,
            copied,
        })
    }

    /// Copies last frame's prepaint of `range` as [`Window::reuse_prepaint`]
    /// does, but leaves the dispatch nodes it enters open in `dispatch`, for
    /// what follows to hang off them.
    fn copy_prepaint_segment(
        &mut self,
        range: Range<PrepaintStateIndex>,
        dispatch: &mut OpenDispatchCopy,
    ) {
        self.next_frame.hitboxes.extend(
            self.rendered_frame.hitboxes[range.start.hitboxes_index..range.end.hitboxes_index]
                .iter()
                .cloned(),
        );
        self.next_frame.tooltip_requests.extend(
            self.rendered_frame.tooltip_requests
                [range.start.tooltips_index..range.end.tooltips_index]
                .iter_mut()
                .map(|request| request.take()),
        );
        self.next_frame.accessed_element_states.extend(
            self.rendered_frame.accessed_element_states[range.start.accessed_element_states_index
                ..range.end.accessed_element_states_index]
                .iter()
                .cloned(),
        );
        self.text_system()
            .reuse_layouts(range.start.line_layout_index..range.end.line_layout_index);

        let contains_focus = dispatch.copy(
            range.start.dispatch_tree_index..range.end.dispatch_tree_index,
            &mut self.rendered_frame.dispatch_tree,
            &mut self.next_frame.dispatch_tree,
            self.focus,
        );
        if contains_focus {
            self.next_frame.focus = self.focus;
        }

        for deferred_draw in &self.rendered_frame.deferred_draws
            [range.start.deferred_draws_index..range.end.deferred_draws_index]
        {
            self.next_frame.deferred_draws.push(DeferredDraw {
                current_view: deferred_draw.current_view,
                parent_node: dispatch.refresh(deferred_draw.parent_node),
                element_id_stack: deferred_draw.element_id_stack.clone(),
                text_style_stack: deferred_draw.text_style_stack.clone(),
                content_mask: deferred_draw.content_mask,
                rem_size: deferred_draw.rem_size,
                priority: deferred_draw.priority,
                element: None,
                absolute_offset: deferred_draw.absolute_offset,
                prepaint_range: deferred_draw.prepaint_range.clone(),
                paint_range: deferred_draw.paint_range.clone(),
                enclosing_retained: EnclosingRetained::default(),
            });
        }
    }

    /// Copies last frame's records `records`, whose prepaint was copied from
    /// `from` on to `to`, into this frame inside the spliced view `anchor`,
    /// returning where they went and `segment`, for their paint to be shifted
    /// by it.
    fn copy_records(
        &mut self,
        records: Range<usize>,
        from: &PrepaintStateIndex,
        to: &PrepaintStateIndex,
        segment: usize,
        anchor: usize,
    ) -> Vec<(usize, usize)> {
        let source = &self.rendered_frame.retained;
        let target = &mut self.next_frame.retained;
        let mut copied = Vec::with_capacity(records.len());
        for index in records {
            let record = &source.records[index];
            let prepaint_range = record.prepaint_range.start.shifted(from, to)
                ..record.prepaint_range.end.shifted(from, to);
            let painted = matches!(record.paint, PaintStatus::Painted { .. });
            let mut copy = copy_record(record, prepaint_range, anchor);
            if !painted {
                copy.paint = PaintStatus::Unpainted;
            }
            copied.push((target.push(copy), segment));
        }
        copied
    }

    /// Paints the view [`Window::splice_prepaint`] prepainted: last frame's
    /// paint, copied around the gaps, and the gaps painted where they were.
    pub(crate) fn splice_paint(
        &mut self,
        id: &GlobalElementId,
        spliced: &mut SplicedPrepaint,
        cx: &mut App,
    ) {
        let SplicedPrepaint {
            previous,
            index,
            gaps,
            copied,
        } = spliced;
        let record = &self.rendered_frame.retained.records[*previous];
        let paint_range = record.paint_range.clone();
        let own_hovers = record.own_hovers.clone();

        self.retained_state.subtree_stack.push(id.clone());
        // The gaps' hovers are nested in this view's.
        self.retained_state
            .open_paints
            .push(OpenPaint { nested: Vec::new() });
        let hovers_start = self.retained_state.hover_dependencies.len();
        let start = self.paint_index();
        let mut segments: Vec<(PaintIndex, PaintIndex)> = Vec::with_capacity(gaps.len() + 1);
        let mut cursor = paint_range.start.clone();
        for gap in gaps.iter_mut() {
            let gap_record = &self.rendered_frame.retained.records[gap.record];
            let gap_range = gap_record.paint_range.clone();
            let context = gap_record.context.clone();
            let rebuild = gap_record.rebuild.clone().unwrap();
            let gap_id = gap_record.id.clone();

            segments.push((cursor.clone(), self.paint_index()));
            self.reuse_paint(cursor.clone()..gap_range.start.clone());
            let inherited =
                self.enter_gap(&gap_id, &rebuild, context.content_mask, context.opacity);
            // What its element's paint does, inside the view around it.
            let element_id = gap.element_id();
            if let Some((node, prepaint)) = gap.prepainted.as_mut() {
                self.element_id_stack.push(element_id);
                self.next_frame.dispatch_tree.set_active_node(*node);
                gap.view
                    .paint_view(Some(&gap.global_id), prepaint, self, cx);
                self.element_id_stack.pop();
            }
            self.leave_gap(inherited);
            cursor = gap_range.end.clone();
        }
        segments.push((cursor.clone(), self.paint_index()));
        self.reuse_paint(cursor..paint_range.end);
        let end = self.paint_index();

        // The records copied along with it land where their stretch did.
        let mut copied_hovers: Vec<(HitboxId, bool)> = Vec::new();
        for &(copy, segment) in copied.iter() {
            let (from, to) = &segments[segment];
            let record = &mut self.next_frame.retained.records[copy];
            if let PaintStatus::Pending { .. } = record.paint {
                record.paint_range = record.paint_range.start.shifted(from, to)
                    ..record.paint_range.end.shifted(from, to);
                record.paint = PaintStatus::Painted { source: None };
                copied_hovers.extend_from_slice(&record.hover_dependencies);
            }
        }

        // Its hovers: its own, the copied records', and the gaps' new ones,
        // which painting them added.
        let gap_hovers = self.retained_state.hover_dependencies[hovers_start..].to_vec();
        let mut hovers = own_hovers.to_vec();
        hovers.extend_from_slice(&copied_hovers);
        hovers.extend_from_slice(&gap_hovers);
        let record = &mut self.next_frame.retained.records[*index];
        record.paint_range = start..end;
        record.paint = PaintStatus::Painted { source: None };
        record.hover_dependencies = hovers.into();
        self.retained_state.open_paints.pop();
        self.retained_state.subtree_stack.pop();

        // As for a view drawn from last frame, a hover found changed only now
        // builds it on the next frame.
        let mut copied_checked = own_hovers.to_vec();
        copied_checked.extend_from_slice(&copied_hovers);
        if !self.hovers_unchanged(&copied_checked) {
            self.retained_state
                .subtrees_dirty_next_frame
                .extend(self.retained_state.subtree_stack.iter().cloned());
            self.retained_state
                .subtrees_dirty_next_frame
                .insert(id.clone());
            self.request_animation_frame();
        }
        // The views around it depend on its hovers too, but not as their own.
        self.retained_state
            .hover_dependencies
            .truncate(hovers_start);
        let all = self.next_frame.retained.records[*index]
            .hover_dependencies
            .clone();
        self.add_nested_hovers(&all);
    }
}

/// A copy of `record`, with its prepaint at `prepaint_range` in this frame and
/// its paint to be placed once `anchor`, the view it was copied along with,
/// is painted. A spliced view places it itself; if it is never painted, the
/// copy is forgotten at the end of the frame.
fn copy_record(
    record: &RetainedSubtree,
    prepaint_range: Range<PrepaintStateIndex>,
    anchor: usize,
) -> RetainedSubtree {
    RetainedSubtree {
        id: record.id.clone(),
        prepaint_range,
        paint_range: record.paint_range.clone(),
        paint: PaintStatus::Pending { anchor },
        nested: record.nested,
        context: record.context.clone(),
        dependencies: record.dependencies.clone(),
        own_dependencies: record.own_dependencies.clone(),
        hover_dependencies: record.hover_dependencies.clone(),
        own_hovers: record.own_hovers.clone(),
        layout_keys: record.layout_keys.clone(),
        layout: record.layout.clone(),
        rebuild: record.rebuild.clone(),
    }
}

/// The entity of the view whose element has the id `id`.
fn view_entity(id: &GlobalElementId) -> Option<EntityId> {
    match id.0.last()? {
        ElementId::View(entity) => Some(*entity),
        _ => None,
    }
}

/// Last frame's dispatch nodes copied stretch by stretch, with the nodes the
/// copy is inside of left open between stretches.
#[derive(Default)]
struct OpenDispatchCopy {
    /// Last frame's nodes the copy is inside of, innermost last.
    open: Vec<DispatchNodeId>,
    /// Each stretch copied, and where it landed.
    stretches: Vec<(Range<usize>, usize)>,
}

impl OpenDispatchCopy {
    /// Copies the nodes `range` of `source` into `target`, returning whether
    /// one of them holds `focus`.
    fn copy(
        &mut self,
        range: Range<usize>,
        source: &mut DispatchTree,
        target: &mut DispatchTree,
        focus: Option<FocusId>,
    ) -> bool {
        self.stretches.push((range.clone(), target.len()));
        let mut contains_focus = false;
        for index in range {
            let node = &mut source.nodes[index];
            while let Some(&open) = self.open.last() {
                if node.parent == Some(open) {
                    break;
                }
                self.open.pop();
                target.pop_node();
            }
            self.open.push(DispatchNodeId(index));
            if node.focus_id.is_some() && node.focus_id == focus {
                contains_focus = true;
            }
            target.move_node(node);
        }
        contains_focus
    }

    /// Closes open nodes until the innermost is `parent`, or none is.
    fn unwind_to(&mut self, parent: Option<DispatchNodeId>, target: &mut DispatchTree) {
        while let Some(&open) = self.open.last() {
            if Some(open) == parent {
                break;
            }
            self.open.pop();
            target.pop_node();
        }
    }

    fn close(&mut self, target: &mut DispatchTree) {
        self.unwind_to(None, target);
    }

    /// Where last frame's node `node`, which was copied, is now.
    fn refresh(&self, node: DispatchNodeId) -> DispatchNodeId {
        let (range, start) = self
            .stretches
            .iter()
            .find(|(range, _)| range.contains(&node.0))
            .expect("a copied deferred draw hangs off a copied node");
        DispatchNodeId(node.0 - range.start + start)
    }
}
