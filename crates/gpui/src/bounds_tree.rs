use crate::{Bounds, Half};
use std::{
    cmp,
    fmt::Debug,
    ops::{Add, Sub},
    ptr::NonNull,
};

/// Maximum children per internal node (R-tree style branching factor).
/// Higher values = shorter tree = fewer cache misses, but more work per node.
const MAX_CHILDREN: usize = 12;

/// A spatial tree optimized for finding maximum ordering among intersecting bounds.
///
/// This is an R-tree variant specifically designed for the use case of assigning
/// z-order to overlapping UI elements. Key optimizations:
/// - Tracks the leaf with global max ordering for O(1) fast-path queries
/// - Uses higher branching factor (4) for lower tree height
/// - Aggressive pruning during search based on max_order metadata
#[derive(Debug)]
pub(crate) struct BoundsTree<U>
where
    U: Clone + Debug + Default + PartialEq,
{
    /// All nodes stored contiguously for cache efficiency.
    nodes: Vec<Node<U>>,
    /// Index of the root node, if any.
    root: Option<usize>,
    /// Index of the leaf with the highest ordering (for fast-path lookups).
    max_leaf: Option<usize>,
    /// Reusable stack for tree traversal during insertion.
    insert_path: Vec<usize>,
    /// Reusable stack for search operations.
    search_stack: Vec<NonNull<Node<U>>>,
    /// The bounds inserted since the tree was last cleared, in order, each
    /// with the ordering it was given.
    recorded: Vec<(Bounds<U>, u32)>,
    /// What `recorded` held when the tree was cleared.
    previous: Vec<(Bounds<U>, u32)>,
    /// Whether every insert since the tree was cleared has matched the one in
    /// the same position in `previous`. The orderings are then already known
    /// and the tree is not built at all; it is built from `recorded` the
    /// first time an insert differs.
    replaying: bool,
}

/// A node in the bounds tree.
#[derive(Debug, Clone)]
struct Node<U>
where
    U: Clone + Debug + Default + PartialEq,
{
    /// Bounding box containing this node and all descendants.
    bounds: Bounds<U>,
    /// Maximum ordering value in this subtree.
    max_order: u32,
    /// Node-specific data.
    kind: NodeKind,
}

#[derive(Debug, Clone)]
enum NodeKind {
    /// Leaf node containing actual bounds data.
    Leaf {
        /// The ordering assigned to this bounds.
        order: u32,
    },
    /// Internal node with children.
    Internal {
        /// Indices of child nodes (2 to MAX_CHILDREN).
        children: NodeChildren,
    },
}

/// Fixed-size array for child indices, avoiding heap allocation.
#[derive(Debug, Clone)]
struct NodeChildren {
    // Keeps an invariant where the max order child is always at the end
    indices: [usize; MAX_CHILDREN],
    len: u8,
}

impl NodeChildren {
    fn new() -> Self {
        Self {
            indices: [0; MAX_CHILDREN],
            len: 0,
        }
    }

    fn push(&mut self, index: usize) {
        debug_assert!((self.len as usize) < MAX_CHILDREN);
        self.indices[self.len as usize] = index;
        self.len += 1;
    }

    fn len(&self) -> usize {
        self.len as usize
    }

    fn as_slice(&self) -> &[usize] {
        &self.indices[..self.len as usize]
    }
}

