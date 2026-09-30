# Scroll layers: compositing scrolled content from cached tiles

Status: implemented on Linux/wgpu (2026-09-30), list layers included. The
measurements are in [`docs/scroll-layers.md`](../../scroll-layers.md), which
also describes how a list's layer keeps its rows apart (their input records,
hovers and tiles), beyond what §8 says. Tuned on `gpui_perf`'s scroll scenarios: the overscan is two
viewports (`OVERSCAN_VIEWPORTS`), which cut the mean scrolled frame by 21-44 %
against one; tile size (256, 512, 1024) made no measurable difference, so it
stays 512; a repaint margin of 0.1 or 0.5 was worse than a quarter.

## 1. Why

Scrolling is where gpui-fast gains least. Measured on Linux (wgpu, 145 Hz,
one real wheel event per frame, pointer over the content), both GPUI Kit's
Button story and `gpui_perf`'s showcase page cost **20–35 % process CPU and
1.1–2.0 ms of main-thread draw per frame**. The breakdown for the Button
story:

| Share | Cause |
|---|---|
| ~60 % | The story view only **moved**, but a retained view is reused only at identical bounds (`retained_context_matches`), so it goes through `build_at_retained_layout`: render, layout, prepaint and paint again. |
| 20–35 % | Ancestors (`Gallery` with its sidebar, `Root`, `WindowState`, title bar) are rebuilt because GPUI Kit writes window-wide state during render (`sync_focused_input_registry`, `GlobalState`, `SelectionStateRegistry`), which defeats splicing. **Out of scope here**; tracked separately. |
| rest | Event dispatch, the scroll container's own view, frame overhead. |

UIKit, Flutter and Chrome make scrolling cheap the same way: content is
rasterized once into cached layer textures, and a scroll only changes the
offset at which a compositor draws them. This design brings that model to
gpui-fast.

## 2. Goals and non-goals

**Goals**

1. A frame in which only scroll offsets changed does **not** render, lay out,
   prepaint or paint the scrolled content, and does not re-rasterize it: it
   composites cached tiles at the new offset.
2. Covered patterns, all in the first version:
   - **A** — a scrolling `div` whose content is a child view
     (`div().overflow_y_scroll().child(view)`), e.g. GPUI Kit's gallery.
   - **B** — a scrolling `div` whose content is plain elements in the same
     view as the div.
   - **C** — virtual lists: `uniform_list` and `list`.
3. **Pixel-identical output.** Every frame drawn with layers equals the frame
   drawn without them, byte for byte, on the wgpu backend. Where a layer
   cannot guarantee that, the container falls back to today's path.
4. No change to what application code observes: every position handed to
   application code (event positions, `Hitbox` bounds, element bounds) stays
   in window coordinates and is correct when it is observed.
5. The upstream-sync rules hold: logic in `fast/`, hooks in upstream files.

**Non-goals (v1)**

- Metal and DirectX. The core is backend-neutral; only wgpu consumes layers
  in v1. macOS and Windows keep today's path, decided at compile time.
- Scrolling transformed or rotated content, nested scroll layers (an inner
  scroll container inside a layer is painted into the outer layer and
  invalidates it when it scrolls; see §6.6).
- Fixing GPUI Kit's writes during render (the 20–35 % ancestor share).
- Horizontal and vertical scrolling are both supported; momentum and
  scroll physics are unchanged (they are GPUI's and the platform's).

**Success criteria**

- Button story, continuous wheel scroll, 145 Hz: main-thread draw per frame
  ≤ 35 % of today's (≤ 0.45 ms from 1.3 ms), excluding the ancestor share
  caused by GPUI Kit.
- `gpui_perf` gains real-wheel scroll scenarios (§9); their scroll frames
  drop by ≥ 2× in instructions.
- `--verify`, the oracle test and the new pixel tests pass with layers on.
- No scenario regresses by more than 3 % in instructions with layers on.

