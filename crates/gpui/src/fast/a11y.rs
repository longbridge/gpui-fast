//! Accessibility nodes drawn again from the last frame.
//!
//! While assistive technology is attached, a window builds an AccessKit tree
//! every frame and hands the whole of it to the platform: each element with
//! an id and a role pushes a node as it prepaints, its children's nodes
//! hang off it, and a focusable element says whether it holds the focus.
//! A retained subtree drawn again from the last frame is not prepainted, so
//! none of that would happen for it.
//!
//! Instead, a frame logs what building its tree did, in order — a node
//! pushed, a leaf added, a node finished, an element made focusable, an
//! active descendant claimed — and keeps where in the log each stretch of
//! the frame starts and ends, as it keeps where its hitboxes do
//! ([`crate::window::PrepaintStateIndex::fast_a11y_index`]). Drawing a
//! stretch again from the last frame does again what its part of the log
//! did, through the same builder: nodes are pushed where the stretch lands,
//! so they hang off whatever node is open there; a finished node is last
//! frame's node with the children this frame gave it, so a view rebuilt
//! inside a stretch drawn again (see [`crate::fast::splice`]) hangs its own
//! nodes off the nodes around it; and the focus is asked again, so a node
//! is focused if its element's focus handle is focused now. The finished
//! nodes are read from the tree last sent, which the window keeps anyway
//! for debugging. A stretch drawn again logs what it did as the frame it
//! lands in logs anything, so it can be drawn again on the next frame.
//!
//! The listeners an element registers for accessibility actions as it
//! paints are logged too, and a stretch of paint drawn again moves them
//! from last frame's into this frame's, as it moves mouse listeners.
//!
//! A scroll layer's content (see [`crate::fast::layers`]) is composited at
//! another offset than it was painted at: its nodes are kept, as painted,
//! in an [`A11yStretch`], and done again moved by the scroll since, as its
//! hitboxes are.

use crate::{
    Bounds, FocusId, PaintIndex, Pixels, Point, PrepaintStateIndex, Window,
    window::a11y::{A11y, A11yActionListener},
};
use accesskit::{Action, NodeId};
use collections::FxHashMap;
use std::{mem, ops::Range, rc::Rc};

/// One thing building a frame's accessibility tree did.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Op {
    /// A node pushed: it hangs off the node open then and is open until
    /// its [`Op::Pop`].
    Push(NodeId),
    /// A leaf added under the node open, finished at this index of the
    /// frame's nodes.
    Leaf(usize),
    /// The node open finished, at this index of the frame's nodes.
    Pop(usize),
    /// A node made focusable with a focus handle, focused if the handle is.
    Focusable(NodeId, FocusId),
    /// A node claiming to be the active descendant of the focused node.
    ActiveDescendant(NodeId),
}

/// What a window's accessibility tree builder logs, and last frame's log
/// to draw stretches again from. Lives in the builder.
#[derive(Default)]
pub(crate) struct A11yLog {
    /// Whether the frame being drawn logs: accessibility was active as it
    /// began.
    recording: bool,
    /// Whether the last frame was logged, so it can be drawn again from.
    previous_recorded: bool,
    ops: Vec<Op>,
    previous_ops: Vec<Op>,
    /// The accessibility action listeners registered while painting, in
    /// order, by the node they listen on.
    actions: Vec<(NodeId, Action)>,
    /// Last frame's, with where each listener is among those of its node.
    previous_actions: Vec<(NodeId, Action, usize)>,
    previous_listeners: FxHashMap<NodeId, Vec<Option<(Action, A11yActionListener)>>>,
    previous_bounds: FxHashMap<NodeId, Bounds<Pixels>>,
    #[cfg(debug_assertions)]
    previous_info: FxHashMap<NodeId, crate::window::a11y::debug::NodeDebugInfo>,
    /// For each node pushed again and not yet finished, whether it was
    /// pushed. Kept through the frame: a view drawn again around a nested
    /// view built again finishes its nodes after the nested view.
    replay_open: Vec<bool>,
}

impl A11yLog {
    #[inline]
    pub(crate) fn push(log: &mut Self, id: NodeId) {
        if log.recording {
            log.ops.push(Op::Push(id));
        }
    }

    #[inline]
    pub(crate) fn leaf(log: &mut Self, at: usize) {
        if log.recording {
            log.ops.push(Op::Leaf(at));
        }
    }

    #[inline]
    pub(crate) fn pop(log: &mut Self, at: usize) {
        if log.recording {
            log.ops.push(Op::Pop(at));
        }
    }

