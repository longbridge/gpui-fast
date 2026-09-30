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

- Linux, on the wgpu renderer. Everywhere else layers are compiled out
  (`fast::layers::COMPILED`), and nothing changes.
- Scrolling `div`s (`overflow_x_scroll` / `overflow_y_scroll`), whether their
  content is a child view or plain elements in the same view.
- Not yet `uniform_list` or `list`: `fast::layers::lists::LIST_LAYERS` is off,
  and the list layer tests are `#[ignore]`d. As built, list layers do not pay
  off, for three reasons, each enough on its own:
  - Rows that take input (hover, click, cursor) are never given a layer. A
    list layer cannot carry a row's hitboxes, listeners and dispatch nodes
    through a composited frame.
  - Hovers are recorded for the whole layer. Rows moving under a still pointer
    change the hovered row on most frames, and each change repaints every
    held row.
  - A frame that adds rows rebuilds the whole content scene and re-hashes
    every tile, which costs about three times drawing without a layer.
  Making list layers pay off needs per-row input records, per-row hover
  checks, and content that can be assembled row by row.
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
- Before any input other than the wheel reaches a layer that has moved since
  it was painted, the layer is repainted (`fast::layers::input`). Listeners,
  element state and anything handed to application code therefore hold
  current window coordinates.

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
| `scroll-child-view` | 0.618 ms, 8.83M instructions | 0.068 ms, 0.88M instructions |
| `scroll-same-view` | 0.599 ms, 8.59M instructions | 0.226 ms, 3.19M instructions |
| `scroll-uniform-list` | 0.393 ms, 5.35M | 0.399 ms, 5.35M (no list layers) |
| `scroll-list` | 0.377 ms, 5.09M | 0.384 ms, 5.09M (no list layers) |

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