## 3. Concepts

- **Scroll layer** (`fast::layers::Layer`): the cached content of one scroll
  container: a content-space scene, its tiles, and the records needed to
  composite it and route input into it. Keyed by the scroll container's
  `GlobalElementId`.
- **Content space**: the layer's own coordinate system, in device pixels:
  window space at the moment the content was last painted. It does not move
  when the container scrolls.
- **Viewport**: the scroll container's clip rect in window space (its
  `overflow_mask`).
- **Painted region**: the part of content space the layer has painted:
  viewport plus **overscan** (default: one viewport extent on each side along
  the scroll axes, clamped to the content). Content outside it was culled.
- **Tile**: a square of content space, 512 × 512 device pixels, rasterized
  into its own texture.
- **Translation** `T`: how far the content has moved since it was painted,
  `T_now − T_paint`, a whole number of device pixels (scroll offsets are
  snapped, §5.3). Window position = content position + `T`.
- **Scroll-only frame** for a layer: nothing the layer's content depends on
  changed except the container's scroll offset (§6.2).

## 4. Architecture

```
            gpui (core, fast/layers/)                       gpui_wgpu (fast/layers/)
 ┌────────────────────────────────────────────┐        ┌─────────────────────────────┐
 │ promotion & eligibility  (layers/policy)   │        │ TileCache: textures, LRU,   │
 │ content-space painting   (layers/paint)    │ Scene  │ VRAM budget                 │
 │ scroll-only detection    (layers/invalid)  │ ─────► │ raster: tile passes, baked  │
 │ input routing & rebuild  (layers/input)    │ .layers│ clear colour, tile globals, │
 │ tile diffing             (layers/tiles)    │        │ tile-sized path intermediate│
 │ virtual-list rows        (layers/lists)    │        │ composite: poly-sprite quads│
 └────────────────────────────────────────────┘        └─────────────────────────────┘
```

**Per frame, for each scroll container:**

1. **Decide** (policy): is there a live layer, is it eligible (§6.5), is this
   a scroll-only frame for it?
2. **Scroll-only frame:** skip the content. Insert the tile quads into the
   main scene at the current translation. Translate the layer's input records
   (§7). If the painted region no longer covers viewport + a margin, paint the
   missing strip into the layer (§5.4).
3. **Content changed:** paint the content into the layer's content-space
   scene over the whole painted region (culling against the painted region,
   clipping against nothing but content bounds), diff tiles (§5.5), mark dirty
   tiles, emit the composite.
4. **Not eligible:** today's path. The layer is dropped after a grace period
   (§6.5).

The renderer rasterizes dirty tiles in passes before the main pass, then
draws each composite as textured quads in the main pass.

## 5. Painting and rasterizing

### 5.1 The core ↔ renderer contract

Two things cross from the core to the renderer, both through `Scene`.

**Tile quads are ordinary primitives.** Each visible tile of a layer is
inserted into the main scene as a `PolychromeSprite` whose
`tile.texture_id` lies in a reserved range that no atlas allocates (one id
per layer) and whose `tile.tile_id` packs the tile coordinate:

```rust
// fast/layers/scene.rs, exported from gpui.rs
pub const LAYER_TILE_TEXTURE_BASE: u32 = 0xF000_0000;
pub fn layer_tile_texture_id(layer: LayerKey) -> AtlasTextureId; // kind Polychrome, one per layer
pub fn layer_tile_id(tile: TileCoord) -> TileId;                  // the tile, packed
pub fn decode_layer_tile(texture: AtlasTextureId, tile: TileId) -> Option<(LayerKey, TileCoord)>;
```

