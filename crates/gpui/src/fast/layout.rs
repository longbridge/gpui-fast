//! Taffy layout nodes retained across frames, and the style fingerprint that decides whether a retained node can be left alone.

use crate::{
    AbsoluteLength, App, AvailableSpace, DefiniteLength, Edges, GridTemplate, LayoutId, Length,
    Pixels, Size, Style, TaffyLayoutEngine, Window,
    fast::stats::LayoutStats,
    taffy::{EXPECT_MESSAGE, MeasureFn, NodeContext, ToTaffy as _},
    util::round_to_device_pixel,
};
use collections::{FxHashMap, FxHasher};
use smallvec::SmallVec;
use std::{
    any::Any,
    fmt::Debug,
    hash::{Hash as _, Hasher as _},
    mem,
    rc::Rc,
};
use taffy::TaffyTree;

/// The retained half of a [`TaffyLayoutEngine`]: the nodes it keeps from one
/// frame to the next, what it needs to tell whether a frame can leave them
/// alone, and the counters describing the work it did.
#[derive(Default)]
pub(crate) struct LayoutRetention {
    /// Nodes surviving from earlier frames, keyed by their position in the
    /// element tree. `Window::push_layout_key` derives the keys.
    retained: FxHashMap<u64, RetainedNode>,
    /// Nodes allocated this frame that are not retained, because the caller had
    /// no key for them or because their key was already claimed. Released at
    /// the end of the frame, which is what used to happen to every node.
    transient: Vec<LayoutId>,
    /// Styles as the element requested them, for the few nodes whose style
    /// Taffy no longer holds verbatim because [`TaffyLayoutEngine::stretch_auto_size_to_fill`]
    /// rewrote it. Without this, every frame would compare an unstretched
    /// request against a stretched style, find a difference, and dirty the
    /// window root.
    unstretched_styles: FxHashMap<LayoutId, taffy::style::Style>,
    /// Incremented once per frame; stamped onto nodes as they are claimed.
    frame: u64,
    /// How many retained entries have been claimed so far this frame. When it
    /// matches the size of `retained` at the end of the frame, nothing has been
    /// orphaned and the sweep can be skipped entirely.
    claimed_this_frame: usize,
    /// The keys claimed while [`TaffyLayoutEngine::record_claimed_keys`] is
    /// recording, for a retained subtree to keep its nodes by. See
    /// [`crate::fast::retained`].
    claimed_key_log: Vec<u64>,
    /// How many recordings are open, nested retained subtrees each having one.
    open_key_recordings: usize,
    pub(crate) stats: LayoutStats,
    /// Whether to time layout and measurements. See [`LayoutStats`].
    pub(crate) timed: bool,
    /// Counts every write that changes what a layout computes: a node created,
    /// a style or a child list rewritten, a measurement that no longer
    /// stands. A subtree laid out again without any of these has the layout
    /// it had. See [`TaffyLayoutEngine::layout_changes`].
    layout_changes: u64,
    /// Counts the measured nodes given a new measurement to take. Their
    /// measurement has to be taken again, since what it produces lives in
    /// state the element made afresh, though it is expected to come out as
    /// before. See [`TaffyLayoutEngine::remeasures`].
    remeasures: u64,
}

/// Removes a node from the tree, and with it what its measurement captured.
///
/// Taffy keeps a removed node's context until another node is given its slot,
/// so a measurement closure, and everything it holds — the text of a text
/// element and the lines shaped from it — would outlive its node by however
/// many frames that takes. Replacing the closure rather than clearing the
/// context releases all of it without dirtying the node, or a parent it is
/// still attached to, on the way out.
fn remove_node(taffy: &mut TaffyTree<NodeContext>, id: LayoutId) {
    if let Some(context) = taffy.get_node_context_mut(id.into()) {
        let released: Box<MeasureFn> = Box::new(|_, _, _, _| Size::default());
        #[cfg(feature = "stacker")]
        let released = crate::taffy::StackSafe::new(released);
        context.measure = released;
    }
    taffy.remove(id.into()).expect(EXPECT_MESSAGE);
}

