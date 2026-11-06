//! gpui-fast's tab stop map, which `gpui.rs` puts in place of upstream's
//! `tab_stop.rs`. It answers every query upstream's does, with the same
//! results, but builds its order only when one is asked.
//!
//! Painting inserts every focusable element into the next frame's map, and a
//! reused view or a scroll layer replays all of the insertions it recorded,
//! while only Tab navigation (and, with accessibility active, the tab stop
//! count) queries it. Upstream kept a `SumTree` and a hash map up to date on
//! every insertion, so a frame of focusable content paid for an ordered
//! insert per element whether or not anything asked. Here insertions only
//! append to the history upstream already keeps, and the order and the
//! focus-id lookup are built from it on the first query after a change.

use std::{cell::RefCell, fmt::Debug};

use collections::FxHashMap;

use crate::{FocusHandle, FocusId};

/// Represents a collection of focus handles using the tab-index APIs.
///
/// Only the insertion history is kept as elements are painted; the tab order
/// (by group path, then insertion) and the focus-id lookup are built from it
/// on the first query after a change.
#[derive(Debug, Default)]
pub(crate) struct TabStopMap {
    pub(crate) insertion_history: Vec<TabStopOperation>,
    /// The order and lookup for `insertion_history`, once queried.
    index: RefCell<Option<TabStopIndex>>,
}

/// The queryable form of a [`TabStopMap`]'s insertion history.
#[derive(Debug, Default)]
struct TabStopIndex {
    by_id: FxHashMap<FocusId, TabStopNode>,
    /// Every inserted node, in tab order.
    order: Vec<TabStopNode>,
}

#[derive(Debug, Clone)]
pub enum TabStopOperation {
    Insert(FocusHandle),
    Group(TabIndex),
    GroupEnd,
}

impl TabStopOperation {
    fn focus_handle(&self) -> Option<&FocusHandle> {
        match self {
            TabStopOperation::Insert(focus_handle) => Some(focus_handle),
            _ => None,
        }
    }
}

type TabIndex = isize;

#[derive(Debug, Default, PartialEq, Eq, Clone, Ord, PartialOrd)]
struct TabStopPath(smallvec::SmallVec<[TabIndex; 6]>);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TabStopNode {
    /// Path to access the node in the tree
    /// The final node in the list is a leaf node corresponding to an actual focus handle,
    /// all other nodes are group nodes
    path: TabStopPath,
    /// index into the backing array of nodes. Corresponds to insertion order
    node_insertion_index: usize,

    /// Whether this node is a tab stop
    tab_stop: bool,
}

impl Ord for TabStopNode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.path
            .cmp(&other.path)
            .then(self.node_insertion_index.cmp(&other.node_insertion_index))
    }
}

impl PartialOrd for TabStopNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl TabStopIndex {
    /// Replays `history` the way upstream's eager insertions built the map:
    /// the last insertion of a focus id wins its lookup, while every
    /// insertion keeps its place in the order.
    fn build(history: &[TabStopOperation]) -> Self {
        let mut path = TabStopPath::default();
        let mut index = Self::default();
        for (node_insertion_index, operation) in history.iter().enumerate() {
            match operation {
                TabStopOperation::Insert(focus_handle) => {
                    let mut node_path = path.clone();
                    node_path.0.push(focus_handle.tab_index);
                    let node = TabStopNode {
                        path: node_path,
                        node_insertion_index,
                        tab_stop: focus_handle.tab_stop,
                    };
                    index.by_id.insert(focus_handle.id, node.clone());
                    index.order.push(node);
                }
                TabStopOperation::Group(tab_index) => path.0.push(*tab_index),
                TabStopOperation::GroupEnd => {
                    path.0.pop();
                }
            }
        }
        index.order.sort_unstable();
        index
    }

    /// The position of `node`, which is in the order.
    fn position(&self, node: &TabStopNode) -> Option<usize> {
        self.order.binary_search(node).ok()
    }

    fn next_after(&self, node: &TabStopNode) -> Option<&TabStopNode> {
        let start = self.position(node)? + 1;
        self.order[start..].iter().find(|item| item.tab_stop)
    }

    fn prev_before(&self, node: &TabStopNode) -> Option<&TabStopNode> {
        let end = self.position(node)?;
        self.order[..end].iter().rev().find(|item| item.tab_stop)
    }
}

impl TabStopMap {
    pub fn insert(&mut self, focus_handle: &FocusHandle) {
        self.insertion_history
            .push(TabStopOperation::Insert(focus_handle.clone()));
        self.index.get_mut().take();
    }

    pub fn begin_group(&mut self, tab_index: isize) {
        self.insertion_history
            .push(TabStopOperation::Group(tab_index));
        self.index.get_mut().take();
    }

    pub fn end_group(&mut self) {
        self.insertion_history.push(TabStopOperation::GroupEnd);
        self.index.get_mut().take();
    }