The sprite's `bounds` is the tile's window rectangle (whole device
pixels, the tile's full size), its `content_mask` the viewport, opacity 1,
no corner radii, and `tile.bounds` is `(0, 0, tile_size, tile_size)`. Draw
order, clipping, sorting and batching are therefore the scene's own. A
renderer that sees a polychrome batch whose texture id decodes to a layer
tile binds that tile's texture instead of an atlas texture.

**Layer content rides along.** `Scene` gains one field (an exception to "no
new public API", see §11):

```rust
pub struct Scene {
    // ...existing fields...
    pub layers: SceneLayers,           // fast/layers/scene.rs
}

pub struct SceneLayers {
    pub frames: Vec<LayerFrame>,       // one per layer composited this frame
}

pub struct LayerFrame {
    pub key: LayerKey,                 // stable while the layer lives
    pub generation: u64,               // bumps when the content scene is replaced
    pub background: Rgba,              // baked clear colour, opaque
    pub tile_size: u32,                // device px, 512
    pub content: Rc<Scene>,            // content space, the whole painted region
    pub dirty_tiles: Vec<TileCoord>,   // tiles whose content changed this generation
}

impl LayerFrame {
    /// The primitives of `content` that intersect `tile`, translated into the
    /// tile's space (origin at the tile's top-left), sorted and batchable.
    pub fn tile_scene(&self, tile: TileCoord) -> Scene;
}
```

- `content` is shared (`Rc`) and only replaced when the layer repaints, so a
  scroll-only frame costs nothing to hand over.
- The renderer rasterizes, before the main pass, every tile in
  `dirty_tiles` plus every tile the main scene composites that it does not
  hold (evicted, device lost, new window), from `tile_scene`. No state flows
  back from the renderer to the core.
- A renderer that does not support layers never receives any: the core
  enables layers only under `cfg(target_os = "linux")` in v1.

### 5.2 Background baking

Subpixel (LCD) text needs to know what it is drawn over. Linux's cosmic-text
recommends subpixel rendering, so a transparent tile cannot reproduce it.

When a layer is (re)painted, the core looks up what the main scene has
already painted under the viewport (a `BoundsTree` query over earlier
orders). The layer is eligible only if the topmost primitive under the whole
viewport is **one opaque, solid-colour quad that covers the viewport
entirely** (no gradient, no pattern, no corner radius reaching into the
viewport, opacity 1, window background opaque). Its colour becomes
`background` and every tile is cleared with it. Otherwise the container uses
today's path. A background colour change (theme switch) repaints the layer.

### 5.3 Content space and snapping

- The content is painted as today, in window space, at the current scroll
  offset. Its primitives go into the layer's own `Scene` (the window's scene
  is swapped for the layer's while the container paints its children) and
  are stored translated by `−T_paint`, the layer's translation when it was
  painted, so content space is fixed while the container scrolls. Composite
  places tile `(x, y)` at `(x·tile, y·tile) + T_now`.
- Scroll offsets are snapped to whole device pixels at the sites that apply
  them (div prepaint `with_element_offset(scroll_offset)`, list and uniform
  list item origins), **whenever layers are enabled** (the Linux gate), with
  or without a layer, so the layer path and today's path place content on
  the same grid. Offsets that are already whole device pixels are unchanged.
- Glyph origin quantization (`paint_glyph`, `paint_emoji`,
  `fast::glyphs`) is made translation-invariant: today `round_half_toward_
  zero`, `fract` and `trunc` misplace glyphs at negative device coordinates.
  The replacement, `fast::glyphs::quantize_origin`, returns exactly today's
  result for non-negative coordinates (everything visible on screen) and
  shifts consistently for negative ones (overscan above or left of the
  window), so a glyph painted in overscan lands where a direct repaint would
  put it once scrolled into view.
- The renderer draws `tile_scene`s with globals whose `viewport_size` is the
  tile size. Primitives are translated on the CPU (the shaders have no
  offset uniform; all position-dependent shading is relative to
  `bounds.origin`, so whole-pixel translation is exact).

### 5.4 Culling: the painted region

GPUI culls while painting: `Scene::insert_primitive` drops primitives outside
their content mask, and `paint_line` skips glyphs outside the mask. A layer
separates the **cull region** (painted region, larger) from the **clip mask**
(what is visible). Inside a layer:

- the scroll container pushes the painted region instead of the viewport as
  its overflow mask (hook at the two `with_content_mask(overflow_mask)` sites
  in `Interactivity::prepaint`/`paint`), so nested masks and culling work in
  the larger region;
- primitives recorded in the content scene carry that mask; the composite's
  `content_mask` is the viewport, so the GPU clips correctly;
- hitboxes are inserted with the **viewport** intersected, so hit testing is
  unchanged (§7).

When a scroll exposes content outside the painted region minus a margin
(one quarter of the overscan), the layer paints the missing part: for A and B
the content is painted again over a painted region re-centred on the viewport
(a normal content-changed frame, §4 step 3; tiles that come out identical are
not re-rasterized, §5.5). For C only the newly exposed rows are rendered
(§8). This is the one expensive frame per overscan's worth of scrolling.

### 5.5 Tile diffing

After a content paint, each tile's primitives (those intersecting it, in
drawing order) are hashed. Tiles whose hash matches the previous generation
keep their texture; the rest are dirty. A hover highlight therefore
re-rasterizes one or two tiles, not the layer.

### 5.6 Renderer: tiles on wgpu

- `gpui_wgpu/src/fast/layers/`: `TileCache` (per window) maps
  `(LayerKey, TileCoord)` to a texture of `surface_format`, usage
  `RENDER_ATTACHMENT | TEXTURE_BINDING | COPY_SRC`, from a pool.
- Raster passes run in `fast::frame::record` before the main pass, in the
  same encoder: per dirty tile, clear to `background`, bind tile globals
  (own uniform buffer, `viewport_size = tile`, same gamma and
  `premultiplied_alpha` as the frame), draw the tile's batches with the
  existing pipelines. Instance data for tile batches is appended to the frame's
  single staging upload.
- Paths are never composited from tiles: `fs_path_rasterization` derives its
  antialiasing from `dpdx`/`dpdy`, which pair pixels in 2×2 quads, so a path
  rasterized into a tile and moved by an odd number of device pixels differs
  from a direct draw by one level on some edge pixels. Content that paints a
  path makes its container ineligible (§6.5); the renderer's tile path
  support (a tile-sized intermediate) exists only so its pixel tests can pin
  this down.
- Composite: a polychrome batch whose texture id decodes to a layer tile
  (§5.1) is drawn with the existing polychrome-sprite pipeline, the tile's
  texture bound through `BindGroupCache::texture` in place of the atlas
  texture.
  Whole-pixel bounds equal to the tile size sample texel centres, so the copy
  is exact.
- Budget: 64 MB of tile textures per window by default; least recently
  composited tiles are evicted first; a layer whose composited tiles alone
  exceed the budget is demoted (§6.5). Textures are released on window
  resize and scale-factor change along with the layer.

## 6. Invalidation and eligibility

### 6.1 What a layer records

A layer records, for its content, everything a retained view records today
(fast/retained.rs): entity and global dependencies, state versions, hovers,
layout keys, element states touched, rem size, text style, opacity, content
mask, plus the painted region, the translation it was painted at, the snapped scroll offset,
and the background.

### 6.2 Scroll-only detection

Wheel scrolls are invisible to the dependency system today: the div and list
wheel listeners mutate the offset directly and notify the owning view, which
looks like any other change. The design adds a distinct signal:

- `fast::layers::note_scrolled(container_id)`, called from the div wheel
  listener (`paint_scroll_listener`), the list wheel listener
  (`StateInner::scroll`), and programmatic scrolls (`ScrollHandle::set_offset`,
  `scroll_to_*`, `ListState::scroll_to*`, `UniformListScrollHandle::scroll_to_*`).
  It replaces `invalidate_retained_subtrees` for scrolls inside layers and
  records which container moved.
- **Offset reads** are recorded: the offset getters (`ScrollHandle::offset`,
  `max_offset`, `top_item`, `bottom_item`, `logical_scroll_top`,
  `scroll_px_offset_for_scrollbar`, uniform list equivalents) note a read of
  the container's offset. A view whose **render** read the offset of a
  container is invalidated by that container's scrolls like any other
  dependency.
- A frame is scroll-only for a layer when the owning view is dirty only
  because of `note_scrolled` for this container (not notified otherwise, no
  own dependency changed, no hover changed), and nothing inside the content
  changed (child views clean, no dependency of the content changed).

### 6.3 Patterns A and B share one path

The layer sits at the scroll `div`, where it prepaints and paints its
children. Whatever the children are — a child view (A) or plain elements
(B) — on a scroll-only frame the div does not prepaint or paint them; it
carries last frame's non-scene records for them (§7) and inserts the tile
quads.

- **A:** the owning view is thin (scroll div, scrollbar) and is rebuilt as
  today; the child view element requests its layout through retained layout
  reuse (it is clean), and is never prepainted or painted.
- **B:** the owning view is rendered and laid out as today (the wheel
  notified it); only the prepaint and paint of the content are skipped. The
  owner's render cost remains in v1 (see §12).
- **Inside a layer, nested retained views record nothing and reuse nothing**:
  the layer is their retention. When the layer repaints (content changed),
  everything inside it is built. Views outside the layer are unaffected.

### 6.4 When the content counts as unchanged

For the layer at scroll container `c`, a frame is scroll-only when:

- every view whose element is inside the content is clean and its recorded
  dependencies are unchanged (`reusable_retained` would accept it), and
- the owning view of `c` is either clean, or dirty only through
  `note_scrolled(c)` with no own dependency changed and its render did not
  read `c`'s offset, and
- no hover recorded by the content changed (hit-tested with the translated
  hitboxes), and
- the scroll div's own style, bounds size, content mask, opacity, text style
  and background are unchanged.

### 6.5 Eligibility, promotion and demotion

A scroll container gets a layer when all hold:

- layers enabled (Linux, view retention on, not refreshing, no drag, a11y
  inactive, inspector not picking);
- it is a `div` with `overflow` scroll on some axis, a `uniform_list` or a
  `list`;
- it has scrolled on 2 consecutive frames (promotion; a static container never
  pays for a layer);
- background baking succeeds (§5.2);
- its content has **no deferred draws, no anchored elements, no input handler
  (focused text input), no surfaces, no paths, no nested layer**, no view that requested
  an animation frame this frame. Any of these makes the container use today's
  path for the frame.

Demotion: a layer whose content changed on more than 50 % of the last 16
frames (spinners, streaming text), or that exceeds the tile budget, is
dropped and the container stays on today's path until it has been stable for
60 frames. A layer not composited for 120 frames is dropped.

### 6.6 Nested scroll containers

An inner scroll container inside a layer's content is painted into the outer
layer. Its scrolls are content changes of the outer layer (repaint + tile
diff). It never gets its own layer in v1.

## 7. Input: keeping window coordinates true

Application code sees window coordinates only. On scroll-only frames the
content's closures and element states are not re-run, so positions they
captured are stale by the translation delta since the layer's content was
last painted (`delta`). The rules:

1. **Hitboxes** of the content are stored in content space in the layer and
   emitted into the frame's hitbox list translated to window space (and
   intersected with the viewport) every frame. Hit testing, `is_hovered`,
   `should_handle_scroll` and cursor styles are therefore correct.