/// A node kept from one frame to the next, alongside enough of the request
/// that produced it to tell whether this frame can leave it alone.
///
/// Taffy caches layout results per node and discards that cache for a node and
/// all of its ancestors whenever the node is dirtied. Every mutating call —
/// `set_style`, `set_children`, `set_node_context` — dirties unconditionally,
/// so keeping nodes across frames is worth nothing on its own: the value comes
/// from *not writing* to them, which is only possible by remembering what the
/// previous frame asked for and comparing against it first.
struct RetainedNode {
    id: LayoutId,
    /// Frame in which this node was last claimed. A node that goes a whole
    /// frame unclaimed has left the element tree and is released.
    claimed_in_frame: u64,
    /// The children the node was last given, kept here because reading them
    /// back out of Taffy allocates.
    children: SmallVec<[LayoutId; 8]>,
    /// Whether the node measures its own size.
    measured: bool,
    /// [`layout_fingerprint`] of the style the node was last asked for. While
    /// the request is the same, converting it to a Taffy style and comparing
    /// that against the node's is work with only one possible outcome.
    style_fingerprint: u64,
    /// What the element measuring this node left for the next frame's element
    /// to take its measurement over from. See
    /// [`TaffyLayoutEngine::request_retained_carried_measured_layout`].
    measurement: Option<Rc<dyn Any>>,
}

/// What [`TaffyLayoutEngine::claim`] found for an element's key.
enum Claim {
    /// A node from an earlier frame, now claimed for this one.
    Reused(u64, LayoutId),
    /// No node yet; one should be allocated and retained under this key.
    Vacant(u64),
    /// No node may be retained for this element: either it has no key, or
    /// another element already claimed the one it has.
    Unkeyed,
}

