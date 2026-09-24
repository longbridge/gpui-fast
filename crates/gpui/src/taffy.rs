use crate::{
    AbsoluteLength, App, Bounds, DefiniteLength, Edges, GridTemplate, Length, Pixels, Point, Size,
    Style, Window, size,
    util::{
        ceil_to_device_pixel, round_half_toward_zero, round_stroke_to_device_pixel,
        round_to_device_pixel,
    },
};
use collections::{FxHashMap, FxHashSet, FxHasher};
use smallvec::SmallVec;
use std::{
    any::Any,
    fmt::Debug,
    hash::{Hash as _, Hasher as _},
    mem,
    ops::Range,
    rc::Rc,
    time::{Duration, Instant},
};
use taffy::{
    TaffyTree, TraversePartialTree as _,
    geometry::{Point as TaffyPoint, Rect as TaffyRect, Size as TaffySize},
    prelude::{max_content, min_content},
    style::AvailableSpace as TaffyAvailableSpace,
    tree::NodeId,
};

#[cfg(feature = "stacker")]
type StackSafe<T> = stacksafe::StackSafe<T>;
#[cfg(not(feature = "stacker"))]
type StackSafe<T> = T;

type MeasureFn =
    dyn FnMut(Size<Option<Pixels>>, Size<AvailableSpace>, &mut Window, &mut App) -> Size<Pixels>;
type NodeMeasureFn = StackSafe<Box<MeasureFn>>;

struct NodeContext {
    measure: NodeMeasureFn,
}

/// Counters describing the work the layout engine performed, for benchmarking
/// and profiling.
///
/// Counts accumulate across frames until [`TaffyLayoutEngine::reset_stats`] is
/// called; clearing the tree between frames does not reset them. Collection is
/// cheap enough to leave enabled in release builds: a few integer increments per
/// node, plus one clock read per call to [`TaffyLayoutEngine::compute_layout`].
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
    /// Times Taffy actually invoked a measurement. A node can be measured more
    /// than once in a layout — for its intrinsic size and then for its final
    /// one — so this runs ahead of the number of measured nodes.
    pub measure_calls: u64,
    /// Time spent inside those measurements, which is time `compute_layout_time`
    /// also counts. The difference between the two is Taffy's own solving.
    pub measure_time: Duration,
    /// Measurements that answered from a result the element had already
    /// computed, rather than computing a new one. Taffy probes a node more than
    /// once per layout, so the gap between this and `measure_calls` is what the
    /// probing actually costs.
    pub measure_reuses: u64,
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
    /// Time spent inside Taffy's own layout computation.
    pub compute_layout_time: Duration,
    /// Lines of text handed to the platform to be shaped. A line the text
    /// cache still held from this frame or the last one is not counted, so this
    /// is the shaping the cache did not save.
    pub lines_shaped: u64,
    /// Time spent shaping those lines. Shaping done while measuring is also
    /// part of `measure_time`; shaping done while painting is part of
    /// `paint_time`.
    pub shape_time: Duration,
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
    /// Present while the node measures its own size.
    measure: Option<RetainedMeasure>,
    /// [`layout_fingerprint`] of the style the node was last asked for. While
    /// the request is the same, converting it to a Taffy style and comparing
    /// that against the node's is work with only one possible outcome.
    style_fingerprint: u64,
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

/// The retained half of a measured leaf.
struct RetainedMeasure {
    /// What the last measurement depended on, as described by the caller.
    /// `None` means the caller could not describe its inputs, so the
    /// measurement is treated as stale every frame.
    key: Option<u64>,
    /// What the measurement closure the node holds was built from, as
    /// described by the caller. While it matches, the closure is kept rather
    /// than built again; `None` means it is built again every frame.
    closure_key: Option<u64>,
    /// Where the caller left the result of that measurement.
    ///
    /// Handed back when the key still matches, because Taffy may then answer
    /// the node's size from cache without calling the measurement at all, and
    /// callers such as text layout keep the artifacts they paint from in here.
    state: Rc<dyn Any>,
}

pub struct TaffyLayoutEngine {
    taffy: TaffyTree<NodeContext>,
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
    absolute_layout_bounds: FxHashMap<LayoutId, Bounds<Pixels>>,
    /// Unrounded absolute border-box top-left per-node coordinate in device pixels.
    absolute_outer_origins: FxHashMap<LayoutId, Point<f32>>,
    computed_layouts: FxHashSet<LayoutId>,
    layout_bounds_scratch_space: Vec<LayoutId>,
    stats: LayoutStats,
}

const EXPECT_MESSAGE: &str = "we should avoid taffy layout errors by construction if possible";

impl TaffyLayoutEngine {
    pub fn new() -> Self {
        let mut taffy = TaffyTree::new();
        taffy.disable_rounding();
        TaffyLayoutEngine {
            taffy,
            retained: FxHashMap::default(),
            transient: Vec::new(),
            unstretched_styles: FxHashMap::default(),
            frame: 0,
            claimed_this_frame: 0,
            absolute_layout_bounds: FxHashMap::default(),
            absolute_outer_origins: FxHashMap::default(),
            computed_layouts: FxHashSet::default(),
            layout_bounds_scratch_space: Vec::new(),
            stats: LayoutStats::default(),
        }
    }