impl<U> BoundsTree<U>
where
    U: Clone
        + Debug
        + PartialEq
        + PartialOrd
        + Add<U, Output = U>
        + Sub<Output = U>
        + Half
        + Default,
{
    /// Clears all nodes from the tree.
    ///
    /// What was inserted since the last clear is kept aside: a frame usually
    /// inserts the same bounds in the same order as the one before it, and an
    /// ordering depends on nothing but the bounds inserted before it, so as
    /// long as that holds the orderings can be handed out again as they were.
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.root = None;
        self.max_leaf = None;
        self.insert_path.clear();
        self.search_stack.clear();
        std::mem::swap(&mut self.previous, &mut self.recorded);
        self.recorded.clear();
        self.replaying = true;
    }

    /// Inserts bounds into the tree and returns its assigned ordering.
    ///
    /// The ordering is one greater than the maximum ordering of any
    /// existing bounds that intersect with the new bounds.
    pub fn insert(&mut self, new_bounds: Bounds<U>) -> u32 {
        if self.replaying {
            let position = self.recorded.len();
            if let Some((previous, ordering)) = self.previous.get(position)
                && *previous == new_bounds
            {
                let ordering = *ordering;
                self.recorded.push((new_bounds, ordering));
                return ordering;
            }
            self.replaying = false;
            self.build_from_recorded();
        }

        // Find maximum ordering among intersecting bounds
        let max_intersecting = self.find_max_ordering(&new_bounds);
        let ordering = max_intersecting + 1;

        // Insert the new leaf
        let new_leaf_idx = self.insert_leaf(new_bounds.clone(), ordering);

        self.track_max_leaf(new_leaf_idx, ordering);
        self.recorded.push((new_bounds, ordering));
        ordering
    }

    /// Remembers `leaf` as the one with the highest ordering if it is.
    fn track_max_leaf(&mut self, leaf_idx: usize, ordering: u32) {
        self.max_leaf = match self.max_leaf {
            None => Some(leaf_idx),
            Some(old_idx) if self.nodes[old_idx].max_order < ordering => Some(leaf_idx),
            some => some,
        };
    }

    /// Builds the tree from what has been inserted since it was cleared, whose
    /// orderings were handed out without it. Their orderings are known, so
    /// there is nothing to search for, only leaves to place.
    fn build_from_recorded(&mut self) {
        for position in 0..self.recorded.len() {
            let (bounds, ordering) = self.recorded[position].clone();
            let leaf_idx = self.insert_leaf(bounds, ordering);
            self.track_max_leaf(leaf_idx, ordering);
        }
    }

    /// Finds the maximum ordering among all bounds that intersect with the query.
    fn find_max_ordering(&mut self, query: &Bounds<U>) -> u32 {
        let Some(root_idx) = self.root else {
            return 0;
        };

        // Fast path: check if the max-ordering leaf intersects
        if let Some(max_idx) = self.max_leaf {
            let max_node = &self.nodes[max_idx];
            if query.intersects(&max_node.bounds) {
                return max_node.max_order;
            }
        }

        // Slow path: search the tree
        self.search_stack.clear();
        self.search_stack.push(NonNull::from(&self.nodes[root_idx]));

        let mut max_found = 0u32;

        while let Some(node) = self.search_stack.pop() {
            // SAFETY: `node` is guaranteed to be valid as the `nodes` stack is unmodified in this function
            // and the `search_stack` only contains pointers from this function call.
            let node = unsafe { node.as_ref() };

            // Pruning: skip if this subtree can't improve our result
            if node.max_order <= max_found {
                continue;
            }

            // Spatial pruning: skip if bounds don't intersect
            if !query.intersects(&node.bounds) {
                continue;
            }

            match &node.kind {
                NodeKind::Leaf { order } => {
                    max_found = cmp::max(max_found, *order);
                }
                NodeKind::Internal { children } => {
                    // Children are maintained with highest max_order at the end.
                    // Push in forward order to highest (last) is popped first.
                    self.search_stack.extend(
                        children
                            .as_slice()
                            .iter()
                            .map(|&child_idx| &self.nodes[child_idx])
                            .filter(|node| node.max_order > max_found)
                            .map(NonNull::from),
                    );
                }
            }
        }

        max_found
    }

    /// Inserts a leaf node with the given bounds and ordering.
    /// Returns the index of the new leaf.
    ///
    /// The tree is kept balanced the way an R-tree is: every leaf sits at the
    /// same depth, a node given more than [`MAX_CHILDREN`] children splits in
    /// two, and a split that reaches the root grows the tree by a level.
    /// Bounds arrive in painting order, which is spatially coherent — row after
    /// row, cell after cell — so a tree that never split nested each insert
    /// near the last one a level deeper than it, and every search and insert
    /// after it paid for the depth.
    fn insert_leaf(&mut self, bounds: Bounds<U>, order: u32) -> usize {
        let new_leaf_idx = self.nodes.len();
        self.nodes.push(Node {
            bounds: bounds.clone(),
            max_order: order,
            kind: NodeKind::Leaf { order },
        });

        let Some(root_idx) = self.root else {
            // Tree is empty, new leaf becomes root
            self.root = Some(new_leaf_idx);
            return new_leaf_idx;
        };

        // If root is a leaf, create internal node with both
        if matches!(self.nodes[root_idx].kind, NodeKind::Leaf { .. }) {
            let new_root_idx = self.push_internal(&[root_idx, new_leaf_idx]);
            self.root = Some(new_root_idx);
            return new_leaf_idx;
        }

        // Descend to the internal node whose children are leaves.
        self.insert_path.clear();
        let mut current_idx = root_idx;
        loop {
            self.insert_path.push(current_idx);
            let NodeKind::Internal { children } = &self.nodes[current_idx].kind else {
                unreachable!("Should only traverse internal nodes");
            };
            let children = children.as_slice();
            if matches!(self.nodes[children[0]].kind, NodeKind::Leaf { .. }) {
                break;
            }
            current_idx = self.choose_subtree(children, &bounds);
        }

        // Add the leaf at the bottom and work back up, splitting what
        // overflows and handing the new half to the level above.
        let mut pending = Some(new_leaf_idx);
        for level in (0..self.insert_path.len()).rev() {
            let node_idx = self.insert_path[level];
            let node = &mut self.nodes[node_idx];
            node.bounds = node.bounds.union(&bounds);
            node.max_order = cmp::max(node.max_order, order);

            if let Some(child_idx) = pending.take() {
                let NodeKind::Internal { children } = &mut node.kind else {
                    unreachable!("Should only traverse internal nodes");
                };
                if children.len() < MAX_CHILDREN {
                    children.push(child_idx);
                } else {
                    pending = Some(self.split(node_idx, child_idx));
                }
            }
            self.place_max_last(node_idx);
        }

        if let Some(sibling_idx) = pending {
            let new_root_idx = self.push_internal(&[root_idx, sibling_idx]);
            self.root = Some(new_root_idx);
        }

        new_leaf_idx
    }

    /// The child that has to grow least to take `bounds`, or of two that grow
    /// alike, the smaller.
    fn choose_subtree(&self, children: &[usize], bounds: &Bounds<U>) -> usize {
        let mut best = None::<(usize, U, U)>;
        for &child_idx in children {
            let child_bounds = &self.nodes[child_idx].bounds;
            let size = child_bounds.half_perimeter();
            let growth = bounds.union(child_bounds).half_perimeter() - size.clone();
            let better = match &best {
                None => true,
                Some((_, best_growth, best_size)) => {
                    growth < *best_growth || (growth == *best_growth && size < *best_size)
                }
            };
            if better {
                best = Some((child_idx, growth, size));
            }
        }
        best.expect("an internal node has children").0
    }

    /// Splits the children of a full node, and one more, between it and a new
    /// node, which is returned for the level above to take. They are divided
    /// by where their centers fall along whichever axis the node spans
    /// further, which keeps each half compact.
    fn split(&mut self, node_idx: usize, extra_idx: usize) -> usize {
        let mut entries = [0usize; MAX_CHILDREN + 1];
        let NodeKind::Internal { children } = &self.nodes[node_idx].kind else {
            unreachable!("Only internal nodes are split");
        };
        entries[..MAX_CHILDREN].copy_from_slice(children.as_slice());
        entries[MAX_CHILDREN] = extra_idx;

        let spread = &self.nodes[node_idx].bounds.size;
        let horizontal = spread.width > spread.height;
        let nodes = &self.nodes;
        let center = |idx: usize| {
            let bounds = &nodes[idx].bounds;
            if horizontal {
                bounds.origin.x.clone() + bounds.size.width.half()
            } else {
                bounds.origin.y.clone() + bounds.size.height.half()
            }
        };
        entries.sort_by(|a, b| {
            center(*a)
                .partial_cmp(&center(*b))
                .unwrap_or(cmp::Ordering::Equal)
        });

        let (kept, moved) = entries.split_at(entries.len() / 2);
        self.set_children(node_idx, kept);
        self.push_internal(moved)
    }

    /// Adds an internal node over `children` and returns its index.
    fn push_internal(&mut self, children: &[usize]) -> usize {
        let node_idx = self.nodes.len();
        self.nodes.push(Node {
            bounds: self.nodes[children[0]].bounds.clone(),
            max_order: 0,
            kind: NodeKind::Internal {
                children: NodeChildren::new(),
            },
        });
        self.set_children(node_idx, children);
        node_idx
    }

    /// Gives an internal node `children`, and the bounds and ordering that
    /// cover them.
    fn set_children(&mut self, node_idx: usize, children: &[usize]) {
        let mut bounds = self.nodes[children[0]].bounds.clone();
        let mut max_order = 0;
        let mut node_children = NodeChildren::new();
        for &child_idx in children {
            let child = &self.nodes[child_idx];
            bounds = bounds.union(&child.bounds);
            max_order = cmp::max(max_order, child.max_order);
            node_children.push(child_idx);
        }
        let node = &mut self.nodes[node_idx];
        node.bounds = bounds;
        node.max_order = max_order;
        node.kind = NodeKind::Internal {
            children: node_children,
        };
        self.place_max_last(node_idx);
    }

    /// Moves the child with the highest ordering to the end, where a search
    /// visits it first.
    fn place_max_last(&mut self, node_idx: usize) {
        let NodeKind::Internal { children } = &self.nodes[node_idx].kind else {
            return;
        };
        let children = children.as_slice();
        let max_order = self.nodes[node_idx].max_order;
        if children
            .last()
            .is_some_and(|&last| self.nodes[last].max_order == max_order)
        {
            return;
        }
        let Some(max_pos) =
            (0..children.len()).max_by_key(|&pos| self.nodes[children[pos]].max_order)
        else {
            return;
        };
        let last = children.len() - 1;
        if max_pos != last
            && let NodeKind::Internal { children } = &mut self.nodes[node_idx].kind
        {
            children.indices.swap(max_pos, last);
        }
    }
}