2. **Wheel events** are dispatched normally. Wheel listeners inside the
   content (nested scroll containers) use only `should_handle_scroll` and the
   hitbox id, which rule 1 keeps correct.
3. **Every other input event** whose position lies in the viewport, and every
   keyboard event while focus is inside the content, first **rebuilds** the
   layer's content at the current offset when `delta ≠ 0` (a normal content
   paint), and only then dispatches. Closures and element states then hold
   current window coordinates, so drag offsets, `DragMoveEvent.bounds`,
   keyboard `ClickEvent` bounds, `InteractiveText` indices, context-menu
   positions and IME bounds are exact. The cost is one rebuild on the first
   pointer or key event after a scroll.
4. **Hover changes during a scroll** (content moving under a still pointer):
   detected by the translated hitboxes; the affected views are notified as
   today, which makes the next frame a content-changed frame for the layer
   (tile diff keeps it to the changed tiles).
5. **Tooltips** requested inside the content are dropped when the layer
   translates (a scroll hides tooltips today as well).
6. **Accessibility**: layers are off while a11y is active.
7. **Element states that store absolute bounds** (`TextLayout.bounds`,
   `ScrollHandle.child_bounds`, `scroll_anchor.last_origin`): application
   code reading them outside input dispatch on a scroll-only frame would see
   stale values. `ScrollHandle::bounds_for_item` and `child_bounds` are
   translated on read by the layer (hook in the getter); `TextLayout` bounds
   are only read inside the element's own listeners (rule 3 covers them).