    /// Counters for the work performed since the last call to [`Self::reset_stats`].
    pub fn stats(&self) -> LayoutStats {
        self.stats
    }

    /// Zeroes the counters returned by [`Self::stats`].
    pub fn reset_stats(&mut self) {
        self.stats = LayoutStats::default();
    }

    /// How many nodes the tree is currently holding, retained and transient
    /// alike. Used by tests to check that retention does not leak.
    pub fn node_count(&self) -> usize {
        self.taffy.total_node_count()
    }

    /// Ends the frame: releases nodes that no longer appear in the element
    /// tree and drops the per-frame caches.
    ///
    /// Nodes that were claimed this frame stay, along with their Taffy layout
    /// caches, which is what lets the next frame skip recomputing the parts of
    /// the tree that did not change.
    pub fn end_frame(&mut self) {
        self.stats.frames += 1;

        for id in self.transient.drain(..) {
            self.unstretched_styles.remove(&id);
            self.taffy.remove(id.into()).expect(EXPECT_MESSAGE);
            self.stats.nodes_freed += 1;
        }

        // In a steady frame every retained node was claimed, and there is
        // nothing to sweep.
        if self.retained.len() != self.claimed_this_frame {
            let frame = self.frame;
            let taffy = &mut self.taffy;
            let unstretched_styles = &mut self.unstretched_styles;
            let freed = &mut self.stats.nodes_freed;
            self.retained.retain(|_, node| {
                if node.claimed_in_frame == frame {
                    return true;
                }
                unstretched_styles.remove(&node.id);
                taffy.remove(node.id.into()).expect(EXPECT_MESSAGE);
                *freed += 1;
                false
            });
        }

        self.absolute_layout_bounds.clear();
        self.absolute_outer_origins.clear();
        self.computed_layouts.clear();
        self.claimed_this_frame = 0;
        self.frame += 1;
    }

    /// Takes the node retained under `key` for use in this frame.
    fn claim(&mut self, key: Option<u64>) -> Claim {
        let Some(key) = key else {
            return Claim::Unkeyed;
        };
        let frame = self.frame;
        let Some(node) = self.retained.get_mut(&key) else {
            return Claim::Vacant(key);
        };
        if node.claimed_in_frame == frame {
            // Two elements resolved to one key. Letting both use the node would
            // corrupt the tree, and retaining the second under the same key
            // would strand the first, so the second goes unkeyed.
            return Claim::Unkeyed;
        }
        node.claimed_in_frame = frame;
        self.claimed_this_frame += 1;
        self.stats.nodes_reused += 1;
        Claim::Reused(key, node.id)
    }

    /// Records a freshly allocated node under `key`, or as transient when there
    /// is no key to record it under.
    fn retain(
        &mut self,
        key: Option<u64>,
        id: LayoutId,
        children: &[LayoutId],
        measure: Option<RetainedMeasure>,
        style_fingerprint: u64,
    ) {
        let Some(key) = key else {
            self.transient.push(id);
            return;
        };
        self.retained.insert(
            key,
            RetainedNode {
                id,
                claimed_in_frame: self.frame,
                children: SmallVec::from_slice(children),
                measure,
                style_fingerprint,
            },
        );
        self.claimed_this_frame += 1;
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
            .retained
            .get_mut(&key)
            .expect("a claimed key is always present");
        if node.style_fingerprint == fingerprint {
            self.stats.style_compares += 1;
            // A field `layout_fingerprint` fails to read would leave the node
            // with a stale style whenever only that field changed, and nothing
            // would say so; debug builds compare in full to catch one.
            debug_assert!(
                self.unstretched_styles
                    .get(&id)
                    .unwrap_or_else(|| self.taffy.style(id.0).expect(EXPECT_MESSAGE))
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
        self.stats.style_compares += 1;
        let previous = self
            .unstretched_styles
            .get(&id)
            .unwrap_or_else(|| self.taffy.style(id.0).expect(EXPECT_MESSAGE));
        if previous == &style {
            return;
        }
        // Taffy now holds exactly what was requested, so the stretched-style
        // bookkeeping no longer applies.
        self.unstretched_styles.remove(&id);
        self.stats.style_writes += 1;
        self.taffy.set_style(id.0, style).expect(EXPECT_MESSAGE);
    }

    /// Writes `children` to a retained node, but only if the list changed.
    fn apply_children(&mut self, key: u64, id: LayoutId, children: &[LayoutId]) {
        let node = self
            .retained
            .get_mut(&key)
            .expect("a claimed key is always present");
        if node.children.as_slice() == children {
            return;
        }
        node.children.clear();
        node.children.extend_from_slice(children);
        self.stats.children_writes += 1;
        self.taffy
            // This is safe because LayoutId is repr(transparent) to taffy::tree::NodeId.
            .set_children(id.0, LayoutId::to_taffy_slice(children))
            .expect(EXPECT_MESSAGE);
    }

    /// Adds a node to the layout tree, reusing the one retained under `key`
    /// when there is one.
    ///
    /// `key` identifies this element's position in the element tree across
    /// frames; `None` opts out of reuse, and the node is released at the end of
    /// the frame.
    pub fn request_layout(
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
                    .retained
                    .get(&key)
                    .is_some_and(|node| node.measure.is_some())
                {
                    self.retained
                        .get_mut(&key)
                        .expect("a claimed key is always present")
                        .measure = None;
                    self.taffy
                        .set_node_context(id.0, None)
                        .expect(EXPECT_MESSAGE);
                }
                return id;
            }
            Claim::Vacant(key) => Some(key),
            Claim::Unkeyed => None,
        };

        self.stats.nodes_created += 1;
        let style_fingerprint = layout_fingerprint(&style, rem_size, scale_factor);
        let taffy_style = style.to_taffy(rem_size, scale_factor);
        let id: LayoutId = if children.is_empty() {
            self.taffy
                .new_leaf(taffy_style)
                .expect(EXPECT_MESSAGE)
                .into()
        } else {
            self.taffy
                // This is safe because LayoutId is repr(transparent) to taffy::tree::NodeId.
                .new_with_children(taffy_style, LayoutId::to_taffy_slice(children))
                .expect(EXPECT_MESSAGE)
                .into()
        };
        self.retain(key, id, children, None, style_fingerprint);
        id
    }