impl TaffyLayoutEngine {
    /// How many nodes the tree is currently holding, retained and transient
    /// alike. Used by tests to check that retention does not leak.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn node_count(&self) -> usize {
        self.taffy.total_node_count()
    }

    /// Ends the frame for the retained nodes: releases nodes that no longer
    /// appear in the element tree. See [`TaffyLayoutEngine::end_frame`].
    ///
    /// Nodes that were claimed this frame stay, along with their Taffy layout
    /// caches, which is what lets the next frame skip recomputing the parts of
    /// the tree that did not change.
    pub(crate) fn release_unclaimed_nodes(&mut self) {
        let retention = &mut self.retention;
        retention.stats.frames += 1;

        for id in retention.transient.drain(..) {
            retention.unstretched_styles.remove(&id);
            remove_node(&mut self.taffy, id);
            retention.stats.nodes_freed += 1;
        }

        // In a steady frame every retained node was claimed, and there is
        // nothing to sweep.
        if retention.retained.len() != retention.claimed_this_frame {
            let frame = retention.frame;
            let taffy = &mut self.taffy;
            let unstretched_styles = &mut retention.unstretched_styles;
            let freed = &mut retention.stats.nodes_freed;
            retention.retained.retain(|_, node| {
                if node.claimed_in_frame == frame {
                    return true;
                }
                unstretched_styles.remove(&node.id);
                remove_node(taffy, node.id);
                *freed += 1;
                false
            });
        }

        retention.claimed_this_frame = 0;
        retention.frame += 1;
    }

    /// Takes the node retained under `key` for use in this frame.
    fn claim(&mut self, key: Option<u64>) -> Claim {
        let Some(key) = key else {
            return Claim::Unkeyed;
        };
        let retention = &mut self.retention;
        let frame = retention.frame;
        let Some(node) = retention.retained.get_mut(&key) else {
            return Claim::Vacant(key);
        };
        if node.claimed_in_frame == frame {
            // Two elements resolved to one key. Letting both use the node would
            // corrupt the tree, and retaining the second under the same key
            // would strand the first, so the second goes unkeyed.
            return Claim::Unkeyed;
        }
        node.claimed_in_frame = frame;
        retention.claimed_this_frame += 1;
        retention.stats.nodes_reused += 1;
        if retention.open_key_recordings > 0 {
            retention.claimed_key_log.push(key);
        }
        Claim::Reused(key, node.id)
    }

    /// Starts recording the keys of the nodes claimed or allocated from now
    /// on, returning where the recording starts. Recordings nest.
    pub(crate) fn record_claimed_keys(&mut self) -> usize {
        self.retention.open_key_recordings += 1;
        self.retention.claimed_key_log.len()
    }

    /// Ends the recording started at `start`, returning the keys it saw.
    pub(crate) fn finish_recording_claimed_keys(&mut self, start: usize) -> Vec<u64> {
        let retention = &mut self.retention;
        let keys = retention.claimed_key_log[start..].to_vec();
        retention.open_key_recordings -= 1;
        if retention.open_key_recordings == 0 {
            retention.claimed_key_log.clear();
        }
        keys
    }

    /// Keeps the nodes retained under `keys` for another frame without
    /// requesting them, for a subtree drawn from what it drew last frame
    /// rather than laid out again: they are there to be reused when it is
    /// built next. Keys already claimed this frame, or no longer retained,
    /// are passed over.
    pub(crate) fn keep_retained(&mut self, keys: &[u64]) {
        let retention = &mut self.retention;
        let frame = retention.frame;
        for key in keys {
            if let Some(node) = retention.retained.get_mut(key)
                && node.claimed_in_frame != frame
            {
                node.claimed_in_frame = frame;
                retention.claimed_this_frame += 1;
                if retention.open_key_recordings > 0 {
                    retention.claimed_key_log.push(*key);
                }
            }
        }
    }

    /// Takes over nodes claimed this frame by a layout request made before
    /// the one being recorded, as if it had claimed them itself: they are
    /// kept, and recorded as its own.
    pub(crate) fn adopt_claimed(&mut self, keys: &[u64]) {
        let retention = &mut self.retention;
        let frame = retention.frame;
        for key in keys {
            let Some(node) = retention.retained.get_mut(key) else {
                continue;
            };
            if node.claimed_in_frame != frame {
                node.claimed_in_frame = frame;
                retention.claimed_this_frame += 1;
            }
            if retention.open_key_recordings > 0 {
                retention.claimed_key_log.push(*key);
            }
        }
    }

    /// Keeps the nodes retained under `keys` for another frame, as
    /// [`Self::keep_retained`] does, but only if every one of them is still
    /// retained and unclaimed this frame; otherwise keeps none of them and
    /// returns false. A subtree whose layout is reused without being requested
    /// again needs all of its nodes, just as they were.
    pub(crate) fn try_keep_retained(&mut self, keys: &[u64]) -> bool {
        let frame = self.retention.frame;
        let all_there = keys.iter().all(|key| {
            self.retention
                .retained
                .get(key)
                .is_some_and(|node| node.claimed_in_frame != frame)
        });
        if all_there {
            self.keep_retained(keys);
            self.retention.stats.nodes_reused += keys.len() as u64;
        }
        all_there
    }

    /// Undoes [`Self::try_keep_retained`] for `keys`, so that the subtree can
    /// be laid out again after all this frame and claim its nodes itself.
    pub(crate) fn release_kept(&mut self, keys: &[u64]) {
        let retention = &mut self.retention;
        let frame = retention.frame;
        for key in keys {
            if let Some(node) = retention.retained.get_mut(key)
                && node.claimed_in_frame == frame
            {
                node.claimed_in_frame = frame.wrapping_sub(1);
                retention.claimed_this_frame -= 1;
                retention.stats.nodes_reused = retention.stats.nodes_reused.saturating_sub(1);
            }
        }
    }

    /// How many nodes allocated this frame are not retained. A subtree that
    /// allocated any cannot have its layout reused, since they go at the end
    /// of the frame.
    pub(crate) fn transient_count(&self) -> usize {
        self.retention.transient.len()
    }

    /// See [`LayoutRetention::layout_changes`] on the field.
    pub(crate) fn layout_changes(&self) -> u64 {
        self.retention.layout_changes
    }

    /// See [`LayoutRetention::remeasures`] on the field.
    pub(crate) fn remeasures(&self) -> u64 {
        self.retention.remeasures
    }

    /// Records a freshly allocated node under `key`, or as transient when there
    /// is no key to record it under.
    fn retain(
        &mut self,
        key: Option<u64>,
        id: LayoutId,
        children: &[LayoutId],
        measured: bool,
        style_fingerprint: u64,
    ) {
        let retention = &mut self.retention;
        let Some(key) = key else {
            retention.transient.push(id);
            return;
        };
        retention.retained.insert(
            key,
            RetainedNode {
                id,
                claimed_in_frame: retention.frame,
                children: SmallVec::from_slice(children),
                measured,
                style_fingerprint,
                measurement: None,
            },
        );
        retention.claimed_this_frame += 1;
        if retention.open_key_recordings > 0 {
            retention.claimed_key_log.push(key);
        }
    }

    /// Brings a retained node's style up to date with `style`, converting and
    /// comparing it only when it is not the request the node was last given.
    fn apply_requested_style(
        &mut self,
        key: u64,
        id: LayoutId,
        style: &Style,
        rem_size: Pixels,
        scale_factor: f32,
    ) {
        let fingerprint = layout_fingerprint(style, rem_size, scale_factor);
        let node = self
            .retention
            .retained
            .get_mut(&key)
            .expect("a claimed key is always present");
        if node.style_fingerprint == fingerprint {
            self.retention.stats.style_compares += 1;
            // A field `layout_fingerprint` fails to read would leave the node
            // with a stale style whenever only that field changed, and nothing
            // would say so; debug builds compare in full to catch one.
            debug_assert!(
                self.retention
                    .unstretched_styles
                    .get(&id)
                    .unwrap_or_else(|| self.taffy.style(id.into()).expect(EXPECT_MESSAGE))
                    == &style.to_taffy(rem_size, scale_factor),
                "layout_fingerprint matched a style that converts differently; \
                 it has to read every field to_taffy does"
            );
            return;
        }
        node.style_fingerprint = fingerprint;
        self.apply_style(id, style.to_taffy(rem_size, scale_factor));
    }

    /// Writes `style` to a node, but only if it differs from what the node was
    /// last asked for.
    fn apply_style(&mut self, id: LayoutId, style: taffy::style::Style) {
        let retention = &mut self.retention;
        retention.stats.style_compares += 1;
        let previous = retention
            .unstretched_styles
            .get(&id)
            .unwrap_or_else(|| self.taffy.style(id.into()).expect(EXPECT_MESSAGE));
        if previous == &style {
            return;
        }
        // Taffy now holds exactly what was requested, so the stretched-style
        // bookkeeping no longer applies.
        retention.unstretched_styles.remove(&id);
        retention.stats.style_writes += 1;
        retention.layout_changes += 1;
        self.taffy
            .set_style(id.into(), style)
            .expect(EXPECT_MESSAGE);
    }

    /// Writes `children` to a retained node, but only if the list changed.
    fn apply_children(&mut self, key: u64, id: LayoutId, children: &[LayoutId]) {
        let retention = &mut self.retention;
        let node = retention
            .retained
            .get_mut(&key)
            .expect("a claimed key is always present");
        if node.children.as_slice() == children {
            return;
        }
        node.children.clear();
        node.children.extend_from_slice(children);
        retention.stats.children_writes += 1;
        retention.layout_changes += 1;
        self.taffy
            // This is safe because LayoutId is repr(transparent) to taffy::tree::NodeId.
            .set_children(id.into(), LayoutId::to_taffy_slice(children))
            .expect(EXPECT_MESSAGE);
    }

    /// Adds a node to the layout tree, reusing the one retained under `key`
    /// when there is one. See [`TaffyLayoutEngine::request_layout`].
    ///
    /// `key` identifies this element's position in the element tree across
    /// frames; `None` opts out of reuse, and the node is released at the end of
    /// the frame.
    pub(crate) fn request_retained_layout(
        &mut self,
        key: Option<u64>,
        style: Style,
        rem_size: Pixels,
        scale_factor: f32,
        children: &[LayoutId],
    ) -> LayoutId {
        let key = match self.claim(key) {
            Claim::Reused(key, id) => {
                self.apply_requested_style(key, id, &style, rem_size, scale_factor);
                self.apply_children(key, id, children);
                // A node that measured itself on an earlier frame no longer does.
                if self
                    .retention
                    .retained
                    .get(&key)
                    .is_some_and(|node| node.measured)
                {
                    let node = self
                        .retention
                        .retained
                        .get_mut(&key)
                        .expect("a claimed key is always present");
                    node.measured = false;
                    node.measurement = None;
                    self.taffy
                        .set_node_context(id.into(), None)
                        .expect(EXPECT_MESSAGE);
                }
                return id;
            }
            Claim::Vacant(key) => Some(key),
            Claim::Unkeyed => None,
        };

        self.retention.stats.nodes_created += 1;
        self.retention.layout_changes += 1;
        let style_fingerprint = layout_fingerprint(&style, rem_size, scale_factor);
        let taffy_style = style.to_taffy(rem_size, scale_factor);
        let id: LayoutId = self
            .taffy
            .new_leaf(taffy_style)
            .expect(EXPECT_MESSAGE)
            .into();
        if !children.is_empty() {
            // A retained child can arrive here still listed under the parent
            // it had last frame. `new_with_children` would leave it listed
            // there, so when that parent's children were next rewritten the
            // child would lose its parent link, and with it the offset
            // `layout_bounds` adds up from its ancestors. `set_children`
            // detaches each child from wherever it was first.
            self.taffy
                // This is safe because LayoutId is repr(transparent) to taffy::tree::NodeId.
                .set_children(id.into(), LayoutId::to_taffy_slice(children))
                .expect(EXPECT_MESSAGE);
        }
        self.retain(key, id, children, false, style_fingerprint);
        id
    }

    /// Adds a self-measuring leaf to the layout tree, reusing the node retained
    /// under `key` when there is one. See
    /// [`TaffyLayoutEngine::request_measured_layout`].
    ///
    /// Nothing says what the measurement depends on, so a reused node is given
    /// the new closure and dirtied, and `measure` is guaranteed to run.
    pub(crate) fn request_retained_measured_layout(
        &mut self,
        key: Option<u64>,
        style: Style,
        rem_size: Pixels,
        scale_factor: f32,
        measure: impl FnMut(
            Size<Option<Pixels>>,
            Size<AvailableSpace>,
            &mut Window,
            &mut App,
        ) -> Size<Pixels>
        + 'static,
    ) -> LayoutId {
        let measure = Box::new(measure) as Box<MeasureFn>;
        #[cfg(feature = "stacker")]
        let measure = crate::taffy::StackSafe::new(measure);
        self.retention.stats.measure_rebinds += 1;

        let (key, id) = match self.claim(key) {
            Claim::Reused(key, id) => (key, id),
            claim => {
                let key = match claim {
                    Claim::Vacant(key) => Some(key),
                    _ => None,
                };
                let style_fingerprint = layout_fingerprint(&style, rem_size, scale_factor);
                let taffy_style = style.to_taffy(rem_size, scale_factor);
                self.retention.stats.nodes_created += 1;
                self.retention.layout_changes += 1;
                let id: LayoutId = self
                    .taffy
                    .new_leaf_with_context(taffy_style, NodeContext { measure })
                    .expect(EXPECT_MESSAGE)
                    .into();
                self.retain(key, id, &[], true, style_fingerprint);
                return id;
            }
        };

        self.apply_requested_style(key, id, &style, rem_size, scale_factor);
        self.apply_children(key, id, &[]);

        // Nothing says whether the measurement still stands, and what it
        // produces lives in state the element made afresh this frame, so it
        // has to be taken again.
        self.retention.remeasures += 1;
        if let Some(context) = self.taffy.get_node_context_mut(id.into()) {
            context.measure = measure;
        } else {
            self.taffy
                .set_node_context(id.into(), Some(NodeContext { measure }))
                .expect(EXPECT_MESSAGE);
        }
        self.taffy.mark_dirty(id.into()).expect(EXPECT_MESSAGE);
        let node = self
            .retention
            .retained
            .get_mut(&key)
            .expect("a claimed key is always present");
        node.measured = true;
        node.measurement = None;

        id
    }

    /// Adds a self-measuring leaf to the layout tree as
    /// [`Self::request_retained_measured_layout`] does, but lets the element
    /// take over the measurement of the node retained under `key`, rather
    /// than have it taken again.
    ///
    /// The element measuring the node last frame left `memo` there; `adopt` is
    /// given it and takes the measurement over if it still stands, in which
    /// case the node is given the new closure without being dirtied, and keeps
    /// what Taffy cached for it and the nodes above it. Either way this
    /// element's `memo` is left for the next frame's.
    pub(crate) fn request_retained_carried_measured_layout(
        &mut self,
        key: Option<u64>,
        style: Style,
        rem_size: Pixels,
        scale_factor: f32,
        memo: Rc<dyn Any>,
        adopt: impl FnOnce(&dyn Any) -> bool,
        measure: impl FnMut(
            Size<Option<Pixels>>,
            Size<AvailableSpace>,
            &mut Window,
            &mut App,
        ) -> Size<Pixels>
        + 'static,
    ) -> LayoutId {
        let frame = self.retention.frame;
        let previous = key
            .and_then(|key| self.retention.retained.get(&key))
            .filter(|node| {
                node.claimed_in_frame != frame
                    && node.measured
                    && node.style_fingerprint == layout_fingerprint(&style, rem_size, scale_factor)
            })
            .and_then(|node| node.measurement.clone());
        if let Some(previous) = previous
            && adopt(&*previous)
            && let Claim::Reused(key, id) = self.claim(key)
            && let Some(context) = self.taffy.get_node_context_mut(id.into())
        {
            let measure = Box::new(measure) as Box<MeasureFn>;
            #[cfg(feature = "stacker")]
            let measure = crate::taffy::StackSafe::new(measure);
            context.measure = measure;
            self.retention
                .retained
                .get_mut(&key)
                .expect("a claimed key is always present")
                .measurement = Some(memo);
            self.retention.stats.measurements_kept += 1;
            return id;
        }

        let id = self.request_retained_measured_layout(key, style, rem_size, scale_factor, measure);
        if let Some(node) = key.and_then(|key| self.retention.retained.get_mut(&key))
            && node.id == id
        {
            node.measurement = Some(memo);
        }
        id
    }

    /// Treats any `auto` dimension of the given node's style as filling `size`.
    /// See [`TaffyLayoutEngine::stretch_auto_size_to_fill`].
    ///
    /// The style Taffy ends up holding is not the one the element asked for, and
    /// the difference is not recoverable from the result — a stretched `auto`
    /// looks exactly like an explicit length. The requested style is therefore
    /// kept aside so the next frame compares like with like instead of
    /// rewriting, and dirtying, the root on every frame.
    pub(crate) fn stretch_retained_auto_size_to_fill(
        &mut self,
        id: LayoutId,
        size: Size<Pixels>,
        scale_factor: f32,
    ) {
        let retention = &mut self.retention;
        let requested = match retention.unstretched_styles.get(&id) {
            Some(requested) => requested,
            None => self.taffy.style(id.into()).expect(EXPECT_MESSAGE),
        };
        let stretch_width = requested.size.width.is_auto();
        let stretch_height = requested.size.height.is_auto();
        if !stretch_width && !stretch_height {
            return;
        }

        let requested = requested.clone();
        let mut style = requested.clone();
        if stretch_width {
            style.size.width =
                taffy::style::Dimension::length(round_to_device_pixel(size.width.0, scale_factor));
        }
        if stretch_height {
            style.size.height =
                taffy::style::Dimension::length(round_to_device_pixel(size.height.0, scale_factor));
        }
        if self.taffy.style(id.into()).expect(EXPECT_MESSAGE) != &style {
            retention.stats.style_writes += 1;
            self.taffy
                .set_style(id.into(), style)
                .expect(EXPECT_MESSAGE);
        }
        retention.unstretched_styles.insert(id, requested);
    }

    /// Lays out again the subtree under `id`, a node already placed by its
    /// parent this frame, within `available_space`, leaving it where its
    /// parent put it.
    ///
    /// Computing a layout from a node treats it as a root and moves it to the
    /// origin; its absolute position, worked out before, is put back so that
    /// the bounds of everything under it are found relative to it as before.
    ///
    /// A root is sized by its own style, so a node its parent stretched would
    /// shrink to its content. It is held at the size its parent gave it while
    /// it is laid out, and given back the style it asked for afterwards.
    pub(crate) fn relayout_in_place(
        &mut self,
        id: LayoutId,
        available_space: Size<AvailableSpace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let scale_factor = window.scale_factor();
        let bounds = self.layout_bounds(id, scale_factor);
        let origin = self
            .absolute_outer_origins
            .get(&id)
            .copied()
            .expect("layout_bounds caches the absolute origin");
        let requested = self.taffy.style(id.into()).expect(EXPECT_MESSAGE).clone();
        let given = self.taffy.layout(id.into()).expect(EXPECT_MESSAGE).size;
        let mut held = requested.clone();
        held.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(given.width),
            height: taffy::style::Dimension::length(given.height),
        };
        held.min_size = held.size;
        held.max_size = held.size;
        held.box_sizing = taffy::style::BoxSizing::BorderBox;
        self.taffy.set_style(id.into(), held).expect(EXPECT_MESSAGE);
        self.compute_layout(id, available_space, window, cx);
        self.taffy
            .set_style(id.into(), requested)
            .expect(EXPECT_MESSAGE);
        let stack = &mut self.layout_bounds_scratch_space;
        stack.push(id);
        while let Some(id) = stack.pop() {
            self.absolute_layout_bounds.remove(&id);
            self.absolute_outer_origins.remove(&id);
            stack.extend(
                self.taffy
                    .children(id.into())
                    .expect(EXPECT_MESSAGE)
                    .into_iter()
                    .map(LayoutId::from),
            );
        }
        self.absolute_outer_origins.insert(id, origin);
        self.absolute_layout_bounds.insert(id, bounds);
    }
}