## 8. Virtual lists (pattern C)

- The list's owning view calls the row closure (`render_items` / `render_item`)
  inside `view.update`, so reads made by the closure are the owning view's
  dependencies. On a scroll-only frame (§6.2) rows already painted in the
  layer are unchanged by construction.
- `uniform_list`: the hook before `(self.render_items)(visible_range)` splits
  the range into rows the layer holds and rows it does not; only the missing
  rows are rendered, laid out and painted into the layer at their content
  positions (`padded.origin + (0, h × ix)` in content space). The measured item
  (`measure_item`) is skipped on scroll-only frames; its size is kept.
- `list`: the hook in `layout_items` renders only rows missing from the
  layer; row positions come from the `SumTree` heights, which a scroll-only
  frame does not change.
- The layer's painted region for lists is measured in rows: visible rows plus
  overscan rows. Rows scrolled out beyond the overscan are dropped from the
  layer's content scene; tiles covering only dropped rows are released.
- Decorations of `uniform_list` (which take the scroll offset) are painted
  outside the layer, as today.
- A content-changed frame (the owner was notified for another reason)
  repaints the visible and overscan rows and diffs tiles.

## 9. Verification

- **Pixel tests** (gpui_wgpu, surfaceless device): render the same scene
  directly and through layers (tile raster + composite) into textures with
  `COPY_SRC`, read back, compare bytes. Cases: quads, borders, shadows,
  gradients, paths (tile-sized intermediate), mono/subpixel/poly sprites,
  underlines, content masks crossing tile edges, primitives spanning tiles,
  negative content coordinates. Skipped when the adapter lacks
  `DUAL_SOURCE_BLENDING` for the subpixel cases.