    /// Adds a self-measuring leaf to the layout tree, reusing the node retained
    /// under `key` when there is one.
    ///
    /// `measure_key` describes what the measurement depends on. When a retained
    /// node is found whose `measure_key` is unchanged, the node is left clean,
    /// so Taffy may answer its size from cache and never call `measure` at all.
    /// Because callers stash paintable results inside the measurement (text
    /// layout does), the state that went with the previous measurement is
    /// handed to `build_measure` and returned, so the caller can adopt it
    /// instead of starting from an empty one.
    ///
    /// Passing `measure_key: None` keeps the old behaviour: the node is dirtied
    /// every frame and `measure` is guaranteed to run.
    pub fn request_measured_layout(
        &mut self,
        key: Option<u64>,
        style: Style,
        rem_size: Pixels,
        scale_factor: f32,
        measure_key: Option<u64>,
        closure_key: Option<u64>,
        fresh_state: Rc<dyn Any>,
        build_measure: impl FnOnce(&Rc<dyn Any>) -> Box<MeasureFn>,
    ) -> (LayoutId, Rc<dyn Any>) {
        let (key, id) = match self.claim(key) {
            Claim::Reused(key, id) => (key, id),
            claim => {
                let key = match claim {
                    Claim::Vacant(key) => Some(key),
                    _ => None,
                };
                let measure = build_measure(&fresh_state);
                #[cfg(feature = "stacker")]
                let measure = StackSafe::new(measure);
                let style_fingerprint = layout_fingerprint(&style, rem_size, scale_factor);
                let taffy_style = style.to_taffy(rem_size, scale_factor);
                self.stats.nodes_created += 1;
                self.stats.measure_rebinds += 1;
                let id: LayoutId = self
                    .taffy
                    .new_leaf_with_context(taffy_style, NodeContext { measure })
                    .expect(EXPECT_MESSAGE)
                    .into();
                self.retain(
                    key,
                    id,
                    &[],
                    Some(RetainedMeasure {
                        key: measure_key,
                        closure_key,
                        state: fresh_state.clone(),
                    }),
                    style_fingerprint,
                );
                return (id, fresh_state);
            }
        };

        self.apply_requested_style(key, id, &style, rem_size, scale_factor);
        self.apply_children(key, id, &[]);

        // The previous measurement still stands only if the caller described
        // its inputs and they have not changed. The type check guards against a
        // key collision handing back state of an unrelated kind.
        let node = self
            .retained
            .get_mut(&key)
            .expect("a claimed key is always present");
        let previous = node.measure.take();
        let reusable = match &previous {
            Some(previous) => {
                measure_key.is_some()
                    && previous.key == measure_key
                    && (*previous.state).type_id() == (*fresh_state).type_id()
            }
            None => false,
        };
        // The closure the node holds was built around the state being handed
        // back, from inputs the caller says are unchanged, so it is the closure
        // that would be built now.
        let keeps_closure = reusable
            && closure_key.is_some()
            && previous
                .as_ref()
                .is_some_and(|previous| previous.closure_key == closure_key);
        let state = match (reusable, previous) {
            (true, Some(previous)) => previous.state,
            _ => fresh_state,
        };

        if !keeps_closure {
            let measure = build_measure(&state);
            #[cfg(feature = "stacker")]
            let measure = StackSafe::new(measure);

            // Swapping the closure in place leaves the node clean. Going
            // through `set_node_context` would dirty it, which is exactly what
            // a reusable measurement must avoid.
            if let Some(context) = self.taffy.get_node_context_mut(id.0) {
                context.measure = measure;
            } else {
                self.taffy
                    .set_node_context(id.0, Some(NodeContext { measure }))
                    .expect(EXPECT_MESSAGE);
            }
        }

        if !reusable {
            self.stats.measure_rebinds += 1;
            self.taffy.mark_dirty(id.0).expect(EXPECT_MESSAGE);
        }

        self.retained
            .get_mut(&key)
            .expect("a claimed key is always present")
            .measure = Some(RetainedMeasure {
            key: measure_key,
            closure_key,
            state: state.clone(),
        });

        (id, state)
    }

