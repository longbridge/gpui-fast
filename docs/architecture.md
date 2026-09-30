# Architecture

This document explains how gpui-fast makes GPUI draw a frame from the last
one, and why it is built the way it is. [`retained-mode.md`](retained-mode.md)
is the reference for what an application sees and how retention is verified
and measured; [`upstream-sync.md`](upstream-sync.md) covers how the fork stays
mergeable. This document covers the reasoning behind both.

## The problem

GPUI draws in immediate mode. Outside subtrees an application explicitly
caches (`AnyView::cached`), GPUI reconstructs a frame's element, layout,
prepaint and paint work every time it draws one: it renders the views, builds
a new element tree, lays out a new Taffy tree, shapes the text and paints the
scene, even when one number in one table cell changed. The cost of a frame
is close to the cost of the whole window, not the cost of what changed.

gpui-fast moves the cost of a frame from the cost of the whole window toward
the cost of what changed. When nothing a view depends on has changed, the
view is not rendered, not laid out, not prepainted and not painted; its
output is taken from the last frame.

It does this without turning GPUI into a conventional retained-mode
framework. What is rebuilt is still constructed through GPUI's normal element
pipeline. gpui-fast associates the elements of the new frame with work the
previous frame produced, records what that work depended on, and for each
view chooses to:

1. reuse the previous frame's output,
2. splice a changed nested view into the previous output around it, or
3. rebuild, when reuse would not be safe.