    pub fn clear(&mut self) {
        self.insertion_history.clear();
        self.index.get_mut().take();
    }

    /// Runs `f` with the index of the current insertion history.
    fn with_index<R>(&self, f: impl FnOnce(&TabStopIndex) -> R) -> R {
        let mut index = self.index.borrow_mut();
        f(index.get_or_insert_with(|| TabStopIndex::build(&self.insertion_history)))
    }

    pub fn next(&self, focused_id: Option<&FocusId>) -> Option<FocusHandle> {
        self.with_index(|index| {
            let first_stop = || {
                let first = index.order.first()?;
                if first.tab_stop {
                    Some(first)
                } else {
                    index.next_after(first)
                }
            };
            let node = focused_id
                .and_then(|id| index.by_id.get(id))
                .and_then(|node| index.next_after(node))
                .or_else(first_stop)?;
            self.focus_handle_for_order(node)
        })
    }

    pub fn prev(&self, focused_id: Option<&FocusId>) -> Option<FocusHandle> {
        self.with_index(|index| {
            let last_stop = || {
                let last = index.order.last()?;
                if last.tab_stop {
                    Some(last)
                } else {
                    index.prev_before(last)
                }
            };
            let node = focused_id
                .and_then(|id| index.by_id.get(id))
                .and_then(|node| index.prev_before(node))
                .or_else(last_stop)?;
            self.focus_handle_for_order(node)
        })
    }

    pub fn replay(&mut self, nodes: &[TabStopOperation]) {
        if nodes.is_empty() {
            return;
        }
        self.insertion_history.extend_from_slice(nodes);
        self.index.get_mut().take();
    }

    pub fn paint_index(&self) -> usize {
        self.insertion_history.len()
    }

    pub(crate) fn tab_stop_count(&self) -> usize {
        self.with_index(|index| index.by_id.values().filter(|node| node.tab_stop).count())
    }

    fn focus_handle_for_order(&self, order: &TabStopNode) -> Option<FocusHandle> {
        let handle = self.insertion_history[order.node_insertion_index].focus_handle();
        debug_assert!(
            handle.is_some(),
            "The order node did not correspond to an element, this is a GPUI bug"
        );
        handle.cloned()
    }
}

#[cfg(test)]
mod tests {
    use itertools::Itertools as _;
    use parking_lot::RwLock;

    use crate::{FocusHandle, FocusId, FocusMap, TabStopMap};
    use std::sync::Arc;

    /// Painting only records: what a frame replays must not build the index.
    #[test]
    fn inserting_builds_no_index_until_a_query() {
        let focus_map = Arc::new(RwLock::new(FocusMap::default()));
        let mut tab = TabStopMap::default();
        let handle = FocusHandle::new(&focus_map).tab_stop(true);
        tab.begin_group(0);
        tab.insert(&handle);
        tab.end_group();
        assert!(tab.index.borrow().is_none());
        assert_eq!(tab.tab_stop_count(), 1);
        assert!(tab.index.borrow().is_some());
        tab.insert(&handle);
        assert!(tab.index.borrow().is_none(), "a change drops the index");
    }

    /// Replaying a frame's history gives the order inserting it did, and a
    /// handle inserted twice is found at its last insertion, as upstream's
    /// eagerly kept lookup found it.
    #[test]
    fn replay_and_repeated_handles_match_inserting() {
        let focus_map = Arc::new(RwLock::new(FocusMap::default()));
        let a = FocusHandle::new(&focus_map).tab_stop(true);
        let b = FocusHandle::new(&focus_map).tab_stop(true);
        let c = FocusHandle::new(&focus_map).tab_stop(true);
        let mut painted = TabStopMap::default();
        painted.insert(&a);
        painted.begin_group(0);
        painted.insert(&b);
        painted.end_group();
        painted.insert(&c);
        painted.insert(&a);
        // Tab order: a (first insertion), c, a (last insertion), then the
        // group holding b.

        let mut replayed = TabStopMap::default();
        replayed.replay(&painted.insertion_history);
        for map in [&painted, &replayed] {
            assert_eq!(map.tab_stop_count(), 3);
            assert_eq!(map.next(None), Some(a.clone()));
            assert_eq!(
                map.next(Some(&a.id)),
                Some(b.clone()),
                "a is found at its last insertion"
            );
            assert_eq!(map.prev(Some(&a.id)), Some(c.clone()));
            assert_eq!(map.next(Some(&b.id)), Some(a.clone()), "b is last");
            assert_eq!(map.prev(Some(&c.id)), Some(a.clone()));
        }
    }