    /// Treats any `auto` dimension of the given node's style as filling `size`.
    ///
    /// This is applied to window roots before layout so they behave like the
    /// root element on the web, which stretches to fill the initial containing
    /// block (the viewport) unless given an explicit size. Explicitly styled
    /// dimensions are preserved.
    ///
    /// The style Taffy ends up holding is not the one the element asked for, and
    /// the difference is not recoverable from the result — a stretched `auto`
    /// looks exactly like an explicit length. The requested style is therefore
    /// kept aside so the next frame compares like with like instead of
    /// rewriting, and dirtying, the root on every frame.
    pub fn stretch_auto_size_to_fill(
        &mut self,
        id: LayoutId,
        size: Size<Pixels>,
        scale_factor: f32,
    ) {
        let requested = match self.unstretched_styles.get(&id) {
            Some(requested) => requested,
            None => self.taffy.style(id.0).expect(EXPECT_MESSAGE),
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
        if self.taffy.style(id.0).expect(EXPECT_MESSAGE) != &style {
            self.stats.style_writes += 1;
            self.taffy.set_style(id.0, style).expect(EXPECT_MESSAGE);
        }
        self.unstretched_styles.insert(id, requested);
    }

    // Used to understand performance
    #[allow(dead_code)]
    fn count_all_children(&self, parent: LayoutId) -> anyhow::Result<u32> {
        let mut count = 0;

        for child in self.taffy.children(parent.0)? {
            // Count this child.
            count += 1;

            // Count all of this child's children.
            count += self.count_all_children(LayoutId(child))?
        }

        Ok(count)
    }

    // Used to understand performance
    #[allow(dead_code)]
    fn max_depth(&self, depth: u32, parent: LayoutId) -> anyhow::Result<u32> {
        println!(
            "{parent:?} at depth {depth} has {} children",
            self.taffy.child_count(parent.0)
        );

        let mut max_child_depth = 0;

        for child in self.taffy.children(parent.0)? {
            max_child_depth = std::cmp::max(max_child_depth, self.max_depth(0, LayoutId(child))?);
        }

        Ok(depth + 1 + max_child_depth)
    }

    // Used to understand performance
    #[allow(dead_code)]
    fn get_edges(&self, parent: LayoutId) -> anyhow::Result<Vec<(LayoutId, LayoutId)>> {
        let mut edges = Vec::new();

        for child in self.taffy.children(parent.0)? {
            edges.push((parent, LayoutId(child)));

            edges.extend(self.get_edges(LayoutId(child))?);
        }

        Ok(edges)
    }

    #[cfg_attr(feature = "stacker", stacksafe::stacksafe)]
    pub fn compute_layout(
        &mut self,
        id: LayoutId,
        available_space: Size<AvailableSpace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Leaving this here until we have a better instrumentation approach.
        // println!("Laying out {} children", self.count_all_children(id)?);
        // println!("Max layout depth: {}", self.max_depth(0, id)?);

        // Output the edges (branches) of the tree in Mermaid format for visualization.
        // println!("Edges:");
        // for (a, b) in self.get_edges(id)? {
        //     println!("N{} --> N{}", u64::from(a), u64::from(b));
        // }
        //

        if !self.computed_layouts.insert(id) {
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
        }

        let scale_factor = window.scale_factor();

        let transform = |v: AvailableSpace| match v {
            AvailableSpace::Definite(pixels) => {
                AvailableSpace::Definite(Pixels(pixels.0 * scale_factor))
            }
            AvailableSpace::MinContent => AvailableSpace::MinContent,
            AvailableSpace::MaxContent => AvailableSpace::MaxContent,
        };
        let available_space = size(
            transform(available_space.width),
            transform(available_space.height),
        );

        // Accumulated outside `self` because the closure below borrows the tree.
        let mut measure_calls = 0;
        let mut measure_time = Duration::ZERO;

        let compute_started_at = Instant::now();
        self.taffy
            .compute_layout_with_measure(
                id.into(),
                available_space.into(),
                |known_dimensions, available_space, _id, node_context, _style| {
                    let Some(node_context) = node_context else {
                        return taffy::geometry::Size::default();
                    };
                    let measure_started_at = Instant::now();

                    let known_dimensions = Size {
                        width: known_dimensions.width.map(|e| Pixels(e / scale_factor)),
                        height: known_dimensions.height.map(|e| Pixels(e / scale_factor)),
                    };

                    let available_space: Size<AvailableSpace> = available_space.into();
                    let untransform = |ev: AvailableSpace| match ev {
                        AvailableSpace::Definite(pixels) => {
                            AvailableSpace::Definite(Pixels(pixels.0 / scale_factor))
                        }
                        AvailableSpace::MinContent => AvailableSpace::MinContent,
                        AvailableSpace::MaxContent => AvailableSpace::MaxContent,
                    };
                    let available_space = size(
                        untransform(available_space.width),
                        untransform(available_space.height),
                    );

                    let measured_size: Size<Pixels> =
                        (node_context.measure)(known_dimensions, available_space, window, cx);
                    measure_calls += 1;
                    measure_time += measure_started_at.elapsed();
                    snap_measured_size_to_device_pixels(measured_size, scale_factor).into()
                },
            )
            .expect(EXPECT_MESSAGE);
        self.stats.compute_layout_calls += 1;
        self.stats.compute_layout_time += compute_started_at.elapsed();
        self.stats.measure_calls += measure_calls;
        self.stats.measure_time += measure_time;
        self.stats.measure_reuses += std::mem::take(&mut window.pending_measure_reuses);
    }

    // Pixel snapping
    //
    // Painting primitives at non-integer pixel coordinates produces blurry
    // output. Pixel snapping converts layout coordinates into integer
    // device-pixel coordinates so painted edges land exactly on physical
    // pixel boundaries.
    //
    // Non-integer coordinates can arise for several reasons, including:
    //   - flex distribution, percentages, centering, and text measurement
    //     can produce fractional element sizes and positions;
    //   - at fractional scale factors (for example 125% or 150%), integer
    //     logical-pixel values can map to non-integer device-pixel values.
    //
    // We pixel-snap by rounding in device-pixel space, after multiplying
    // by `scale_factor`, so that snapping targets physical pixels. Bounds
    // are divided by `scale_factor` before being returned to GPUI.
    //
    // Midpoints are rounded toward zero. This is a stylistic choice: a
    // 1-logical-pixel line at 150% scale should render as 1 dp rather than
    // 2 dp.
    //
    // Pixel snapping is done in two phases:
    //
    //  1. Pre-layout metric snapping. Before Taffy computes layout, all
    //     authored absolute lengths are rounded in `to_taffy`. This
    //     includes borders, padding, gaps, and explicit sizes.
    //     Custom-measured leaf nodes have their measured sizes rounded up
    //     to integer device-pixel lengths.
    //
    //  2. Post-layout edge snapping. After Taffy resolves the tree, layout
    //     relationships such as flex shares, grid tracks, percentages, and
    //     centering can produce new fractional edge positions. Boxes now
    //     have edges in absolute coordinates, and snapping must decide
    //     where those edges land on the device-pixel grid.
    //
    // Ideally, post-layout snapping would satisfy:
    //
    //  - Edge closure. Two raw layout edges at the same absolute position
    //    should snap to the same pixel column.
    //  - Translation stability. A component's internal geometry should not
    //    change when it moves to a new absolute position.
    //
    // These goals are in tension because rounding is not associative.
    // The simple local schemes make different tradeoffs:
    //
    //  - Absolute edge rounding gives each window coordinate one answer,
    //    so coincident edges always close globally. But a span's snapped
    //    length is `round(far) - round(near)`, which may change by 1 dp
    //    as its absolute origin moves.
    //
    //  - Parent-relative edge rounding rounds each child inside its
    //    parent's coordinate space. This guarantees translation stability,
    //    but a shared edge reached through different parents can
    //    accumulate different rounding, causing non-closure between
    //    cousins.
    //
    //  - Length rounding rounds each width, height, and thickness
    //    independently and then places boxes from those rounded lengths.
    //    Sizes stay stable under translation, but neighboring boxes derive
    //    their shared boundary from different sources, so closure is not
    //    guaranteed.
    //
    // We apply absolute edge rounding for each element's outer box in
    // post-layout rounding to preserve closure. Border and padding widths
    // are not touched by post-layout rounding; they keep their pre-layout
    // rounded value so that they remain stable under translation.
    //
    // This gives both closure and translation stability in the case that
    // all local metrics are integer device-pixel lengths. Pre-layout
    // rounding covers that in most cases. The exception is metrics
    // resolved by layout relationships, such as percentages. Outer box
    // edges will still close globally, and painted border widths are still
    // snapped independently, but the raw content-box origin can carry a
    // 1dp residual into descendants.

    pub fn layout_bounds(&mut self, id: LayoutId, scale_factor: f32) -> Bounds<Pixels> {
        if let Some(layout) = self.absolute_layout_bounds.get(&id).cloned() {
            return layout;
        }

        let layout = self.taffy.layout(id.into()).expect(EXPECT_MESSAGE);
        let layout_location = layout.location;
        let layout_size = layout.size;
        let parent = self.taffy.parent(id.0);

        let absolute_outer_origin = match parent {
            Some(parent_id) => {
                let parent_id = LayoutId::from(parent_id);
                self.layout_bounds(parent_id, scale_factor);
                let parent_origin = *self
                    .absolute_outer_origins
                    .get(&parent_id)
                    .expect("parent absolute outer origin should be cached");
                parent_origin + Point::from(layout_location)
            }
            None => Point::from(layout_location),
        };
        self.absolute_outer_origins
            .insert(id, absolute_outer_origin);

        let absolute_far = absolute_outer_origin + Point::from(Size::from(layout_size));
        let snapped_bounds = Bounds::from_corners(
            absolute_outer_origin.map(round_half_toward_zero),
            absolute_far.map(round_half_toward_zero),
        );

        let bounds = (snapped_bounds / scale_factor).map(Pixels);
        self.absolute_layout_bounds.insert(id, bounds);
        bounds
    }
}

/// A unique identifier for a layout node, generated when requesting a layout from Taffy
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(transparent)]
pub struct LayoutId(NodeId);