    #[inline]
    pub(crate) fn focusable(log: &mut Self, node: NodeId, focus: FocusId) {
        if log.recording {
            log.ops.push(Op::Focusable(node, focus));
        }
    }

    #[inline]
    pub(crate) fn active_descendant(log: &mut Self, node: NodeId) {
        if log.recording {
            log.ops.push(Op::ActiveDescendant(node));
        }
    }

    #[inline]
    pub(crate) fn action(log: &mut Self, node: NodeId, action: Action) {
        if log.recording {
            log.actions.push((node, action));
        }
    }

    /// Ends the frame's log, which becomes the last frame's.
    pub(crate) fn end_frame(log: &mut Self) {
        mem::swap(&mut log.ops, &mut log.previous_ops);
        log.ops.clear();
    }

    /// Starts a frame, whether accessibility is active or not.
    pub(crate) fn new_frame(log: &mut Self) {
        log.previous_recorded = log.recording;
        log.recording = false;
    }
}

/// Starts logging a frame in which accessibility is active, keeping what
/// the last frame left that is about to be cleared: the bounds of its
/// nodes, its action listeners and, in debug builds, where its nodes came
/// from.
pub(crate) fn begin_frame(a11y: &mut A11y) {
    let log = &mut a11y.nodes.fast;
    log.recording = true;
    log.replay_open.clear();
    log.ops.clear();
    mem::swap(&mut log.previous_bounds, &mut a11y.node_bounds);
    let mut counts: FxHashMap<NodeId, usize> = FxHashMap::default();
    log.previous_actions.clear();
    for (node, action) in log.actions.drain(..) {
        let count = counts.entry(node).or_default();
        log.previous_actions.push((node, action, *count));
        *count += 1;
    }
    log.previous_listeners = mem::take(&mut a11y.action_listeners)
        .into_iter()
        .map(|(node, listeners)| (node, listeners.into_iter().map(Some).collect()))
        .collect();
    #[cfg(debug_assertions)]
    {
        log.previous_info = mem::take(&mut a11y.nodes.node_info);
    }
}

/// Where the frame being drawn stands in its log, for a stretch of its
/// prepaint to know where its part of the log lies.
#[inline]
pub(crate) fn ops_index(window: &Window) -> usize {
    window.a11y.nodes.fast.ops.len()
}

/// Where the frame being drawn stands in its log of action listeners.
#[inline]
pub(crate) fn actions_index(window: &Window) -> usize {
    window.a11y.nodes.fast.actions.len()
}

/// Whether accessibility is active but the last frame was drawn without
/// it, so it logged nothing to draw anything again from.
#[inline]
pub(crate) fn stale(a11y: &A11y) -> bool {
    a11y.is_active() && !a11y.nodes.fast.previous_recorded
}

/// What drawing a stretch again draws from: a log, the finished nodes its
/// indices point into, and the bounds and debug information of its nodes.
struct Source<'a> {
    ops: &'a [Op],
    nodes: &'a [(NodeId, accesskit::Node)],
    bounds: &'a FxHashMap<NodeId, Bounds<Pixels>>,
    #[cfg(debug_assertions)]
    info: &'a FxHashMap<NodeId, crate::window::a11y::debug::NodeDebugInfo>,
}

/// Does again what building the accessibility tree did over `range` of
/// the last frame's prepaint, which the frame being drawn copies.
#[inline]
pub(crate) fn reuse_prepaint(window: &mut Window, range: &Range<PrepaintStateIndex>) {
    let ops = range.start.fast_a11y_index..range.end.fast_a11y_index;
    if ops.is_empty() || !window.a11y.is_active() {
        return;
    }
    reuse_ops(window, ops);
}

#[inline(never)]
fn reuse_ops(window: &mut Window, range: Range<usize>) {
    let log = &mut window.a11y.nodes.fast;
    if !log.previous_recorded {
        debug_assert!(
            false,
            "drew again from a frame that logged no accessibility"
        );
        return;
    }
    let ops = mem::take(&mut log.previous_ops);
    let bounds = mem::take(&mut log.previous_bounds);
    #[cfg(debug_assertions)]
    let info = mem::take(&mut log.previous_info);
    let update = window.a11y.debug.last_tree_update.take();
    if let Some(ops_range) = ops.get(range) {
        let source = Source {
            ops: ops_range,
            nodes: update.as_ref().map_or(&[], |update| &update.nodes),
            bounds: &bounds,
            #[cfg(debug_assertions)]
            info: &info,
        };
        replay(window, &source, Point::default());
    } else {
        debug_assert!(
            false,
            "a stretch reaches past last frame's accessibility log"
        );
    }
    window.a11y.debug.last_tree_update = update;
    let log = &mut window.a11y.nodes.fast;
    log.previous_ops = ops;
    log.previous_bounds = bounds;
    #[cfg(debug_assertions)]
    {
        log.previous_info = info;
    }
}