/// A hash of everything in `style` that its conversion to a Taffy style reads,
/// and of what that conversion resolves lengths against.
///
/// It has to read exactly the fields [`ToTaffy`](crate::taffy::ToTaffy) does:
/// a field it misses leaves a retained node with a stale style whenever only
/// that field changes. Debug builds check every match against a full conversion.
fn layout_fingerprint(style: &Style, rem_size: Pixels, scale_factor: f32) -> u64 {
    fn absolute(hasher: &mut FxHasher, length: &AbsoluteLength) {
        match length {
            AbsoluteLength::Pixels(pixels) => (0u8, pixels.0.to_bits()).hash(hasher),
            AbsoluteLength::Rems(rems) => (1u8, rems.0.to_bits()).hash(hasher),
        }
    }
    fn definite(hasher: &mut FxHasher, length: &DefiniteLength) {
        match length {
            DefiniteLength::Absolute(length) => {
                0u8.hash(hasher);
                absolute(hasher, length);
            }
            DefiniteLength::Fraction(fraction) => (1u8, fraction.to_bits()).hash(hasher),
        }
    }
    fn length(hasher: &mut FxHasher, length: &Length) {
        match length {
            Length::Definite(length) => {
                0u8.hash(hasher);
                definite(hasher, length);
            }
            Length::Auto => 1u8.hash(hasher),
        }
    }
    fn edges<T: Clone + Debug + Default + PartialEq>(
        hasher: &mut FxHasher,
        edges: &Edges<T>,
        each: fn(&mut FxHasher, &T),
    ) {
        each(hasher, &edges.top);
        each(hasher, &edges.right);
        each(hasher, &edges.bottom);
        each(hasher, &edges.left);
    }
    fn sizes<T: Clone + Debug + Default + PartialEq>(
        hasher: &mut FxHasher,
        size: &Size<T>,
        each: fn(&mut FxHasher, &T),
    ) {
        each(hasher, &size.width);
        each(hasher, &size.height);
    }
    fn placement(hasher: &mut FxHasher, placement: &crate::GridPlacement) {
        match placement {
            crate::GridPlacement::Line(line) => (0u8, *line).hash(hasher),
            crate::GridPlacement::Span(span) => (1u8, *span).hash(hasher),
            crate::GridPlacement::Auto => 2u8.hash(hasher),
        }
    }
    fn template(hasher: &mut FxHasher, template: &Option<GridTemplate>) {
        match template {
            Some(template) => {
                (1u8, template.repeat).hash(hasher);
                mem::discriminant(&template.min_size).hash(hasher);
            }
            None => 0u8.hash(hasher),
        }
    }

    let mut hasher = FxHasher::default();
    let hasher = &mut hasher;
    rem_size.0.to_bits().hash(hasher);
    scale_factor.to_bits().hash(hasher);

    mem::discriminant(&style.display).hash(hasher);
    mem::discriminant(&style.overflow.x).hash(hasher);
    mem::discriminant(&style.overflow.y).hash(hasher);
    absolute(hasher, &style.scrollbar_width);
    mem::discriminant(&style.position).hash(hasher);
    edges(hasher, &style.inset, length);
    sizes(hasher, &style.size, length);
    sizes(hasher, &style.min_size, length);
    sizes(hasher, &style.max_size, length);
    style.aspect_ratio.map(f32::to_bits).hash(hasher);
    edges(hasher, &style.margin, length);
    edges(hasher, &style.padding, definite);
    edges(hasher, &style.border_widths, absolute);
    style
        .align_items
        .map(|x| mem::discriminant(&x))
        .hash(hasher);
    style.align_self.map(|x| mem::discriminant(&x)).hash(hasher);
    style
        .align_content
        .map(|x| mem::discriminant(&x))
        .hash(hasher);
    style
        .justify_content
        .map(|x| mem::discriminant(&x))
        .hash(hasher);
    sizes(hasher, &style.gap, definite);
    mem::discriminant(&style.flex_direction).hash(hasher);
    mem::discriminant(&style.flex_wrap).hash(hasher);
    length(hasher, &style.flex_basis);
    style.flex_grow.to_bits().hash(hasher);
    style.flex_shrink.to_bits().hash(hasher);
    template(hasher, &style.grid_rows);
    template(hasher, &style.grid_cols);
    match &style.grid_location {
        Some(location) => {
            1u8.hash(hasher);
            for line in [&location.row, &location.column] {
                placement(hasher, &line.start);
                placement(hasher, &line.end);
            }
        }
        None => 0u8.hash(hasher),
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::px;

    /// Every field the conversion to a Taffy style reads has to reach the
    /// fingerprint too: a change the fingerprint cannot see is one a retained
    /// node never receives. Each case changes one field that the conversion
    /// reads, and both have to notice.
    #[test]
    fn the_layout_fingerprint_sees_every_field_the_taffy_style_is_made_from() {
        use crate::{
            AlignContent, AlignItems, Display, FlexDirection, FlexWrap, GridLocation,
            GridPlacement, GridTemplateMinSize, Overflow, Position, relative, rems,
        };

        let (rem_size, scale_factor) = (px(16.), 2.);
        let base = Style::default();
        let cases: Vec<(&str, Box<dyn Fn(&mut Style)>)> = vec![
            ("display", Box::new(|s| s.display = Display::Grid)),
            ("overflow.x", Box::new(|s| s.overflow.x = Overflow::Hidden)),
            ("overflow.y", Box::new(|s| s.overflow.y = Overflow::Scroll)),
            (
                "scrollbar_width",
                Box::new(|s| s.scrollbar_width = px(7.).into()),
            ),
            ("position", Box::new(|s| s.position = Position::Absolute)),
            ("inset", Box::new(|s| s.inset.left = px(3.).into())),
            ("size", Box::new(|s| s.size.width = px(40.).into())),
            ("size in rems", Box::new(|s| s.size.width = rems(2.).into())),
            (
                "size as a fraction",
                Box::new(|s| s.size.width = relative(0.5).into()),
            ),
            ("min_size", Box::new(|s| s.min_size.height = px(5.).into())),
            ("max_size", Box::new(|s| s.max_size.width = px(90.).into())),
            ("aspect_ratio", Box::new(|s| s.aspect_ratio = Some(1.5))),
            ("margin", Box::new(|s| s.margin.top = px(2.).into())),
            ("padding", Box::new(|s| s.padding.bottom = px(4.).into())),
            (
                "border_widths",
                Box::new(|s| s.border_widths.right = px(1.).into()),
            ),
            (
                "align_items",
                Box::new(|s| s.align_items = Some(AlignItems::Center)),
            ),
            (
                "align_self",
                Box::new(|s| s.align_self = Some(AlignItems::End)),
            ),
            (
                "align_content",
                Box::new(|s| s.align_content = Some(AlignContent::End)),
            ),
            (
                "justify_content",
                Box::new(|s| s.justify_content = Some(AlignContent::Center)),
            ),
            ("gap", Box::new(|s| s.gap.width = px(6.).into())),
            (
                "flex_direction",
                Box::new(|s| s.flex_direction = FlexDirection::Column),
            ),
            ("flex_wrap", Box::new(|s| s.flex_wrap = FlexWrap::Wrap)),
            ("flex_basis", Box::new(|s| s.flex_basis = px(12.).into())),
            ("flex_grow", Box::new(|s| s.flex_grow = 1.)),
            ("flex_shrink", Box::new(|s| s.flex_shrink = 0.)),
            (
                "grid_rows",
                Box::new(|s| {
                    s.grid_rows = Some(GridTemplate {
                        repeat: 3,
                        min_size: GridTemplateMinSize::Zero,
                    })
                }),
            ),
            (
                "grid_cols",
                Box::new(|s| {
                    s.grid_cols = Some(GridTemplate {
                        repeat: 2,
                        min_size: GridTemplateMinSize::MinContent,
                    })
                }),
            ),
            (
                "grid_location",
                Box::new(|s| {
                    s.grid_location = Some(GridLocation {
                        row: GridPlacement::Line(1)..GridPlacement::Span(2),
                        column: GridPlacement::Auto..GridPlacement::Auto,
                    })
                }),
            ),
        ];

        let base_fingerprint = layout_fingerprint(&base, rem_size, scale_factor);
        let base_taffy = base.to_taffy(rem_size, scale_factor);
        for (field, change) in &cases {
            let mut style = base.clone();
            change(&mut style);
            assert_ne!(
                style.to_taffy(rem_size, scale_factor),
                base_taffy,
                "changing {field} should change the Taffy style, or this case tests nothing"
            );
            assert_ne!(
                layout_fingerprint(&style, rem_size, scale_factor),
                base_fingerprint,
                "changing {field} changes the Taffy style but not the fingerprint"
            );
        }

        // Lengths are resolved against these, so they are inputs as much as
        // the style is.
        let mut in_rems = base;
        in_rems.size.width = rems(2.).into();
        assert_ne!(
            layout_fingerprint(&in_rems, rem_size, scale_factor),
            layout_fingerprint(&in_rems, px(20.), scale_factor)
        );
        assert_ne!(
            layout_fingerprint(&in_rems, rem_size, scale_factor),
            layout_fingerprint(&in_rems, rem_size, 1.)
        );
    }
}