impl LayoutId {
    fn to_taffy_slice(node_ids: &[Self]) -> &[taffy::NodeId] {
        // SAFETY: LayoutId is repr(transparent) to taffy::tree::NodeId.
        unsafe { std::mem::transmute::<&[LayoutId], &[taffy::NodeId]>(node_ids) }
    }
}

impl std::hash::Hash for LayoutId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        u64::from(self.0).hash(state);
    }
}

impl From<NodeId> for LayoutId {
    fn from(node_id: NodeId) -> Self {
        Self(node_id)
    }
}

impl From<LayoutId> for NodeId {
    fn from(layout_id: LayoutId) -> NodeId {
        layout_id.0
    }
}

fn snap_measured_size_to_device_pixels(size: Size<Pixels>, scale_factor: f32) -> Size<f32> {
    size.map(|d| ceil_to_device_pixel(d.0.max(0.0), scale_factor))
}

fn border_widths_to_taffy(
    widths: &Edges<AbsoluteLength>,
    rem_size: Pixels,
    scale_factor: f32,
) -> TaffyRect<taffy::style::LengthPercentage> {
    let snap = |w: &AbsoluteLength| {
        taffy::style::LengthPercentage::length(round_stroke_to_device_pixel(
            w.to_pixels(rem_size).0,
            scale_factor,
        ))
    };
    TaffyRect {
        top: snap(&widths.top),
        right: snap(&widths.right),
        bottom: snap(&widths.bottom),
        left: snap(&widths.left),
    }
}

