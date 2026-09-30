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

- Linux, on the wgpu renderer, and macOS, on the Metal renderer. Everywhere
  else layers are compiled out (`fast::layers::COMPILED`), and nothing
  changes.
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
  (`fast::layers::tiles`).
- **Bypass.** The frame is drawn exactly as it would be without layers. This
  happens when a layer cannot guarantee identical pixels or current window
  coordinates, for example:
  - the background under the viewport is not one opaque solid quad;
  - the content has deferred or anchored elements, a focused input, or
    surfaces;
  - the content changes on most frames (demotion);
  - paths in the content are covered by something drawn after them.

Paths are never rasterized into tiles, because odd translations change their
antialiasing. When nothing covers them, they are drawn over the tiles in the
frame.

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
