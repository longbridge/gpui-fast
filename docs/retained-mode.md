# Retained mode

How gpui-fast draws a frame from the last one, what an application needs to
know about it, and how it is checked and measured. The code is in
`crates/gpui/src/fast/`: `retained.rs` (retained subtrees), `dependencies.rs`
(what a subtree read), `layout.rs` and `layout_key.rs` (retained layout nodes)
and `stats.rs` (counters for tests and benchmarks).

## How it works

A frame walks the element tree three times: **build** renders views and asks
for layout, **prepaint** computes layout and places elements, **paint** turns
them into the scene handed to the GPU. Upstream GPUI does all three from
scratch every frame.

### Views

Every view — any `Entity<V: Render>` or `AnyView` placed in the element tree,
cached or not — is a retained subtree. While nothing it depends on changed
since the last frame, it is not rendered, laid out, prepainted or painted: its
layout comes from the nodes it kept, and its hitboxes, listeners, dispatch
nodes and primitives are copied from the last frame. A view depends on:

- **What it read.** Every entity accessed and every global read while it was
  rendered, laid out, prepainted and painted — its own entity, models it reads
  without observing them, the views nested in it — and the `ListState`s and
  `ScrollHandle`s its elements track. Those two bump a version whenever their
  state changes, so a view is drawn again when one it read moved, notified or
  not.
- **What it was updated with.** An entity updated (`entity.update(..)`) while
  no view is being drawn — by a task, a listener, an action — counts as
  changed even if nobody notified it, since upstream would have rendered the
  view reading it again anyway when the view around it was notified.
- **Where it is drawn.** Its bounds, content mask, text style and opacity. A
  view that moved is built again, at the layout nodes it kept, and laid out at
  the size its parent gave it.
- **The hovers it was painted by**, and interactions inside it: a hover,
  scroll or press that changes how it looks draws it again.

Nothing is drawn from the last frame while the window is being refreshed
(`window.refresh()`, and what refreshes it: a resize, a focus change), while
something is dragged, while the inspector is picking, or while accessibility
is active.

A notified view marks the views around it dirty, because they have to be
walked to reach it. A view that is dirty only for that reason — it was not
notified, nothing it read itself changed and it is hovered as it was — is not
built again: it is drawn from the last frame stretch by stretch, with the
nested views that changed built again in the gaps where they were, at their
own layout nodes and with what they inherited there. If a nested view asks for
another layout, the view around it is built after all, taking over the nested
view already built rather than building it twice. The more of a window is
split into views, the less a change costs. The code is in
`crates/gpui/src/fast/splice.rs`.

A view counts as having read itself, so an application that changes a view
outside drawing (`entity.update(..)`) without notifying it gets it built again
on the next frame. A frame driver or timer that only needs to notify another
view should notify it by id (`cx.notify(entity_id)`) rather than updating the
view that owns the driver.

### Records per retained subtree

Each frame keeps a record per retained subtree: where its hitboxes, dispatch
nodes, listeners and primitives went, what it read, the hovers it was painted
by and the layout nodes it holds. The records live in the frame rather than in
element state, because the stretches they point to belong to one frame. Drawing
a subtree again copies its record and the records nested in it, shifted to
where the copy landed, so a nested subtree stays reusable on its own later,
when what is around it has to be built again. A view that is built again
therefore does not build everything nested in it.

### Cached views

`Entity::cached(style)` and `AnyView::cached(style)` are upstream's API and
work as upstream documents them. They are retained subtrees like any other
view, so they are also built again when an entity or global they read changed,
and keep their layout nodes while they are reused.

### Layout nodes

Upstream clears the whole Taffy tree at the end of every frame. gpui-fast keeps
nodes from one frame to the next, and writes a node's style, children and
measurement only when they differ from last frame's, so Taffy's own layout
cache survives and unchanged parts of the tree are not laid out again. An
element finds its node again by:

- its path from the root of the element tree, each step being the element's
  `ElementId`, or its position among siblings without one;
- its `ElementId`, wherever it moves among its siblings;
- its index, for an item of a `uniform_list` or `list` without an `ElementId`,
  so rows still in view keep their layout while the list scrolls.

A node unclaimed for a frame is released.

A text element measures itself, and a measured node given a new closure would
be dirtied every frame, with every node above it. Instead, when last frame's
text element at the same place measured the same text, runs and text style,
the new element takes a copy of that measurement and the node is left clean.
A view built again, because it moved or because the view around it was
notified, is then not laid out again unless something in it changed.

## What an application needs to know

Nothing, as long as what a view's render reads lives in entities, globals and
list or scroll state. Anything else it reads — an `Rc<RefCell<..>>` shared
outside entities, the time, `window.modifiers()` — it has to be notified of
(`cx.notify()`), as a cached view already has to be in upstream GPUI. Otherwise
it keeps showing what it showed when it was last built.

To rule retention in or out when something looks stale, run with
`GPUI_VIEW_RETENTION=0`: every view is then drawn from scratch each frame, as
upstream does.

## How it is verified

A retained frame has to be the frame drawing from scratch would have produced.

- The oracle test (`crates/gpui/src/fast/tests/oracle.rs`) drives two windows
  through the same random history, one drawing incrementally and one from
  scratch, and requires every frame to match. It covers sibling, nested and
  deferred views notified alone, a model read without being observed, and a
  global, and asserts that views really were reused.
- `crates/gpui/src/fast/tests/retained.rs` covers reuse, rebuilding when a
  dependency or a hover changes, moved views and retention turned off.
- `cargo run -p gpui_perf --release -- --headless --verify` compares the quads
  painted with retention on and off on every frame of every simulated screen.

```sh
cargo test -p gpui --features test-support
cargo run -p gpui_perf --release -- --headless --verify
```

## How it is measured

Measure release builds only. [`CONTRIBUTING.md`](../CONTRIBUTING.md#measuring)
has the full usage.

`cargo run -p gpui_perf --release` opens the showcase: a component gallery
shaped like GPUI Kit's, which scrolls its sidebar, a page or a data table by
itself, refreshes the table on a timer, and shows in its status bar what each
frame costs. `--auto` runs each of those with retention on and off and prints
the comparison:

```sh
cargo run -p gpui_perf --release
cargo run -p gpui_perf --release -- --auto
```

With `--headless`, `gpui_perf` drives simulated screens — forms, lists, a
data table, a settings page — without a window, with real text shaping, with
retention on and off, and compares what each frame cost:

```sh
cargo run -p gpui_perf --release -- --headless
cargo run -p gpui_perf --release -- --headless --scenario table --frames 200
```

The `views_frames` example draws a dashboard of panel views into a real
window; the arguments are panels, labels per panel and panels changed per
frame. It prints main-thread CPU per frame and the build, prepaint and paint
times:

```sh
cargo run -p gpui_perf --example views_frames --release -- 60 64 2
GPUI_VIEW_RETENTION=0 cargo run -p gpui_perf --example views_frames --release -- 60 64 2
```

A headless benchmark of 60 panel views × 64 labels is in
`crates/gpui/src/fast/tests/retained_bench.rs`:

```sh
cargo test -p gpui --lib --release retained_bench -- --ignored --nocapture
```