trait ToTaffy<Output> {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> Output;
}

/// A hash of everything in `style` that its conversion to a Taffy style reads,
/// and of what that conversion resolves lengths against.
///
/// It has to read exactly the fields [`ToTaffy`] does: a field it misses leaves
/// a retained node with a stale style whenever only that field changes. Debug
/// builds check every match against a full conversion.
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

impl ToTaffy<taffy::style::Style> for Style {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::style::Style {
        use taffy::style_helpers::{fr, length, minmax, repeat};

        fn to_grid_line(
            placement: &Range<crate::GridPlacement>,
        ) -> taffy::Line<taffy::GridPlacement> {
            taffy::Line {
                start: placement.start.into(),
                end: placement.end.into(),
            }
        }

        fn to_grid_repeat<T: taffy::style::CheapCloneStr>(
            unit: &Option<GridTemplate>,
        ) -> Vec<taffy::GridTemplateComponent<T>> {
            unit.map(|template| {
                match template.min_size {
                    // grid-template-*: repeat(<number>, minmax(0, 1fr));
                    crate::GridTemplateMinSize::Zero => {
                        vec![repeat(
                            template.repeat,
                            vec![minmax(length(0.0_f32), fr(1.0_f32))],
                        )]
                    }
                    // grid-template-*: repeat(<number>, minmax(min-content, 1fr));
                    crate::GridTemplateMinSize::MinContent => {
                        vec![repeat(
                            template.repeat,
                            vec![minmax(min_content(), fr(1.0_f32))],
                        )]
                    }
                    // grid-template-*: repeat(<number>, minmax(0, max-content))
                    crate::GridTemplateMinSize::MaxContent => {
                        vec![repeat(
                            template.repeat,
                            vec![minmax(length(0.0_f32), max_content())],
                        )]
                    }
                }
            })
            .unwrap_or_default()
        }