Layout and text measurement also keep enough state across frames to avoid
invalidating expensive caches. Scrolling, where everything in a scroll
container moves and so no view is drawn where it was, is handled by a
separate mechanism: the scrolled content is kept as cached GPU tiles and
composited at the new offset (see [Scrolled content](#scrolled-content-layers)). **gpui-fast retains frame work, not a parallel
UI tree**: it is incremental reconstruction of GPUI's frames over its
immediate element tree.

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
it on the next frame. gpui-fast does not keep a parallel persistent element or
view hierarchy. When a view is rebuilt, its elements are constructed again
through GPUI's normal pipeline; when it is reused, its interior is not
constructed at all, and its output and the records of the views nested in it
are copied from the last frame. What persists is keyed by each view's
`GlobalElementId`:

```text
a view is reached while the frame is drawn
        │
        ▼
its GlobalElementId
        │
        ▼
the RetainedSubtree the previous frame recorded for it
        │
        ▼
check what it depended on, and where it is drawn
        │
        ▼
reuse, splice, or rebuild
```

The state that makes this possible is spread over systems GPUI already has,
not kept in a second tree:

```text
Frame (the previous one, and the one being drawn)
 ├─ output ranges: hitboxes, dispatch nodes, listeners, primitives
 ├─ a RetainedSubtree record per view
 └─ the records of views nested in reused views, copied and shifted

Taffy
 └─ layout nodes kept across frames, with their layout caches

App (the dependency system)
 └─ generations and change stamps for entities, globals and ambient
    window input

ScrollHandle, ListState
 └─ a version each carries, bumped when its state changes
```

gpui-fast uses two kinds of identity, and they have different roles.

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
what persists where it already lives, as the diagram above shows:

- layout nodes persist in Taffy, which is already a persistent node store
  with its own per-node layout cache;
- a view's output persists as ranges in the last frame, as it does for
  upstream's cached views;
- a view's record (what it read, what it holds) lives in the frame next to
  the output it points to, because those ranges belong to one frame.

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
  is drawn again when it moves, whether anything was notified or not;
- **ambient window input**: reading the pointer position
  (`Window::mouse_position`) or the modifier keys and caps lock
  (`Window::modifiers`, `Window::capslock`) is recorded like reading a
  global. After the window handles an input event, it compares these values
  with what they were before the event and marks the ones that changed, so
  the views that read them are drawn again without notifying anything.

Reads are recorded per retained subtree, in two sets, and the distinction is
what the rest of the design turns on:

- **subtree dependencies** (`dependencies`): everything the view read,
  including what the views nested in it read;
- **own dependencies** (`own_dependencies`): what the view read itself,
  outside the retained views nested in it.

Hovers are recorded the same two ways. The subtree set answers "can this view
be reused as it was?" The own set answers a different question: "is the view
itself unchanged, even though something nested in it changed?" That is what
lets gpui-fast tell

```text
the view itself changed                    → rebuild it
the view is unchanged, a nested view changed → splice the nested view in
```

apart, and splice instead of rebuilding the whole view (see
[Drawing around a rebuilt nested view](#drawing-around-a-rebuilt-nested-view)).

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
`GPUI_VIEW_RETENTION=0` turns retention off entirely, and scroll layers with
it; `GPUI_SCROLL_LAYERS=0` turns off scroll layers alone.

## Drawing a view from the last frame

For each view placed as an `Entity` or `AnyView`, the frame takes one of five
paths (`ViewElement` in `fast/retained.rs`):

| Path | When | What happens |
| --- | --- | --- |
| **Reused** | Nothing it depends on changed, and it is drawn where it was | Its layout comes from its kept nodes; its prepaint and paint ranges, and the records of the subtrees nested in it, are copied from the last frame |
| **Spliced** | It is dirty only because a nested view changed | It is copied from the last frame in stretches, with the nested views that changed rebuilt in the gaps |
| **Built at its retained layout** | Nothing it read changed, but it moved or what it inherits changed | It is rendered again at the layout nodes it kept. If it asks for a different layout, it is laid out within the bounds it was given, and from scratch on the next frame |
| **Prebuilt** | A nested view was already built for a splice that turned out not to be possible | The view around it takes over the nested view already built, instead of building it twice |
| **Built** | Anything else | Rendered, laid out, prepainted and painted as upstream does, recording what it reads |

A view placed with `cached(style)` keeps upstream's cached-view shape: it is
laid out at the style it was given, and is either reused, when nothing it
depends on changed and it is drawn where it was, or rendered again and laid
out as a root within its bounds. Unlike upstream, what it depends on includes
everything it read, not only whether it was notified.

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
notified, its own dependencies and own hovers are unchanged, and every dirty
view nested in it can be rebuilt on its own) is drawn as follows:

```text
the view's own dependencies unchanged, a nested view changed
        │
        ▼
copy the view's output from last frame, up to the nested view
        │
        ▼
rebuild only the nested view, where it was
        │
        ▼
copy the view's output from last frame, after the nested view
```

In more detail:

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
keeps nodes across frames (`fast/layout.rs`), but keeping them is not the
optimization by itself. Taffy discards a node's cached layout, and that of
every ancestor, whenever the node is written: `set_style`, `set_children`
and `set_node_context` all dirty it unconditionally, whether the value
changed or not. The value comes from **not writing** to a kept node when the
element asks for what it asked for last frame. For that, each kept node
remembers the request that produced it: a fingerprint of its style, its
list of children, and, for a text leaf, the inputs of its measurement.
Those are compared first, and the node is written only if they differ, so
unchanged parts of the tree keep their cached layout and are not laid out
again.

A node unclaimed for a whole frame is released. Nodes requested without a
key (for example a list item laid out during prepaint with
`layout_as_root`) are transient and released at the end of the frame, so the
tree does not grow with every frame.

### Text measurement

A leaf that measures itself holds a closure in its node context. For most
measured leaves (a `uniform_list` or `list` measuring its own size, for
example), nothing says what the measurement depends on, so a kept node is
given the new closure and dirtied every frame. Text is the exception, because
its inputs are known and comparable.

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

## Scrolled content: layers

### Why retained views do not cover scrolling

A view is drawn from the last frame only where it was drawn
([Where it is drawn](#where-it-is-drawn)). When a container scrolls, every
view inside it moves, so every one of them is built at its retained layout:
rendered, prepainted and painted again. Nothing it read changed; only an
offset did. In GPUI Kit's component gallery that was about 60 % of a
scrolled frame.

Copying a view's output to a new position is not a fix. The output holds
positions in window coordinates everywhere: primitives, hitboxes, listeners
that captured bounds, element state. And GPUI culls while it paints, so what
was outside the viewport last frame was never painted and cannot be copied.

### The model

gpui-fast does what UIKit, Flutter and Chrome do (`fast/layers/`): a scroll
container's content is painted once into a layer, rasterized into tiles, and
on frames where only the offset changed the tiles are composited at the new
offset.

```text
container scrolls
        │
        ▼
only the offset changed?  ── no ──► repaint the content into the layer
        │ yes                         (viewport + 2 viewports of overscan)
        ▼
still inside what was painted?  ── no ──► repaint around the new viewport
        │ yes
        ▼
composite the tiles at the new offset; carry the content's
hitboxes, listeners and dispatch subtree, translated
```

For each scroll container, a frame takes one of three paths
(`fast/layers/policy.rs`):

| Path | When | What happens |
| --- | --- | --- |
| **Composite** | Only the offset changed, and the viewport is still inside the painted region | The content is not rendered, prepainted or painted. Its non-scene records are carried from the last frame, hitboxes translated. The tiles covering the viewport go into the scene as sprites |
| **Repaint** | The content changed, or the scroll reached the edge of the painted region | The content is painted into the layer's own scene over the viewport plus two viewports of overscan on each scrolled side. Each tile is hashed; only tiles whose hash changed are rasterized again |
| **Bypass** | A layer cannot guarantee the result (below) | The frame is drawn exactly as without layers |

A container gets a layer once it has scrolled on two consecutive frames, and
loses it when its content changes on most frames (demotion), when it is not
composited for 120 frames, or when the window is resized.

### Telling a scroll from a change

A scroll notifies the view holding the container, which by the rules above
would count as a change. A scroll is therefore noted separately
(`fast/layers/invalidate.rs`). The div's wheel listener, and
`ScrollHandle::set_offset` for code that scrolls (a dragged scrollbar,
`scroll_to_*`), note which container moved before the view is notified. A
frame is scroll-only for a layer when the view holding the container was
notified no more times than it was scrolled, nothing the content read
changed, no view inside it was notified, and no hover it was painted with
changed. A render that reads the offset (`ScrollHandle::offset` and the
like) makes the view depend on it like any other state.

### The same pixels

A composited frame must be the frame drawn from scratch
([Constraints](#constraints)), byte for byte:

- **Background.** Subpixel text needs to know what it is drawn over, so a
  layer is used only when the viewport lies on one opaque, solid-colour
  quad. Its colour is baked into every tile. A gradient, a translucent window
  or a rounded corner inside the viewport means Bypass.
- **Pixel grid.** Scroll offsets are snapped to device pixels wherever
  layers are compiled, with or without a layer, so both paths place content
  on the same grid. Glyph origins are quantized so that a whole-pixel shift
  moves them by exactly that shift (`fast/glyphs.rs`); on screen the result
  is upstream's.
- **Culling.** Content in overscan must be painted, so while a layer paints,
  the scroll container culls against the painted region, not the viewport.
  Glyphs are culled by where they can reach, not by the top of their line,
  so a line painted in overscan and scrolled in shows the same glyphs as
  when painted in place.
- **Paths.** A path's antialiasing depends on how its pixels pair in 2×2
  quads, so a path rasterized into a tile and moved by an odd number of
  pixels differs by one level. Paths are never rasterized into tiles: they
  are drawn over the tiles in the frame when nothing covers them, and Bypass
  otherwise.

### Positions that application code sees

Application code only ever sees window coordinates, and they are current
when it sees them (`fast/layers/input.rs`, `fast/layers/reuse.rs`):

- the content's hitboxes are carried translated, so hit testing, hover and
  cursor styles are exact on composited frames;
- a press, a release, a drop, or a pointer move that lands on the content's
  own elements first repaints a layer that moved since it was painted, so
  listeners and element state hold current bounds when the event reaches
  them. A move that lands elsewhere (on a scrollbar being dragged) does not;
- scroll handle bounds are translated when read;
- content that cannot be carried makes its container Bypass: deferred and
  anchored elements, a focused text input, a children-prepainted listener.

### The renderer contract

The core hands renderers two things through `Scene` (`fast/layers/scene.rs`):

- **tile sprites**: each visible tile is an ordinary polychrome sprite whose
  texture id lies in a range no atlas allocates, so ordering, clipping and
  batching are the scene's own;
- **`Scene.layers`**: each composited layer's content scene (shared, handed
  over without copying), its generation and its dirty tiles.

A renderer keeps tile textures by layer and generation, rasterizes dirty and
missing tiles before its main pass from `LayerFrame::tile_scene`, and binds a
tile's texture when it meets a tile sprite. Nothing flows back to the core.
Layers are compiled in only where a renderer implements this
(`fast::layers::COMPILED`): Linux (wgpu) and macOS (Metal) today. The
Direct3D renderer is being ported. Each renderer is verified by the same pixel
tests on its platform's CI. Metal's gradient dither is seeded from the
position within the quad rather than on screen, so a gradient rasterized into
a tile gets the same noise as one drawn in the window.

### Lists

`uniform_list` and `list` are covered by the design but their layers are off
(`fast::layers::lists::LIST_LAYERS`). As built, rows that take input are
never given a layer, a hover change repaints every held row, and adding rows
rebuilds the whole content. Lists need per-row records for layers to pay off.

## What an application must do

Reusing a view is safe only if gpui-fast learns about every change to state
that affects what the view draws:

```text
safe reuse  requires  every change that affects rendering to be observable
```

This is the boundary of any reactive or incremental system, not something
particular to gpui-fast.

No application changes are required for state that goes through the
mechanisms gpui-fast tracks: entities (read, updated and notified), globals,
`ScrollHandle` and `ListState`, hovers and interactions, and the window's
pointer position, modifier keys and caps lock. A change to any of them
invalidates exactly the views that read it.

State that gpui-fast cannot observe remains the application's
responsibility. Examples are values held in `Rc<RefCell<T>>` or `Cell<T>`
outside an entity, atomics, thread-local or static mutable state, wall-clock
time (`Instant::now()`), files or network state, and any other interior
mutability. These are not unsupported; a view may read them. But their
changes are invisible to dependency tracking, so whatever changes them must
also cause a GPUI notification (`cx.notify()` on a view that reads them, or
an `entity.update` that notifies), or the view keeps showing what it showed
when it was last built. Upstream already requires the same of cached views.

What still costs a rebuild on every frame is a change on every frame: an
entity notified, or a global written, during prepaint or paint counts as
changed even if the value is the same. Notify or write only when the value
changes. A per-frame registry is the common case: GPUI Kit's text selection
reset a counter in a global and re-registered every selectable view on every
frame, which rebuilt every view that read that global on every frame, and
only worked because it did. Registrations have to persist while a view is
drawn from the last frame.

Scroll through `ScrollHandle` (or a list's state), not by writing an offset
held elsewhere, so a scroll is told apart from a change of the content.

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
- **The layer oracle** (`fast/tests/layers_oracle.rs`) plays random histories
  of wheel scrolls (fractional deltas at fractional scales included),
  programmatic scrolls, hovers, clicks, key presses and content changes in a
  window with layers and one without. Every frame it compares the scenes
  with each layer's tiles expanded into their primitives, hit tests at random
  points, and the positions every listener observed.
- **Renderer pixel tests** (`gpui_wgpu`, and each renderer that implements
  layers) rasterize and composite tiles and compare them with the same
  content drawn directly, byte for byte, on a real device.
- **`GPUI_VIEW_RETENTION=0`** draws every frame from scratch at runtime, to
  rule retention in or out when something looks wrong in an application;
  `GPUI_SCROLL_LAYERS=0` does the same for scroll layers.

What these checks establish is that, for the sequences of state changes they
run, the retained output equals the output drawn from scratch. They exercise
the dependency tracking thoroughly, but they cannot prove anything about
state an application reads outside it: a view that reads hidden state which
changes without a notification is outside what any such test can see (see
[What an application must do](#what-an-application-must-do)).

## Costs and limits

- **Memory.** Each frame keeps a record per retained subtree, and Taffy keeps
  its nodes between frames instead of rebuilding them. Both are proportional
  to what is on screen.
- **Copying.** Reusing a subtree copies its ranges into the new frame. That
  is linear in the size of what is reused. It is much cheaper than building
  it again, but not free, so a window that changes everywhere every frame
  gains little (see the "all sixty" row below).
- **Untracked state.** A view that reads state gpui-fast cannot observe needs
  a notification when it changes, as described above.
- **Fingerprints.** Whether a retained node's style changed is decided by a
  64-bit fingerprint of the style. Release builds trust it; debug builds
  compare every match in full to catch a field the fingerprint misses.
- **Tiles.** A window's layers hold up to 64 MB of tile textures, least
  recently composited evicted first.
- **Repaints.** A frame that repaints a layer paints five viewports of
  content, and costs several times a frame without layers (up to 5–6.5 ms
  against 0.6 ms in `gpui_perf`'s scrolled page). It comes once per two
  viewports scrolled.
- **Where layers do not apply.** The hooks cost 0.85 % more instructions on
  average across `gpui_perf`'s headless scenarios, and at most +2.7 %.

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

The gain follows how little of the window changes, as it should.

Scrolling, with scroll layers (`gpui_perf --headless`, real wheel events,
retained views on in both columns):

| Scenario | Layers off | Layers on |
| --- | --- | --- |
| A page of 24 sections of buttons, in a child view | 0.618 ms | 0.068 ms (−89%) |
| The same page drawn by the scrolling view itself | 0.599 ms | 0.226 ms (−62%) |

In GPUI Kit's component gallery, scrolling the Button story at 145 Hz costs
27–28 % of a core with gpui-fast's retained views alone and 18–20 % with
scroll layers (draw per frame 1.53–1.63 ms and 0.92–1.02 ms). The
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
`script/upstream-allowlist`. As of this writing, 30 files in the tracked
upstream directories differ from upstream; the 20 of them in
`crates/gpui/src` differ by +609 and −776 lines. More lines are
removed than added, because several method bodies are replaced by a call into
`fast/`.

The public API has one exception, which renderers need: `Scene.layers` and
the types it holds, listed in [`upstream-sync.md`](upstream-sync.md).

That shape serves two purposes. Upstream changes keep merging in with
conflicts confined to hook lines. And each mechanism in `fast/` is a
self-contained piece (retained layout nodes, the bounds grid, text
measurement carrying, retained views, scroll layers) that can be proposed to
GPUI on its own. [`scroll-layers.md`](scroll-layers.md) is the reference for
scroll layers: where they apply, how they fall back, and how to measure them.