    #[test]
    fn test_tab_handles() {
        let focus_map = Arc::new(RwLock::new(FocusMap::default()));
        let mut tab_index_map = TabStopMap::default();

        let focus_handles = [
            FocusHandle::new(&focus_map).tab_stop(true).tab_index(0),
            FocusHandle::new(&focus_map).tab_stop(true).tab_index(1),
            FocusHandle::new(&focus_map).tab_stop(true).tab_index(1),
            FocusHandle::new(&focus_map),
            FocusHandle::new(&focus_map).tab_index(2),
            FocusHandle::new(&focus_map).tab_stop(true).tab_index(0),
            FocusHandle::new(&focus_map).tab_stop(true).tab_index(2),
        ];

        for handle in focus_handles.iter() {
            tab_index_map.insert(handle);
        }
        let expected = [
            focus_handles[0].clone(),
            focus_handles[5].clone(),
            focus_handles[1].clone(),
            focus_handles[2].clone(),
            focus_handles[6].clone(),
        ];

        let mut prev = None;
        let mut found = vec![];
        for _ in 0..expected.len() {
            let handle = tab_index_map.next(prev.as_ref()).unwrap();
            prev = Some(handle.id);
            found.push(handle.id);
        }

        assert_eq!(
            found,
            expected.iter().map(|handle| handle.id).collect::<Vec<_>>()
        );

        // Select first tab index if no handle is currently focused.
        assert_eq!(tab_index_map.next(None), Some(expected[0].clone()));
        // Select last tab index if no handle is currently focused.
        assert_eq!(tab_index_map.prev(None), expected.last().cloned(),);

        assert_eq!(
            tab_index_map.next(Some(&expected[0].id)),
            Some(expected[1].clone())
        );
        assert_eq!(
            tab_index_map.next(Some(&expected[1].id)),
            Some(expected[2].clone())
        );
        assert_eq!(
            tab_index_map.next(Some(&expected[2].id)),
            Some(expected[3].clone())
        );
        assert_eq!(
            tab_index_map.next(Some(&expected[3].id)),
            Some(expected[4].clone())
        );
        assert_eq!(
            tab_index_map.next(Some(&expected[4].id)),
            Some(expected[0].clone())
        );

        // prev
        assert_eq!(tab_index_map.prev(None), Some(expected[4].clone()));
        assert_eq!(
            tab_index_map.prev(Some(&expected[0].id)),
            Some(expected[4].clone())
        );
        assert_eq!(
            tab_index_map.prev(Some(&expected[1].id)),
            Some(expected[0].clone())
        );
        assert_eq!(
            tab_index_map.prev(Some(&expected[2].id)),
            Some(expected[1].clone())
        );
        assert_eq!(
            tab_index_map.prev(Some(&expected[3].id)),
            Some(expected[2].clone())
        );
        assert_eq!(
            tab_index_map.prev(Some(&expected[4].id)),
            Some(expected[3].clone())
        );
    }

    #[test]
    fn test_tab_non_stop_filtering() {
        let focus_map = Arc::new(RwLock::new(FocusMap::default()));
        let mut tab_index_map = TabStopMap::default();

        // Check that we can query next from a non-stop tab
        let tab_non_stop_1 = FocusHandle::new(&focus_map).tab_stop(false).tab_index(1);
        let tab_stop_2 = FocusHandle::new(&focus_map).tab_stop(true).tab_index(2);
        tab_index_map.insert(&tab_non_stop_1);
        tab_index_map.insert(&tab_stop_2);
        let result = tab_index_map.next(Some(&tab_non_stop_1.id)).unwrap();
        assert_eq!(result.id, tab_stop_2.id);

        // Check that we skip over non-stop tabs
        let tab_stop_0 = FocusHandle::new(&focus_map).tab_stop(true).tab_index(0);
        let tab_non_stop_0 = FocusHandle::new(&focus_map).tab_stop(false).tab_index(0);
        tab_index_map.insert(&tab_stop_0);
        tab_index_map.insert(&tab_non_stop_0);
        let result = tab_index_map.next(Some(&tab_stop_0.id)).unwrap();
        assert_eq!(result.id, tab_stop_2.id);
    }

    #[must_use]
    struct TabStopMapTest {
        tab_map: TabStopMap,
        focus_map: Arc<RwLock<FocusMap>>,
        expected: Vec<(usize, FocusId)>,
    }

    impl TabStopMapTest {
        #[must_use]
        fn new() -> Self {
            Self {
                tab_map: TabStopMap::default(),
                focus_map: Arc::new(RwLock::new(FocusMap::default())),
                expected: Vec::default(),
            }
        }

        #[must_use]
        fn tab_non_stop(mut self, index: isize) -> Self {
            let handle = FocusHandle::new(&self.focus_map)
                .tab_stop(false)
                .tab_index(index);
            self.tab_map.insert(&handle);
            self
        }