        taffy::style::Style {
            display: self.display.into(),
            overflow: self.overflow.into(),
            scrollbar_width: self.scrollbar_width.to_taffy(rem_size, scale_factor),
            position: self.position.into(),
            inset: self.inset.to_taffy(rem_size, scale_factor),
            size: self.size.to_taffy(rem_size, scale_factor),
            min_size: self.min_size.to_taffy(rem_size, scale_factor),
            max_size: self.max_size.to_taffy(rem_size, scale_factor),
            aspect_ratio: self.aspect_ratio,
            margin: self.margin.to_taffy(rem_size, scale_factor),
            padding: self.padding.to_taffy(rem_size, scale_factor),
            border: border_widths_to_taffy(&self.border_widths, rem_size, scale_factor),
            align_items: self.align_items.map(|x| x.into()),
            align_self: self.align_self.map(|x| x.into()),
            align_content: self.align_content.map(|x| x.into()),
            justify_content: self.justify_content.map(|x| x.into()),
            gap: self.gap.to_taffy(rem_size, scale_factor),
            flex_direction: self.flex_direction.into(),
            flex_wrap: self.flex_wrap.into(),
            flex_basis: self.flex_basis.to_taffy(rem_size, scale_factor),
            flex_grow: self.flex_grow,
            flex_shrink: self.flex_shrink,
            grid_template_rows: to_grid_repeat(&self.grid_rows),
            grid_template_columns: to_grid_repeat(&self.grid_cols),
            grid_row: self
                .grid_location
                .as_ref()
                .map(|location| to_grid_line(&location.row))
                .unwrap_or_default(),
            grid_column: self
                .grid_location
                .as_ref()
                .map(|location| to_grid_line(&location.column))
                .unwrap_or_default(),
            ..Default::default()
        }
    }
}

impl ToTaffy<f32> for AbsoluteLength {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> f32 {
        round_to_device_pixel(self.to_pixels(rem_size).0, scale_factor)
    }
}

impl ToTaffy<taffy::style::LengthPercentageAuto> for Length {
    fn to_taffy(
        &self,
        rem_size: Pixels,
        scale_factor: f32,
    ) -> taffy::prelude::LengthPercentageAuto {
        match self {
            Length::Definite(length) => length.to_taffy(rem_size, scale_factor),
            Length::Auto => taffy::prelude::LengthPercentageAuto::auto(),
        }
    }
}

impl ToTaffy<taffy::style::Dimension> for Length {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::prelude::Dimension {
        match self {
            Length::Definite(length) => length.to_taffy(rem_size, scale_factor),
            Length::Auto => taffy::prelude::Dimension::auto(),
        }
    }
}

impl ToTaffy<taffy::style::LengthPercentage> for DefiniteLength {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::style::LengthPercentage {
        match self {
            DefiniteLength::Absolute(length) => length.to_taffy(rem_size, scale_factor),
            DefiniteLength::Fraction(fraction) => {
                taffy::style::LengthPercentage::percent(*fraction)
            }
        }
    }
}

impl ToTaffy<taffy::style::LengthPercentageAuto> for DefiniteLength {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::style::LengthPercentageAuto {
        match self {
            DefiniteLength::Absolute(length) => length.to_taffy(rem_size, scale_factor),
            DefiniteLength::Fraction(fraction) => {
                taffy::style::LengthPercentageAuto::percent(*fraction)
            }
        }
    }
}

impl ToTaffy<taffy::style::Dimension> for DefiniteLength {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::style::Dimension {
        match self {
            DefiniteLength::Absolute(length) => length.to_taffy(rem_size, scale_factor),
            DefiniteLength::Fraction(fraction) => taffy::style::Dimension::percent(*fraction),
        }
    }
}

impl ToTaffy<taffy::style::LengthPercentage> for AbsoluteLength {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::style::LengthPercentage {
        taffy::style::LengthPercentage::length(self.to_taffy(rem_size, scale_factor))
    }
}

impl ToTaffy<taffy::style::LengthPercentageAuto> for AbsoluteLength {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::style::LengthPercentageAuto {
        taffy::style::LengthPercentageAuto::length(self.to_taffy(rem_size, scale_factor))
    }
}

impl ToTaffy<taffy::style::Dimension> for AbsoluteLength {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::style::Dimension {
        taffy::style::Dimension::length(self.to_taffy(rem_size, scale_factor))
    }
}

impl<T, T2> From<TaffyPoint<T>> for Point<T2>
where
    T: Into<T2>,
    T2: Clone + Debug + Default + PartialEq,
{
    fn from(point: TaffyPoint<T>) -> Point<T2> {
        Point {
            x: point.x.into(),
            y: point.y.into(),
        }
    }
}

impl<T, T2> From<Point<T>> for TaffyPoint<T2>
where
    T: Into<T2> + Clone + Debug + Default + PartialEq,
{
    fn from(val: Point<T>) -> Self {
        TaffyPoint {
            x: val.x.into(),
            y: val.y.into(),
        }
    }
}

impl<T, U> ToTaffy<TaffySize<U>> for Size<T>
where
    T: ToTaffy<U> + Clone + Debug + Default + PartialEq,
{
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> TaffySize<U> {
        TaffySize {
            width: self.width.to_taffy(rem_size, scale_factor),
            height: self.height.to_taffy(rem_size, scale_factor),
        }
    }
}

