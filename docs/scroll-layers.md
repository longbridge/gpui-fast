# Scroll layers

Retained views make a frame cheap when views did not change. Scrolling defeats
them: everything inside a scroll container moves, and a retained view is only
drawn again where it was drawn before, so every view in the scrolled content is
built again on every scrolled frame. Scroll layers handle that case the way
UIKit, Flutter and Chrome do. A scroll container's content is rasterized once
into cached tiles, and a frame in which only the scroll offset changed draws
those tiles at the new offset instead of building the content again.

The design is in
[`superpowers/specs/2026-09-30-scroll-layers-design.md`](superpowers/specs/2026-09-30-scroll-layers-design.md).
This page covers how the pieces fit together, when a container falls back to
drawing as it does without layers, and how to verify and measure layers.

## Where layers apply

- Linux, on the wgpu renderer; macOS, on the Metal renderer; and Windows, on
  the Direct3D 11 renderer. Everywhere else layers are compiled out
  (`fast::layers::COMPILED`), and nothing changes.
- Scrolling `div`s (`overflow_x_scroll` / `overflow_y_scroll`), whether their
  content is a child view or plain elements in the same view.
- `uniform_list` and `list` (`fast::layers::lists`), unless flipped
  vertically. A list's layer holds the rows its viewport shows and two
  viewports of rows on each side, each row kept apart:
  - A frame that only scrolled the list renders only the rows the scroll
    brought into the overscan, the rows whose hover changes and the rows
    that show again after leaving the viewport. Every other row it keeps:
    it is not rendered, laid out, prepainted or painted, and its hitboxes,
    listeners, element states and dispatch nodes are carried from the last
    frame, its hitboxes moved by the scroll since the row was painted.
  - The frame hit tests the pointer between prepaint and paint, and hover
    styles are painted by that. Which rows to render again for their hover is
    decided as the rows prepaint, so the hit test is foretold: the last
    frame's hitboxes, the rows' moved to where they show now. Only the
    rows whose hover changes are rendered again, not the whole layer.
  - A list without a layer drops the element states of a row it no longer
    shows (a nested scroll offset, say). A row leaving the viewport is
    neither rendered nor keeps its element states that frame, and is
    rendered afresh before it shows again.
  - The layer's content is one part per row (`LayerContent`), and each row's
    tiles are hashed once, when it is painted. A frame adding a row hands the
    renderer the other rows as they were and rasterizes only the tiles the
    new row reaches. Rows past the overscan are dropped a batch at a time.
  - The first frame painting a list's layer paints the whole overscan. A
    frame painting it afresh after that, for a change of its content, paints
    only the rows the list shows, as the list does without a layer. A frame
    that keeps the rows renders no more than three quarters of a viewport of
    rows, those it shows that the layer lacks first: the rest of that budget
    grows the overscan, three quarters of it on the side the list scrolls
    toward (`fast::layers::lists::RENDER_PER_FRAME_VIEWPORTS`). A view holding the
    list that is notified now and then, as a chat transcript is when it
    reaches its end, does not rebuild five viewports of rows each time, and a
    scroll faster than the overscan grows renders the rows it shows, not five
    viewports of rows around them.
  - A view that asks a coarse question of where a container is scrolled as
    it renders is taken to have read only the answer
    (`fast::layers::answers`): `ListState::is_scrolled_to_end` (a chat
    transcript's "back to bottom" button), `ListState::item_is_above_viewport`
    and `item_is_below_viewport` (an outline lighting the turns in view),
    `ScrollHandle::top_item` and `bottom_item`, and
    `UniformListScrollHandle::is_scrolled_to_end`. When the view's record is
    judged the question is asked again of the state as it is then (of a
    `list` being prepainted, as its prepaint began), and only another
    answer is a change: a scroll that leaves every answer as it was neither
    builds the view again nor keeps the container off its layer. Reading the
    offset itself (`logical_scroll_top`, `bounds_for_item`,
    `scroll_px_offset_for_scrollbar`, `ScrollHandle::offset`, ...) as it
    renders still depends on the offset, as what is done with a pixel value
    cannot be told. `ListState::scroll_to_end` on a list already at its end,
    and `ScrollHandle::scroll_to_bottom` on a handle already at its bottom,
    as a view keeping either there calls on every render, change nothing.
- `GPUI_SCROLL_LAYERS=0` turns layers off for a process.
  `Window::set_scroll_layers` does the same for one window in tests.

## How a frame uses a layer

A container gets a layer once it has scrolled on two consecutive frames. On
each frame it then takes one of three paths (`fast::layers::policy::decide`):

- **Composite.** Only the scroll offset changed: no view in the content was
  notified or read anything that changed, and no hover changed. The content is
  not prepainted or painted. Its hitboxes are carried over, translated. Its
  listeners, cursor styles and dispatch subtree are carried over too
  (`fast::layers::reuse`). The tiles that cover the viewport go into the scene
  as polychrome sprites with reserved texture ids.
- **Repaint.** The content changed, or the scroll reached the edge of what was
  painted. The content is painted into the layer's own scene over the viewport
  plus two viewports of overscan on each scrolled side (`fast::layers::paint::OVERSCAN_VIEWPORTS`). Each tile is hashed, and
  the renderer rasterizes again only the tiles whose hash changed
  (`fast::layers::tiles`), and only once a frame shows them: a dirty tile in
  the overscan waits, so a list whose rows keep arriving at the edge of its
  overscan does not rasterize the tiles there on every frame.
- **Bypass.** The frame is drawn exactly as it would be without layers. This
  happens when a layer cannot guarantee identical pixels or current window
  coordinates, for example:
  - the background under the viewport is not one opaque solid quad;
  - the content has deferred or anchored elements, a focused input, or
    surfaces, or something in it (an element, or a view drawn in it) asked
    for an animation frame this frame or the last;
  - the view holding the container asked for an animation frame from
    outside the content (a scrollbar fading beside a list, a pulsing
    button): it renders again on every frame the animation runs, and
    rendering the content again with it costs more than drawing it without
    the layer. The layer is dropped but not demoted, and painted again as
    soon as the animation stops. An animation in a view around the one
    holding the container leaves the layer composited;
  - the scroll moved past everything the layer holds since the last frame,
    as a scrollbar's thumb dragged fast does: the layer is dropped, and is
    not promoted again while the scroll moves by a viewport or more a frame;
  - the content changes on at least eight of the last sixteen frames, or
    the layer's estimated work exceeds drawing directly (demotion, below);
  - a row of a list draws over the paths of a row before it, or over what
    that row drew over its paths.

The view holding the container is notified by the container's wheel
listener, and a frame is taken for a scroll only while that view was
notified no more often than the wheel scrolled what it holds. Other wheel
listeners react to the same scroll: GPUI Kit's scrollbar notifies the view
again when the offset moved since it last saw it, to show itself. A
notification of a view sent while a wheel event is dispatched, when the event
scrolled a container that view painted, counts as one for the scroll as long
as nothing else changed while the event was dispatched: no entity was
updated or written and no global changed (`fast::layers::wheel`). A wheel
listener that updates an entity and notifies changed the content, and the
layer is painted again. What this assumes is that state outside entities
that a listener changes in reaction to a scroll, and notifies the view for,
does not change what the view renders inside the scrolled content: the
scrollbar beside it is drawn afresh, the content is not. State that does
belongs in an entity the listener updates.

The demotion policy also tracks rebuilding work over the last 32 completed
layer frames (`fast::layers::work`), in units of drawing the visible content
directly. Virtual lists count the rows they rendered, by their height against
that of the rows shown (or of the viewport, if taller) and by their paint
operations against those of the rows shown; other scroll containers count painted primitives relative to
those visible in the viewport. Scroll extensions, hover changes and input
rebuilds count too. Painting content afresh costs about twice what drawing it
directly does (its tiles are hashed and its records rebuilt besides), and a
repaint's work is counted at `REPAINT_COST` times. The initial cache build is
excluded, and a quarter of each frame's budget is reserved for cache
bookkeeping and tile rendering. A layer whose estimated work reaches direct
drawing's budget falls back even if updates occur on fewer than half the
frames, and one whose last six frames cost more than drawing directly, their upkeep
included, half of them each costing more, falls back at once, without waiting
for the 32-frame average. Two content refreshes that each rebuild more than two
visible regions within 120 frames of each other also trigger fallback: broad
updates must not keep causing latency spikes. Refreshes further apart than
that are paid back by the frames composited between them. One isolated update
and the initial cache build are not enough to trigger either guard.

Demotion releases cached rows and resets the fixed-size work history. The
first cooldown requires 60 stable frames; repeated demotions double that wait,
up to 1920 frames, so periodic refreshes do not keep rebuilding and discarding
the cache. An update during cooldown asks for 60 more stable frames from it,
so a view notified every few seconds gets its layer back between
notifications. A repaint for a change that left every row it painted as the
layer held it does not count as a changed frame. After 1920 quiet frames the
backoff resets. This is a work estimate, not a measurement of GPU
time, and does not depend on the monitor's refresh rate.

Paths are never rasterized into tiles: the path shaders antialias with
screen-space derivatives (`dpdx`/`dfdx`/`ddx`), taken within the 2×2 pixel
quads the GPU shades together, so a tile composited at an odd translation can
come out one level apart on a path's edge pixels. Instead the content is split
(`fast::layers::overlay`): its paths, and everything drawn after a path or
after such a primitive that overlaps it, are the layer's *overlay*, kept out
of the tiles and drawn over them in the frame, in drawing order, wherever the
layer is composited. On every pixel the primitives drawing it are then drawn
in the same order as without a layer, and the paths are rasterized where they
show. A table in a rounded frame, whose corner notches are paths under the
frame's border, composites with only the paths and the border drawn each
frame.

### Changes inside a list's rows

A virtual list's layer keeps what each row read apart from what the list read
besides (`fast::layers::lists`). While a row renders, is laid out, prepainted
and painted, the entities it reads are logged under it; the record keeps only
what the rest of the list read. A frame that finds a row's reads changed, or a
view drawn in it notified, still composites: it renders that row again, alone,
as it does a row whose hover changed. A row the list only measured, without
prepainting it, is not the layer's, and nothing it read is kept.

What the list and its rows write while the list is built — a row noting in a
model the rows read that it was drawn — is part of building the list, not a
change of the rows the layer keeps. What the view holding the list writes in
its `render`, as a sidebar writing the items its rows show into a model they
read on every render, is judged with that render, by what the view read: the
rows the layer holds are not all rendered again for it.

When the view holding a `list` renders again for something the layer cannot
tell apart (a notification, a change of what it read itself), it may hand the
list a different row renderer. The frame still composites: the list renders
the rows it shows again, and every other row the layer holds is marked suspect
and rendered again before it shows. A list whose item count changed is
painted afresh instead, as its rows moved to other indices.

Rows keep their layout nodes: each row the layer holds keeps the keys its
layout claimed (and only its own, though a `list` lays out every row it shows
before it prepaints the first), and a row's nodes outlive it for 240 frames
after it leaves, so scrolling back over it reuses them and the measurements
they carry instead of shaping its text again.

## Keeping pixels and coordinates true

- Scroll offsets are snapped to device pixels wherever layers are compiled,
  with or without a layer, so both paths put content on the same grid. Glyph
  origins are quantized so that a whole-pixel shift moves them by exactly that
  shift (`fast::glyphs::quantize_origin`). On screen the result is unchanged.
- Tiles are cleared with the baked background, so subpixel text blends exactly
  as it does on screen.
- Metal's gradient dither is seeded from the position within the quad, not on
  screen, so a gradient gets the same noise in a tile as in the window.
- Before any input other than the wheel reaches a layer that has moved since
  it was painted, the layer is repainted (`fast::layers::input`). Listeners,
  element state and anything handed to application code therefore hold
  current window coordinates. A list's rows are painted over many frames,
  at as many offsets: any row painted at another offset than the one shown
  counts as moved.
- A list's rows are painted in a region reaching far above and below the
  viewport instead of inside the list's own clip, so that no edge of what
  a row was painted in clips it once it has moved; the viewport clips it
  where it is composited, as the list's clip does without a layer.

## Verifying

- `cargo test -p gpui --features test-support --lib fast::tests::layers`
  includes the layer oracle (`fast/tests/layers_oracle.rs`). It runs random
  histories of wheel scrolls (including fractional deltas at scale 1.25),
  hovers, clicks, keys and content changes through a window with layers and a
  window without. Every frame it compares the expanded scenes, hit tests and
  the positions listeners observe.
- `cargo test -p gpui_wgpu` compares rasterized and composited tiles with
  direct drawing, byte for byte, on a surfaceless device.
- `cargo test -p gpui_apple fast::layers` does the same on macOS with a
  headless Metal renderer (`crates/gpui_apple/src/fast/layers/`), which draws
  tiles as the wgpu renderer does.
- `cargo test -p gpui_windows --features test-support fast::layers` does the
  same for the Direct3D 11 renderer (`gpui_windows/src/fast/layers/`), on a
  hardware device or WARP, on Windows.
- `cargo run -p gpui_perf --release -- --headless --verify` also runs the
  scroll scenarios with layers on and off.

## Measuring

The `gpui_perf` headless scroll scenarios dispatch real wheel events with the
pointer over the content. Linux, release build, retained views on, 300 frames:

| Scenario | Layers off | Layers on |
|---|---|---|
| `scroll-child-view` | 0.619 ms, 8.83M instructions | 0.066 ms, 0.88M instructions |
| `scroll-same-view` | 0.578 ms, 8.59M instructions | 0.232 ms, 3.19M instructions |
| `scroll-uniform-list` | 0.397 ms, 5.36M instructions | 0.137 ms, 1.62M instructions |
| `scroll-list` | 0.377 ms, 5.11M instructions | 0.147 ms, 1.70M instructions |

The chat scenarios, macOS (Apple silicon), release build, retained views on,
300 frames:

| Scenario | Layers off | Layers on |
|---|---|---|
| `chat-scroll` | 0.250 ms, 3.58M instructions | 0.090 ms, 1.22M instructions |
| `chat-scroll-no-button` | 0.249 ms, 3.56M instructions | 0.093 ms, 1.21M instructions |

`chat-scroll` scrolls a transcript of 160 messages, each body a view of its
own holding paragraphs, code blocks and tables, away from its end and back,
while the view holding it follows the end, asks whether the list is at its
end and fades a "back to bottom" button in and out. Before the list's layer
told a read of only that from a read of its offset, and painted only the rows
shown when the view was notified, the layer was composited on none of these
frames (0.262 ms, 3.60M instructions), and `chat-scroll-no-button` on 80 % of them (0.137 ms, 1.81M).

Every scenario composited its layer on all scrolled frames. In the list
scenarios the pointer stays over the rows, 40 px of wheel a frame over rows
about 48 px tall: most frames render one row entering the overscan and two
whose hover changed, and, scrolling back, one row showing again after it
left the viewport. Drawn without a layer, each frame renders about 20.

GPUI Kit's Button story, measured before the overscan was raised to two
viewports, at 145 Hz, with one wheel event per frame, over two 12-second runs
of each (a later run against `main`, same setup: 27-28 % and 1.53-1.57 ms on
`main`, 18-19 % and 0.92-0.93 ms with layers):

| | Process CPU | Main-thread draw per frame |
|---|---|---|
| Layers off | 23–41 % (mostly 26–33 %) | 1.26–2.38 ms (mostly 1.5–1.9) |
| Layers on | 17–30 % (mostly 18–22 %) | 0.83–1.73 ms (mostly 0.9–1.1) |

With layers on, 89 % of frames were composited. The rest were repaints as the
scroll crossed the overscan.

Most of what remains in the Button story is GPUI Kit, not the scrolled content.
GPUI Kit writes window-wide state during render (`sync_focused_input_registry`,
`GlobalState`, `SelectionStateRegistry`), which rebuilds the ancestor views on
every scrolled frame.

Refresh workloads can be measured with the same release build using
`GPUI_PERF_STREAM_MS`, the showcase workspace's quote interval in milliseconds
(default 16), and switching only `GPUI_SCROLL_LAYERS`. For example:

```sh
GPUI_PERF_STREAM_MS=133 cargo run -p gpui_perf --release -- --auto --only WorkspaceScroll --retention on --frames 600
GPUI_PERF_STREAM_MS=133 GPUI_SCROLL_LAYERS=0 cargo run -p gpui_perf --release -- --auto --only WorkspaceScroll --retention on --frames 600
```