/// Does again what `source` logged, its nodes moved by `delta`.
fn replay(window: &mut Window, source: &Source, delta: Point<Pixels>) {
    let scale = window.scale_factor();
    let shift = (delta != Point::default())
        .then(|| ((delta.x.0 * scale) as f64, (delta.y.0 * scale) as f64));
    let moved = |node: &accesskit::Node| {
        let mut node = node.clone();
        if let Some((dx, dy)) = shift
            && let Some(rect) = node.bounds()
        {
            node.set_bounds(accesskit::Rect {
                x0: rect.x0 + dx,
                y0: rect.y0 + dy,
                x1: rect.x1 + dx,
                y1: rect.y1 + dy,
            });
        }
        node
    };
    for op in source.ops {
        match *op {
            Op::Push(id) => {
                let pushed = window.a11y.nodes.push(id, accesskit::Node::default());
                window.a11y.nodes.fast.replay_open.push(pushed);
                if pushed {
                    if let Some(bounds) = source.bounds.get(&id) {
                        let bounds = Bounds::new(bounds.origin + delta, bounds.size);
                        window.a11y.node_bounds.insert(id, bounds);
                    }
                    #[cfg(debug_assertions)]
                    if let Some(info) = source.info.get(&id) {
                        window.a11y.nodes.record_node_info(id, info.clone());
                    }
                }
            }
            Op::Leaf(at) => {
                let Some((id, node)) = source.nodes.get(at) else {
                    debug_assert!(false, "a leaf missing from last frame's tree");
                    continue;
                };
                let _pushed = window.a11y.nodes.push_leaf(*id, moved(node));
                #[cfg(debug_assertions)]
                if _pushed && let Some(info) = source.info.get(id) {
                    window.a11y.nodes.record_node_info(*id, info.clone());
                }
            }
            Op::Pop(at) => {
                if window.a11y.nodes.fast.replay_open.pop() != Some(true) {
                    continue;
                }
                let mut node = match source.nodes.get(at) {
                    Some((_, node)) => moved(node),
                    None => {
                        debug_assert!(false, "a node missing from last frame's tree");
                        accesskit::Node::default()
                    }
                };
                if let Some(open) = window.a11y.nodes.nodes_stack.last_mut() {
                    // Its children are this frame's: those pushed again and
                    // those of nested views built again.
                    if open.children() != node.children() {
                        if open.children().is_empty() {
                            node.clear_children();
                        } else {
                            node.set_children(open.children().to_vec());
                        }
                    }
                    *open = node;
                }
                window.a11y.nodes.pop();
            }
            Op::Focusable(node, focus) => {
                window.a11y.set_focusable(node, focus);
                if focus.is_focused(window) {
                    window.a11y.set_focus(node);
                }
            }
            Op::ActiveDescendant(node) => window.a11y.set_active_descendant(node),
        }
    }
}

/// Moves the accessibility action listeners last frame's paint of `range`
/// registered into the frame being drawn, which copies that paint.
#[inline]
pub(crate) fn reuse_paint(window: &mut Window, range: &Range<PaintIndex>) {
    let actions = range.start.fast.a11y_actions..range.end.fast.a11y_actions;
    if actions.is_empty() || !window.a11y.is_active() {
        return;
    }
    reuse_actions(window, actions);
}

#[inline(never)]
fn reuse_actions(window: &mut Window, range: Range<usize>) {
    let a11y = &mut window.a11y;
    let log = &mut a11y.nodes.fast;
    let Some(actions) = log.previous_actions.get(range) else {
        debug_assert!(
            false,
            "a stretch reaches past last frame's action listeners"
        );
        return;
    };
    for &(node, action, index) in actions {
        let listener = log
            .previous_listeners
            .get_mut(&node)
            .and_then(|listeners| listeners.get_mut(index))
            .and_then(Option::take);
        log.actions.push((node, action));
        if let Some(listener) = listener {
            a11y.action_listeners
                .entry(node)
                .or_default()
                .push(listener);
        }
    }
}