- **Scene oracle** (gpui, test platform): extend `fast/tests/oracle.rs` with a
  layer oracle — two windows through the same random history (wheel scrolls,
  programmatic scrolls, hovers, clicks, key input, content mutations, resizes,
  list splices); one with layers, one without; every frame, the layer
  window's main scene with composites expanded (tiles replaced by their
  content primitives translated and clipped) must equal the other window's
  scene; hit-test results for random points must be equal; every dispatched
  event's listener-observed positions must be equal.
- **`gpui_perf --headless --verify`** runs layer scenarios with both paths.
- **Benchmarks**: `gpui_perf` scroll scenarios are changed to dispatch real
  `ScrollWheelEvent`s with the pointer over the content (today they call
  `set_offset`, which skips dispatch and hover), and gain a GPUI-Kit-like page
  (content child view, pattern B page, uniform list, list).
- **Trace**: `GPUI_FAST_TRACE`-style counters become `LayoutStats` fields:
  `layer_frames_composited`, `layer_frames_repainted`, `tiles_rasterized`,
  `layer_rebuilds_for_input`, `layers_demoted`.

## 10. Milestones and parallel work

The contract in §5.1 and the recording in §6.1 are fixed first (M1), then
work splits into streams that touch disjoint files:

| Milestone | Stream | Content | Depends on |
|---|---|---|---|
| M1 | core-contract | `fast/layers/scene.rs` types and tile-texture ids, `Scene.layers` hook, `LayerFrame::tile_scene`, translation-invariant glyph quantization, Linux gate, per-window `WindowLayers` skeleton, stats fields | — |
| M2 | renderer | gpui_wgpu `fast/layers/`: tile cache, raster passes, tile globals, tile path intermediate, composite, budget, surfaceless pixel test harness + pixel tests | M1 |
| M3 | paint | core `fast/layers/paint.rs`, `record.rs`, `background.rs`, `tiles.rs`: scene swap and content-space recording, cull/clip split hooks, scroll-offset snapping, background baking, tile diffing, composite insertion | M1 |
| M4 | invalidation | `fast/layers/invalidate.rs` + `policy.rs`: `note_scrolled`, offset-read hooks, scroll-only detection, patterns A and B, promotion/demotion | M1 |
| M5 | input | `fast/layers/input.rs`, `reuse.rs`: carrying non-scene records on scroll-only frames, hitbox translation and viewport clipping, rebuild-before-input, tooltips, getter translation | M1, M3 |
| M6 | lists | `fast/layers/lists.rs`: uniform_list and list partial row rendering | M3, M4 |
| M7 | verification | layer oracle, `gpui_perf` real-wheel scenarios + GPUI-Kit-like page, `--verify` coverage | M1 (grows with each stream) |
| M8 | integration | end-to-end on the Button story, tuning (tile size, overscan, budget), docs (`docs/scroll-layers.md`, architecture) | all |