        #[must_use]
        fn tab_stop(mut self, index: isize, expected: usize) -> Self {
            let handle = FocusHandle::new(&self.focus_map)
                .tab_stop(true)
                .tab_index(index);
            self.tab_map.insert(&handle);
            self.expected.push((expected, handle.id));
            self.expected.sort_by_key(|(expected, _)| *expected);
            self
        }

        #[must_use]
        fn tab_group(mut self, tab_index: isize, children: impl FnOnce(Self) -> Self) -> Self {
            self.tab_map.begin_group(tab_index);
            self = children(self);
            self.tab_map.end_group();
            self
        }

        fn traverse_tab_map(
            &self,
            traverse: impl Fn(&TabStopMap, Option<&FocusId>) -> Option<FocusHandle>,
        ) -> Vec<FocusId> {
            let mut last_focus_id = None;
            let mut found = vec![];
            for _ in 0..self.expected.len() {
                let handle = traverse(&self.tab_map, last_focus_id.as_ref()).unwrap();
                last_focus_id = Some(handle.id);
                found.push(handle.id);
            }
            found
        }

        fn assert(self) {
            let mut expected = self.expected.iter().map(|(_, id)| *id).collect_vec();

            // Check next order
            let forward_found = self.traverse_tab_map(|tab_map, prev| tab_map.next(prev));
            assert_eq!(forward_found, expected);

            // Test overflow. Last to first
            assert_eq!(
                self.tab_map
                    .next(forward_found.last())
                    .map(|handle| handle.id),
                expected.first().cloned()
            );

            // Check previous order
            let reversed_found = self.traverse_tab_map(|tab_map, prev| tab_map.prev(prev));
            expected.reverse();
            assert_eq!(reversed_found, expected);

            // Test overflow. First to last
            assert_eq!(
                self.tab_map
                    .prev(reversed_found.last())
                    .map(|handle| handle.id),
                expected.first().cloned(),
            );
        }
    }

    #[test]
    fn test_with_disabled_tab_stop() {
        TabStopMapTest::new()
            .tab_stop(0, 0)
            .tab_non_stop(1)
            .tab_stop(2, 1)
            .tab_stop(3, 2)
            .assert();
    }

    #[test]
    fn test_with_multiple_disabled_tab_stops() {
        TabStopMapTest::new()
            .tab_non_stop(0)
            .tab_stop(1, 0)
            .tab_non_stop(3)
            .tab_stop(3, 1)
            .tab_non_stop(4)
            .assert();
    }

    #[test]
    fn test_tab_group_functionality() {
        TabStopMapTest::new()
            .tab_stop(0, 0)
            .tab_stop(0, 1)
            .tab_group(2, |t| t.tab_stop(0, 2).tab_stop(1, 3))
            .tab_stop(3, 4)
            .tab_stop(4, 5)
            .assert()
    }

    #[test]
    fn test_sibling_groups() {
        TabStopMapTest::new()
            .tab_stop(0, 0)
            .tab_stop(1, 1)
            .tab_group(2, |test| test.tab_stop(0, 2).tab_stop(1, 3))
            .tab_stop(3, 4)
            .tab_stop(4, 5)
            .tab_group(6, |test| test.tab_stop(0, 6).tab_stop(1, 7))
            .tab_stop(7, 8)
            .tab_stop(8, 9)
            .assert();
    }

    #[test]
    fn test_nested_group() {
        TabStopMapTest::new()
            .tab_stop(0, 0)
            .tab_stop(1, 1)
            .tab_group(2, |t| {
                t.tab_group(0, |t| t.tab_stop(0, 2).tab_stop(1, 3))
                    .tab_stop(1, 4)
            })
            .tab_stop(3, 5)
            .tab_stop(4, 6)
            .assert();
    }

    #[test]
    fn test_sibling_nested_groups() {
        TabStopMapTest::new()
            .tab_stop(0, 0)
            .tab_stop(1, 1)
            .tab_group(2, |builder| {
                builder
                    .tab_stop(0, 2)
                    .tab_stop(2, 5)
                    .tab_group(1, |builder| builder.tab_stop(0, 3).tab_stop(1, 4))
                    .tab_group(3, |builder| builder.tab_stop(0, 6).tab_stop(1, 7))
            })
            .tab_stop(3, 8)
            .tab_stop(4, 9)
            .assert();
    }

    #[test]
    fn test_sibling_nested_groups_out_of_order() {
        TabStopMapTest::new()
            .tab_stop(9, 9)
            .tab_stop(8, 8)
            .tab_group(7, |builder| {
                builder
                    .tab_stop(0, 2)
                    .tab_stop(2, 5)
                    .tab_group(3, |builder| builder.tab_stop(1, 7).tab_stop(0, 6))
                    .tab_group(1, |builder| builder.tab_stop(0, 3).tab_stop(1, 4))
            })
            .tab_stop(3, 0)
            .tab_stop(4, 1)
            .assert();
    }
}