impl<T, U> ToTaffy<TaffyRect<U>> for Edges<T>
where
    T: ToTaffy<U> + Clone + Debug + Default + PartialEq,
{
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> TaffyRect<U> {
        TaffyRect {
            top: self.top.to_taffy(rem_size, scale_factor),
            right: self.right.to_taffy(rem_size, scale_factor),
            bottom: self.bottom.to_taffy(rem_size, scale_factor),
            left: self.left.to_taffy(rem_size, scale_factor),
        }
    }
}

impl<T, U> From<TaffySize<T>> for Size<U>
where
    T: Into<U>,
    U: Clone + Debug + Default + PartialEq,
{
    fn from(taffy_size: TaffySize<T>) -> Self {
        Size {
            width: taffy_size.width.into(),
            height: taffy_size.height.into(),
        }
    }
}

impl<T, U> From<Size<T>> for TaffySize<U>
where
    T: Into<U> + Clone + Debug + Default + PartialEq,
{
    fn from(size: Size<T>) -> Self {
        TaffySize {
            width: size.width.into(),
            height: size.height.into(),
        }
    }
}

/// The space available for an element to be laid out in
#[derive(Copy, Clone, Default, Debug, Eq, PartialEq)]
pub enum AvailableSpace {
    /// The amount of space available is the specified number of pixels
    Definite(Pixels),
    /// The amount of space available is indefinite and the node should be laid out under a min-content constraint
    #[default]
    MinContent,
    /// The amount of space available is indefinite and the node should be laid out under a max-content constraint
    MaxContent,
}

impl AvailableSpace {
    /// Returns a `Size` with both width and height set to `AvailableSpace::MinContent`.
    ///
    /// This function is useful when you want to create a `Size` with the minimum content constraints
    /// for both dimensions.
    ///
    /// # Examples
    ///
    /// ```
    /// use gpui::AvailableSpace;
    /// let min_content_size = AvailableSpace::min_size();
    /// assert_eq!(min_content_size.width, AvailableSpace::MinContent);
    /// assert_eq!(min_content_size.height, AvailableSpace::MinContent);
    /// ```
    pub const fn min_size() -> Size<Self> {
        Size {
            width: Self::MinContent,
            height: Self::MinContent,
        }
    }
}

impl From<AvailableSpace> for TaffyAvailableSpace {
    fn from(space: AvailableSpace) -> TaffyAvailableSpace {
        match space {
            AvailableSpace::Definite(Pixels(value)) => TaffyAvailableSpace::Definite(value),
            AvailableSpace::MinContent => TaffyAvailableSpace::MinContent,
            AvailableSpace::MaxContent => TaffyAvailableSpace::MaxContent,
        }
    }
}

impl From<TaffyAvailableSpace> for AvailableSpace {
    fn from(space: TaffyAvailableSpace) -> AvailableSpace {
        match space {
            TaffyAvailableSpace::Definite(value) => AvailableSpace::Definite(Pixels(value)),
            TaffyAvailableSpace::MinContent => AvailableSpace::MinContent,
            TaffyAvailableSpace::MaxContent => AvailableSpace::MaxContent,
        }
    }
}

impl From<Pixels> for AvailableSpace {
    fn from(pixels: Pixels) -> Self {
        AvailableSpace::Definite(pixels)
    }
}

impl From<Size<Pixels>> for Size<AvailableSpace> {
    fn from(size: Size<Pixels>) -> Self {
        Size {
            width: AvailableSpace::Definite(size.width),
            height: AvailableSpace::Definite(size.height),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field the conversion to a Taffy style reads has to reach the
    /// fingerprint too: a change the fingerprint cannot see is one a retained
    /// node never receives. Each case changes one field that the conversion
    /// reads, and both have to notice.
    #[test]
    fn the_layout_fingerprint_sees_every_field_the_taffy_style_is_made_from() {
        use crate::{
            AlignContent, AlignItems, Display, FlexDirection, FlexWrap, GridLocation,
            GridPlacement, GridTemplateMinSize, Overflow, Position, px, relative, rems,
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
        let mut in_rems = base.clone();
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

    #[test]
    fn border_widths_to_taffy_use_stroke_snapping() {
        let border_widths = Edges {
            top: Pixels(0.0).into(),
            right: Pixels(0.4).into(),
            bottom: Pixels(0.5).into(),
            left: Pixels(1.6).into(),
        };
        let taffy_border = border_widths_to_taffy(&border_widths, Pixels(16.0), 1.0);

        assert_eq!(
            taffy_border.top,
            taffy::style::LengthPercentage::length(0.0)
        );
        assert_eq!(
            taffy_border.right,
            taffy::style::LengthPercentage::length(1.0)
        );
        assert_eq!(
            taffy_border.bottom,
            taffy::style::LengthPercentage::length(1.0)
        );
        assert_eq!(
            taffy_border.left,
            taffy::style::LengthPercentage::length(2.0)
        );
    }
}
