# Architecture

This document explains how gpui-fast makes GPUI draw a frame from the last
one, and why it is built the way it is. [`retained-mode.md`](retained-mode.md)
is the reference for what an application sees and how retention is verified
and measured; [`upstream-sync.md`](upstream-sync.md) covers how the fork stays
mergeable. This document covers the reasoning behind both.

## The problem

GPUI draws in immediate mode. For every frame it renders every view, builds a
new element tree, lays out a new Taffy tree, shapes the text, and paints the
scene again, even when one number in one table cell changed. The cost of a
frame is the cost of the whole window, not the cost of what changed.

gpui-fast makes a frame cost what changed. When nothing a view depends on has
changed, the view is not rendered, not laid out, not prepainted and not
painted; its output is taken from the last frame.

## Constraints

Three constraints shape everything below. They were chosen deliberately, and
most of the design follows from them.

1. **The frame drawn from scratch is the specification.** A retained frame is
   correct if and only if it is the frame upstream GPUI would have drawn from
   scratch in the same state. There is no second notion of correctness to
   reason about, and it can be tested mechanically (see
   [Verification](#verification)).
2. **The public API stays upstream's.** Code written for upstream GPUI
   compiles and runs unchanged. Nothing opts in, and nothing new has to be
   learned beyond one rule that cached views already impose upstream (see
   [What an application must do](#what-an-application-must-do)).
3. **Upstream's code stays upstream's.** gpui-fast's logic lives in `fast/`
   directories. Upstream files get only small hooks into it, checked by
   `script/check-upstream`, so Zed's changes keep merging in and each piece
   can be proposed upstream on its own.

## GPUI's frame, and the mechanism gpui-fast builds on

A GPUI frame walks the element tree in three phases:

- **request_layout**: views render into elements, and elements request
  Taffy nodes;
- **prepaint**: Taffy computes the layout, and elements are placed, register
  hitboxes, dispatch nodes and listeners;
- **paint**: elements write primitives into the scene.

What a frame produces is stored in flat, append-only arrays on the window's
`Frame`: hitboxes, dispatch nodes, listeners, element states, and the scene's
primitives. The window keeps two frames, the one on screen and the one being
drawn, and swaps them.

Upstream GPUI already has a way to skip work on those arrays: a cached view
(`AnyView::cached`) remembers the index ranges its prepaint and paint wrote
last frame (`PrepaintStateIndex`, `PaintIndex`), and when it is not notified
and its bounds did not change, it copies those ranges from the last frame
instead of drawing itself (`Window::reuse_prepaint`, `Window::reuse_paint`).

gpui-fast does not introduce a new representation of the frame. It
generalizes that existing mechanism:

- every view is a retained subtree, cached or not;
- the decision to reuse a subtree is made from what it actually depends on,
  not just from whether it was notified and where it is drawn;
- a subtree can be reused around nested subtrees that must be rebuilt;
- layout nodes, text measurements, tessellated paths and scene orderings are
  kept across frames too, so a rebuilt view does not redo work that did not
  change.

## Identity: how an element is found again

Anything kept across frames has to be matched with the element that asks for
it on the next frame. gpui-fast uses two kinds of identity, and they have
different roles.

### Views: `GlobalElementId`, the same identity element state uses

A retained view's record is found by its `GlobalElementId`: the path of
`ElementId`s from the root to the view. A view's path includes its own entity
id, because upstream pushes `ElementId::View(entity_id)` for every view. This
is the identity upstream already uses for element state (scroll positions,
hover state, text input state), so a view is retained exactly when upstream
would consider it the same element.

The id's hash is computed once and cached, because hashing a path of ids for
every lookup is expensive. **Equality still compares the full path**:
`GlobalElementId::eq` compares the hash first and then the path. A hash
collision between two different views cannot make one reuse the other's
record.

### Layout nodes: a key that finds a cache entry, not an identity

A Taffy node is matched to its element by a 64-bit key: a hash of the path
from the root, where each step is the element's `ElementId`, or its position
among its siblings when it has none, or its index for an item of a
`uniform_list` or `list` (`fast/layout_key.rs`).

That key does not decide what the node contains. A matched node is brought
up to date with what the element asks for this frame before it is used:

- its **style** is compared with the requested one (by a fingerprint of every
  field the conversion to Taffy reads; debug builds check each fingerprint
  match with a full comparison) and rewritten if it differs;
- its **children** are compared exactly, as a list of node ids, and
  rewritten if they differ;
- a **measured** leaf keeps its measurement only if the element's inputs are
  equal to the ones it was measured from (the same text, runs and text
  style), or if measuring the new inputs under every constraint Taffy used
  gives the same sizes (see [Layout](#layout)). A node that measured itself
  and is claimed by an element that does not loses its measurement.

So two elements that hash to the same key across frames get a node whose
style, children and measurement are rewritten to what the second one asked
for: the result is the same layout, at the cost of a node rewrite. Within one
frame, a second element with a key already claimed gets a fresh node instead
of sharing one. The key decides which cached work is *tried*; correctness
does not depend on it.

### Why not a persistent tree of view nodes

An alternative is to keep a persistent tree of view nodes (for example in a
slot map), each owning its layout subtree and its rendered output.

GPUI's element tree is rebuilt every frame by design, and its existing state
already has an identity: `GlobalElementId` for element state, and the
frame's index ranges for reused output. gpui-fast keeps those, and stores
what persists where it already lives:

- layout nodes persist in Taffy, which is already a persistent node store
  with its own per-node layout cache;
- a subtree's output persists as ranges in the last frame, as it does for
  upstream's cached views;
- a subtree's record (what it read, what it holds) lives in the frame next
  to the output it points to, because those ranges belong to one frame.

Adding a second, parallel tree of view nodes would duplicate that identity
and require restructuring the window's drawing code, which would break
constraint 3 and make the fork hard to keep merged. The trade-off is
discussed further in [Drawing around a rebuilt nested view](#drawing-around-a-rebuilt-nested-view).

## Invalidation: when a view has to be drawn again

A view is drawn from the last frame only when every one of these still holds.
Each rule exists because breaking it would make the retained frame differ
from the frame drawn from scratch.

### What it read

While a view runs `request_layout`, `prepaint` and `paint`, gpui-fast
records what it reads (`fast/dependencies.rs`):

- **entities** it accessed, including its own and the views nested in it;
- **globals** it read, stamped with a generation, so writing a global
  invalidates exactly the views that read it; a view that only asked whether
  a global exists (`cx.has_global`) depends on its presence, not its value;
- **versioned state**: `ScrollHandle` and `ListState` carry a version that is
  bumped whenever their state changes, so a view that reads a scroll position
  is drawn again when it moves, whether anything was notified or not.

Reads are recorded per subtree, in two sets: everything the subtree read
including nested subtrees, and what it read itself outside them. The second
set is what allows drawing a view around a rebuilt nested view (below).

### When an entity counts as changed

Upstream GPUI's rule is that a view is rebuilt when it is notified, along
with every view around it. gpui-fast keeps the effect of that rule and
narrows the cost:

- an entity **updated and notified** (outside drawing), or **notified while
  drawing**, counts as changed for every view that read it;
- an entity **updated without being notified** counts as changed only for
  views drawn inside a view notified since the last frame. That is exactly
  the set upstream rebuilds in that case, since upstream rebuilds a notified
  view with everything under it. A subscription or observation handler that
  updates its subscriber without notifying, which happens for every event,
  therefore does not invalidate anything by itself;
- an entity **notified without being updated** (a scroll wheel, a dragged
  scrollbar, an animation) is drawn again itself, but views that only read
  it are not, since nothing it holds changed;
- the text input the window queries every frame through
  `ElementInputHandler` is updated by those queries, which do not count as
  changes unless the input notifies.

### Where it is drawn

A view's prepaint and paint depend on its bounds, its content mask, the text
style it inherits and its opacity. If any of these differs from last frame,
its output cannot be copied. If its layout nodes are still valid, it is
rebuilt at the nodes it kept and laid out at the size its parent gave it,
without laying out anything around it again.

### How it was hovered and interacted with

A view records the hover state of the hitboxes it was painted with. A hover,
scroll or press that changes how it looks draws it again. Only the innermost
view it happened in is rebuilt; the views around it are drawn from the last
frame around it.

### When nothing is retained

Every view is drawn from scratch while the window is refreshed
(`window.refresh()`, a resize, a focus change), while something is dragged,
while the inspector is picking, and while accessibility is active. Setting
`GPUI_VIEW_RETENTION=0` turns retention off entirely.

## Drawing a view from the last frame

For each view, the frame takes one of five paths
(`ViewElement` in `fast/retained.rs`):

| Path | When | What happens |
| --- | --- | --- |
| **Reused** | Nothing it depends on changed, and it is drawn where it was | Its layout comes from its kept nodes; its prepaint and paint ranges, and the records of the subtrees nested in it, are copied from the last frame |
| **Spliced** | It is dirty only because a nested view changed | It is copied from the last frame in stretches, with the nested views that changed rebuilt in the gaps |
| **Built at its retained layout** | Nothing it read changed, but it moved or what it inherits changed | It is rendered again at the layout nodes it kept, laid out within the bounds it was given |
| **Prebuilt** | A nested view was already built for a splice that turned out not to be possible | The view around it takes over the nested view already built, instead of building it twice |
| **Built** | Anything else | Rendered, laid out, prepainted and painted as upstream does, recording what it reads |

### Records

Each frame keeps a record per retained subtree: where its hitboxes, dispatch
nodes, listeners and primitives went, what it read, the hovers it was painted
with, and the layout nodes it holds (`RetainedSubtree`).

When a subtree is reused, its record and the records of every subtree nested
in it are copied too, shifted to where the copy landed. That is what allows
a nested view to be drawn on its own on a later frame even though the frame
in which it was last visited is gone: when the view around it has to be
rebuilt, the nested view can still be reused.

### Drawing around a rebuilt nested view

Notifying a view marks every view around it dirty, because they have to be
walked to reach it. Upstream rebuilds all of them. In an application whose
root view holds a sidebar, a toolbar and a page, a change deep inside the page
would rebuild the whole window.

gpui-fast rebuilds only the nested view that changed (`fast/splice.rs`). A
view that is dirty only because something nested in it is dirty (it was not
notified, and nothing it read itself changed) is drawn as follows:

- its layout is last frame's; each nested view that changed is laid out
  again at its own nodes;
- its prepaint and paint are copied from last frame up to where each nested
  view began, the nested view is prepainted and painted in that position,
  with the content mask, text style and opacity it inherited there, and the
  copy continues after it.

This is the cached-view mechanism upstream already has, applied in stretches
instead of all at once. It is not a separate kind of buffer manipulation: a
frame's output is a set of ranges in flat arrays, and a view's output is the
concatenation of its own stretches and its nested views' output. Copying a
view around a rebuilt child means copying the stretches before and after the
child and drawing the child between them.

A persistent node tree has to solve the same problem: a clean parent's output
still has to be emitted in paint order around the child's new output. There
it is done by replaying the parent's recorded output around the child. Here
it is done by copying the parent's ranges around the child. Both copy the
parent's output; the difference is where the output is kept.

If a nested view that changed asks for a different layout (a new node, a
rewritten style or child list, a measurement that no longer holds), the view
around it is rebuilt after all, taking over the nested view it already built
rather than building it twice. The splice is an optimization with a fallback
to the full rebuild, never a different result.

A nested view can be rebuilt on its own only if it can be rendered without
the view around it: it has to have been placed as an `Entity` or `AnyView`,
which is how views are almost always placed.

## Layout

Upstream clears the whole Taffy tree at the end of every frame. gpui-fast
keeps nodes across frames (`fast/layout.rs`) and writes a node's style,
children and measurement only when they differ from last frame's. Taffy
invalidates its per-node cache only when a node is written, so unchanged
parts of the tree keep their cached layout and are not laid out again.

A node unclaimed for a whole frame is released. Nodes requested without a
key (for example a list item laid out during prepaint with
`layout_as_root`) are transient and released at the end of the frame, so the
tree does not grow with every frame.

### Text measurement

A text element measures itself through a closure. Giving a node a new closure
would dirty it, and every node above it, every frame. gpui-fast avoids that
in two steps (`fast/text.rs`):

1. **Carry.** If the element at the same place last frame measured the same
   text, runs and text style, the new element takes over that measurement
   and the node stays clean. If only colors or decorations changed, the
   measurement is carried and the decorations are rewritten in place.
2. **Replay.** If the text changed (a price ticking in a table cell), the
   node keeps a log of the constraints Taffy measured it under since it was
   last dirtied, and the size each gave. The new text is measured under the
   same constraints, in the same order. If every size is the same, everything
   Taffy cached for the node and for the nodes above it still holds, and the
   node stays clean. Only text whose size changed dirties its row, its list
   and the window above.

When a replay finds a different size, the new text is measured afresh, as on
a first frame, and any measurement the replay left behind is discarded.

## The scene

### Orderings

Primitives that overlap are drawn in the order they were painted. The scene
gives each primitive an ordering: one more than the greatest ordering of the
earlier primitives it overlaps. That requires a spatial query per primitive.
Upstream uses an R-tree for it.

gpui-fast replaces it with a uniform grid of cells (`fast/bounds_tree.rs`),
which is faster for the few thousand small bounds a frame has. More
importantly, a reused subtree's primitives have the same bounds as last
frame, so their orderings are **replayed** rather than computed again: only
bounds that changed, and bounds that might overlap them, are compared. For a
frame in which nothing moved, that turns the cost of ordering from a query per
primitive into a copy.

### Other kept work

- **Dispatch nodes** of a reused stretch are written in one pass rather than
  node by node (`fast/dispatch.rs`).
- **Paths** (sparklines, chart lines, a donut) are tessellated relative to
  their first point and the triangles kept, so a path of the same shape is
  not tessellated again, wherever it is drawn (`fast/path_cache.rs`).
- **Glyph runs** work out their rendering once per run rather than once per
  glyph (`fast/glyphs.rs`).
- **Element ids and absolute bounds** are cached per frame without rehashing
  (`fast/global_id.rs`, `fast/layout_bounds.rs`).

## What an application must do

Nothing, as long as what a view renders comes from entities, globals, and
list or scroll state. Anything else a view reads (an `Rc<RefCell<..>>` shared
outside entities, the time, `window.modifiers()`) must be followed by
`cx.notify()` when it changes. Upstream already requires the same thing of
cached views.

What still costs a rebuild on every frame is a change on every frame: an
entity notified, or a global written, during prepaint or paint counts as
changed even if the value is the same. Notify or write only when the value
changes.

## Verification

Because the frame drawn from scratch is the specification, verification
compares the two directly.

- **The oracle test** (`fast/tests/oracle.rs`) drives two windows through the
  same random history of changes: one draws incrementally, the other forgets
  everything it retained before every frame. Every frame, the complete scene
  must match: layers, shadows, quads, underlines, monochrome, subpixel and
  polychrome sprites (that is, text, icons and images) with their orderings,
  and paths; and so must every hitbox with its bounds, content mask and
  behavior. The test also asserts that the incremental window really reused
  layout nodes and subtrees, so it cannot pass by never retaining anything.
- **Unit tests**, in `fast/tests/` and next to the code they test, cover
  each mechanism: reuse, rebuilding on each kind of dependency, moved views,
  splicing, layout keys, text carry and replay, the bounds grid, dispatch
  copying, and layout nodes not leaking.
- **`gpui_perf --headless --verify`** runs every benchmark scenario with
  retention on and off in lockstep and compares, every frame, the quads, the
  text, icon and image sprites and the underlines, each with its bounds, clip
  and color. It does not compare paths or shadows; the oracle test does.
- **`GPUI_VIEW_RETENTION=0`** draws every frame from scratch at runtime, to
  rule retention in or out when something looks wrong in an application.

## Costs and limits

- **Memory.** Each frame keeps a record per retained subtree, and Taffy keeps
  its nodes between frames instead of rebuilding them. Both are proportional
  to what is on screen.
- **Copying.** Reusing a subtree copies its ranges into the new frame. That
  is linear in the size of what is reused. It is much cheaper than building
  it again, but not free, so a window that changes everywhere every frame
  gains little (see the "all sixty" row below).
- **State outside entities.** A view that reads state GPUI cannot see must
  notify, as described above.
- **Fingerprints.** Whether a retained node's style changed is decided by a
  64-bit fingerprint of the style. Release builds trust it; debug builds
  compare every match in full to catch a field the fingerprint misses.

## Results

From [`README.md`](../README.md), main-thread CPU per frame, release builds
on Linux.

Headless, 60 panel views of 64 labels each (`retained_bench`):

| Panels notified per frame | From scratch | Retained |
| --- | --- | --- |
| None | 3.31 ms | 0.19 ms (−94%) |
| One | 6.72 ms | 0.71 ms (−89%) |
| Six | 7.27 ms | 1.46 ms (−80%) |
| All sixty | 10.33 ms | 7.38 ms (−29%) |

The gain follows how little of the window changes, as it should. The
`gpui_perf` showcase against upstream's `gpui-pre` snapshot, in a real window,
shows −82% to −88% for scrolling and animation in a component gallery, and
−73% to −75% for a trading workspace with streaming quotes. The README has the
full table and the commands to reproduce it.

## Staying close to upstream

All of the above lives in `crates/gpui/src/fast/`. Upstream files contain
only hooks: a field holding a `fast/` struct, a one-line call, a method body
that forwards to `fast/`, a visibility change, or a `#[path]` redirect to a
rewrite (the bounds tree is one).

`script/check-upstream` compares every upstream file with the upstream commit
it came from and fails a change that is more than a hook: a hunk that adds
more than 8 lines, a file that adds more than 40, or added lines that do not
call into `fast/`. The exceptions are listed, each with its reason, in
`script/upstream-allowlist`. As of this writing, 28 files in the tracked
upstream directories differ from upstream; the 19 of them in
`crates/gpui/src` differ by +464 and −737 lines. More lines are
removed than added, because several method bodies are replaced by a call into
`fast/`.

That shape serves two purposes. Upstream changes keep merging in with
conflicts confined to hook lines. And each mechanism in `fast/` is a
self-contained piece (retained layout nodes, the bounds grid, text
measurement carrying, retained views) that can be proposed to GPUI on its own.