/// What building the accessibility tree did over a stretch of prepaint,
/// kept apart from the frame it was drawn in: a scroll layer's content, to
/// be done again wherever the layer is composited.
pub(crate) struct A11yStretch {
    ops: Vec<Op>,
    nodes: Vec<(NodeId, accesskit::Node)>,
    bounds: FxHashMap<NodeId, Bounds<Pixels>>,
    #[cfg(debug_assertions)]
    info: FxHashMap<NodeId, crate::window::a11y::debug::NodeDebugInfo>,
    /// Whether a synthetic node (see [`crate::Element::a11y_synthetic_children`])
    /// has bounds. Its element's code placed it, and nothing says it moves
    /// with its element: the content is painted again rather than moved.
    placed_synthetic: bool,
}

/// Keeps what building the accessibility tree did over `range` of the
/// frame being drawn's prepaint, if accessibility is active.
pub(crate) fn capture(
    window: &Window,
    range: &Range<PrepaintStateIndex>,
) -> Option<Rc<A11yStretch>> {
    if !window.a11y.is_active() {
        return None;
    }
    let builder = &window.a11y.nodes;
    let ops = builder
        .fast
        .ops
        .get(range.start.fast_a11y_index..range.end.fast_a11y_index)?;
    let mut stretch = A11yStretch {
        ops: Vec::with_capacity(ops.len()),
        nodes: Vec::new(),
        bounds: FxHashMap::default(),
        #[cfg(debug_assertions)]
        info: FxHashMap::default(),
        placed_synthetic: false,
    };
    for op in ops {
        let op = match *op {
            Op::Leaf(at) | Op::Pop(at) => {
                let node = builder.all_nodes.get(at)?.clone();
                let local = stretch.nodes.len();
                #[cfg(debug_assertions)]
                if matches!(op, Op::Leaf(_))
                    && let Some(info) = builder.node_info.get(&node.0)
                {
                    stretch.info.insert(node.0, info.clone());
                }
                stretch.placed_synthetic |= matches!(op, Op::Leaf(_)) && node.1.bounds().is_some();
                stretch.nodes.push(node);
                if matches!(op, Op::Leaf(_)) {
                    Op::Leaf(local)
                } else {
                    Op::Pop(local)
                }
            }
            Op::Push(id) => {
                if let Some(bounds) = window.a11y.node_bounds.get(&id) {
                    stretch.bounds.insert(id, *bounds);
                }
                #[cfg(debug_assertions)]
                if let Some(info) = builder.node_info.get(&id) {
                    stretch.info.insert(id, info.clone());
                }
                Op::Push(id)
            }
            other => other,
        };
        stretch.ops.push(op);
    }
    Some(Rc::new(stretch))
}

/// Does again what `stretch` kept, its nodes moved by `delta`, if
/// accessibility is active.
pub(crate) fn carry(window: &mut Window, stretch: Option<&A11yStretch>, delta: Point<Pixels>) {
    let Some(stretch) = stretch else {
        return;
    };
    if !window.a11y.is_active() {
        return;
    }
    let source = Source {
        ops: &stretch.ops,
        nodes: &stretch.nodes,
        bounds: &stretch.bounds,
        #[cfg(debug_assertions)]
        info: &stretch.info,
    };
    replay(window, &source, delta);
}

/// Whether a scroll layer whose content kept `stretch` has to be painted
/// again rather than composited: accessibility is active, and the content
/// was painted while it was not, or holds synthetic nodes placed by code
/// that may not move them with the scroll.
#[inline]
pub(crate) fn layer_needs_repaint(window: &Window, stretch: Option<&A11yStretch>) -> bool {
    window.a11y.is_active() && stretch.is_none_or(|stretch| stretch.placed_synthetic)
}

#[cfg(any(test, feature = "test-support"))]
impl Window {
    /// Makes the window build its accessibility tree from the next frame on
    /// as though assistive technology had asked for it, or stop.
    pub fn set_a11y_active_for_tests(&mut self, active: bool) {
        crate::fast::a11y::set_active_flag(&self.a11y, active);
        self.refresh();
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn set_active_flag(a11y: &A11y, active: bool) {
    a11y.active_flag
        .store(active, std::sync::atomic::Ordering::SeqCst);
}

/// The accessibility tree last sent to the platform, for tests to compare.
#[cfg(test)]
pub(crate) fn last_tree(window: &Window) -> Option<&accesskit::TreeUpdate> {
    window.a11y.debug.last_tree_update.as_ref()
}