M2, M3, M4 and M7 run in parallel after M1; M5 and M6 start when their
dependencies land. Each stream works on its own branch off the M1 branch
and merges through a PR with `script/check-upstream` clean.

## 11. Upstream impact and exceptions

- New files only under `crates/gpui/src/fast/layers/`,
  `crates/gpui/src/fast/tests/layers*.rs`, `crates/gpui_wgpu/src/fast/layers/`.
- Hooks (one line each, naming `crate::fast::layers::…`): the div overflow-mask
  sites, the div and list scroll listeners, the offset getters and
  programmatic scroll methods, `with_element_offset(scroll_offset)`, list and
  uniform list row rendering, the paint-time hitbox insertion, input dispatch
  (`dispatch_event`), `Scene` construction.
- **API exception to decide:** `Scene.layers` is a new public field, because
  renderers live in other crates and read `Scene` fields directly. `Scene` is
  renderer-facing (`#[expect(missing_docs)]`); the field would be listed in
  `docs/upstream-sync.md` under "Where our API differs from upstream's".

## 12. Risks

| Risk | Mitigation |
|---|---|
| Pixel mismatch between tiles and direct drawing (blending, snapping, subpixel variants) | Pixel tests first (M2); non-negative content space; snapped offsets; fall back on any unsupported background. |
| Stale absolute positions reach application code | Rebuild before any non-wheel input (§7 rule 3); ineligible content types (§6.5); oracle compares listener-observed positions. |
| Layers churn on animated content | Demotion heuristic (§6.5), counted in stats. |
| VRAM use | 64 MB budget per window, LRU, demotion. |
| Pattern B keeps render cost when a scrollbar reads the offset | Measured in M8; a later step can make scrollbars read the offset through the layer's composite without re-rendering the owner. |
| Scope: a large change across core and renderer | Milestones land separately behind the Linux gate; each is verified by the oracle and pixel tests before the next depends on it. |