impl<U> Default for BoundsTree<U>
where
    U: Clone + Debug + Default + PartialEq,
{
    fn default() -> Self {
        BoundsTree {
            nodes: Vec::new(),
            root: None,
            max_leaf: None,
            insert_path: Vec::new(),
            search_stack: Vec::new(),
            recorded: Vec::new(),
            previous: Vec::new(),
            replaying: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Bounds, Point, Size};
    use rand::{Rng, SeedableRng};

    #[test]
    fn test_insert() {
        let mut tree = BoundsTree::<f32>::default();
        let bounds1 = Bounds {
            origin: Point { x: 0.0, y: 0.0 },
            size: Size {
                width: 10.0,
                height: 10.0,
            },
        };
        let bounds2 = Bounds {
            origin: Point { x: 5.0, y: 5.0 },
            size: Size {
                width: 10.0,
                height: 10.0,
            },
        };
        let bounds3 = Bounds {
            origin: Point { x: 10.0, y: 10.0 },
            size: Size {
                width: 10.0,
                height: 10.0,
            },
        };

        // Insert the bounds into the tree and verify the order is correct
        assert_eq!(tree.insert(bounds1), 1);
        assert_eq!(tree.insert(bounds2), 2);
        assert_eq!(tree.insert(bounds3), 3);

        // Insert non-overlapping bounds and verify they can reuse orders
        let bounds4 = Bounds {
            origin: Point { x: 20.0, y: 20.0 },
            size: Size {
                width: 10.0,
                height: 10.0,
            },
        };
        let bounds5 = Bounds {
            origin: Point { x: 40.0, y: 40.0 },
            size: Size {
                width: 10.0,
                height: 10.0,
            },
        };
        let bounds6 = Bounds {
            origin: Point { x: 25.0, y: 25.0 },
            size: Size {
                width: 10.0,
                height: 10.0,
            },
        };
        assert_eq!(tree.insert(bounds4), 1); // bounds4 does not overlap with bounds1, bounds2, or bounds3
        assert_eq!(tree.insert(bounds5), 1); // bounds5 does not overlap with any other bounds
        assert_eq!(tree.insert(bounds6), 2); // bounds6 overlaps with bounds4, so it should have a different order
    }

    #[test]
    fn test_random_iterations() {
        let max_bounds = 100;
        for seed in 1..=1000 {
            // let seed = 44;
            let mut tree = BoundsTree::default();
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed as u64);
            let mut expected_quads: Vec<(Bounds<f32>, u32)> = Vec::new();

            // Insert a random number of random AABBs into the tree.
            let num_bounds = rng.random_range(1..=max_bounds);
            for _ in 0..num_bounds {
                let min_x: f32 = rng.random_range(-100.0..100.0);
                let min_y: f32 = rng.random_range(-100.0..100.0);
                let width: f32 = rng.random_range(0.0..50.0);
                let height: f32 = rng.random_range(0.0..50.0);
                let bounds = Bounds {
                    origin: Point { x: min_x, y: min_y },
                    size: Size { width, height },
                };

                let expected_ordering = expected_quads
                    .iter()
                    .filter_map(|quad| quad.0.intersects(&bounds).then_some(quad.1))
                    .max()
                    .unwrap_or(0)
                    + 1;
                expected_quads.push((bounds, expected_ordering));

                // Insert the AABB into the tree and collect intersections.
                let actual_ordering = tree.insert(bounds);
                assert_eq!(actual_ordering, expected_ordering);
            }
        }
    }

    /// A tree hands out last fill's orderings again while the bounds come in
    /// as they did then, and builds itself the moment they stop. Each frame
    /// here follows the one before for a while and then goes its own way, or
    /// repeats it, or differs from the start, and every ordering is checked
    /// against every bounds inserted before it in that frame.
    #[test]
    fn replaying_the_last_fill_gives_what_inserting_it_would() {
        fn random_bounds(rng: &mut rand::rngs::StdRng) -> Bounds<f32> {
            Bounds {
                origin: Point {
                    x: rng.random_range(-100.0..100.0),
                    y: rng.random_range(-100.0..100.0),
                },
                size: Size {
                    width: rng.random_range(0.0..50.0),
                    height: rng.random_range(0.0..50.0),
                },
            }
        }
        fn fill(tree: &mut BoundsTree<f32>, frame: &[Bounds<f32>]) {
            tree.clear();
            let mut inserted: Vec<(Bounds<f32>, u32)> = Vec::new();
            for bounds in frame {
                let expected = inserted
                    .iter()
                    .filter_map(|(other, order)| other.intersects(bounds).then_some(*order))
                    .max()
                    .unwrap_or(0)
                    + 1;
                assert_eq!(tree.insert(*bounds), expected);
                inserted.push((*bounds, expected));
            }
        }

        for seed in 1..=300 {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let mut tree = BoundsTree::default();
            let count = rng.random_range(1..=120);
            let first: Vec<_> = (0..count).map(|_| random_bounds(&mut rng)).collect();
            fill(&mut tree, &first);

            // Follows the last fill for a while, then diverges.
            let kept = rng.random_range(0..=count);
            let mut second: Vec<_> = first[..kept].to_vec();
            second.extend((0..rng.random_range(0..=60)).map(|_| random_bounds(&mut rng)));
            fill(&mut tree, &second);

            // Repeats it exactly.
            fill(&mut tree, &second);

            // Differs from the first bounds on.
            let third: Vec<_> = (0..count).map(|_| random_bounds(&mut rng)).collect();
            fill(&mut tree, &third);
        }
    }

    /// The random cases above stay small enough that few nodes ever split
    /// more than once. These are large enough for splits to reach several
    /// levels up, and still checked against every bounds inserted before.
    #[test]
    fn test_random_iterations_deep_enough_to_split_every_level() {
        for seed in 1..=10 {
            let mut tree = BoundsTree::default();
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed as u64);
            let mut expected_quads: Vec<(Bounds<f32>, u32)> = Vec::new();
            for _ in 0..2000 {
                let bounds = Bounds {
                    origin: Point {
                        x: rng.random_range(-1000.0..1000.0),
                        y: rng.random_range(-1000.0..1000.0),
                    },
                    size: Size {
                        width: rng.random_range(0.0..80.0),
                        height: rng.random_range(0.0..80.0),
                    },
                };
                let expected_ordering = expected_quads
                    .iter()
                    .filter_map(|quad| quad.0.intersects(&bounds).then_some(quad.1))
                    .max()
                    .unwrap_or(0)
                    + 1;
                expected_quads.push((bounds, expected_ordering));
                assert_eq!(tree.insert(bounds), expected_ordering);
            }
        }
    }

    /// Bounds arrive in painting order, one row after another and one cell
    /// after another within it. Nesting each insert near the last one a
    /// level deeper turned that into a tree as deep as it was long in places;
    /// splitting keeps every leaf a few levels from the root.
    #[test]
    fn painting_order_keeps_the_tree_shallow() {
        fn depth(tree: &BoundsTree<f32>, idx: usize) -> usize {
            match &tree.nodes[idx].kind {
                NodeKind::Leaf { .. } => 1,
                NodeKind::Internal { children } => {
                    1 + children
                        .as_slice()
                        .iter()
                        .map(|&child| depth(tree, child))
                        .max()
                        .unwrap_or(0)
                }
            }
        }

        let mut tree = BoundsTree::<f32>::default();
        let mut leaves = 0;
        for row in 0..400 {
            let y = row as f32 * 24.;
            let row_bounds = Bounds {
                origin: Point { x: 0., y },
                size: Size {
                    width: 450.,
                    height: 24.,
                },
            };
            tree.insert(row_bounds);
            leaves += 1;
            for cell in 0..7 {
                tree.insert(Bounds {
                    origin: Point {
                        x: 8. + cell as f32 * 60.,
                        y: y + 3.,
                    },
                    size: Size {
                        width: 50.,
                        height: 18.,
                    },
                });
                leaves += 1;
            }
        }

        // Every node but the root holds at least half of MAX_CHILDREN, so a
        // balanced tree of this many leaves is at most this deep.
        let bound = (leaves as f32).log((MAX_CHILDREN / 2) as f32).ceil() as usize + 1;
        let depth = depth(&tree, tree.root.unwrap());
        assert!(
            depth <= bound,
            "{leaves} bounds inserted in painting order made a tree {depth} deep, \
             where a balanced one is at most {bound}"
        );
    }
}
