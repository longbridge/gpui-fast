# Scene damage and adaptive CPU rendering — design

## Goal

Retained views, retained layout and scroll layers removed most of the
*logical* work of a frame that changes little. What is left is *physical*:
every frame, however small its change, still acquires a swapchain image,
records a render pass over the whole window and presents it on the GPU. A
blinking caret, a hover, a ticking clock or one quote changing in a table
wakes the GPU for a whole-window frame.

This work adds two things:

1. **Scene damage** (core, every platform). Each finished scene carries the
   rectangles, in device pixels, where it can draw different pixels than the
   scene before it. It is computed from the two scenes, so it is exact: a
   pixel outside the damage is drawn by the same primitives, in the same
   order, with the same inputs, in both scenes.
2. **Adaptive CPU rendering** (Linux, wgpu renderer, Wayland and X11). A
   frame whose damage is small is drawn on the CPU, into a frame kept in
   memory, only inside the damage, and presented without the GPU (`wl_shm`
   on Wayland, `PutImage` on X11). Frames that change much of the window, or
   bursts of frames whose CPU drawing costs too much, are drawn on the GPU as
   before; once frames pause, small changes go back to the CPU.

The frame drawn from scratch on the GPU stays the specification: the CPU
rasterizer reproduces the wgpu shaders' arithmetic and blending, and is
verified against the GPU renderer pixel by pixel.

## 1. Scene damage — `crates/gpui/src/fast/damage.rs`

### Contract

```rust
pub struct SceneDamage {
    pub frame: u64,                       // this scene's number in its window, from 1
    pub since: u64,                       // the scene `rects` compare with; 0 = none, all damaged
    pub rects: Vec<Bounds<DevicePixels>>, // where pixels may differ from scene `since`
    pub changed_primitives: usize,        // primitives in either scene not matched in the other
}
```

`Scene.damage` holds it (a hook field like `Scene.layers`). It is filled in
`Window::draw`, right after the new frame is finished, from the new scene and
the previous one (`crate::fast::damage::finish_frame(self)`). The
renderer compares `since` with the number of the scene it last drew: if they
differ (a scene was drawn but not presented, or nothing was drawn yet), it
must treat the whole window as damaged.

### Why the diff is exact

A pixel's colour is a function of the sequence of primitives that cover it,
taken in drawing order: by `(order, kind)`, then by position within the
kind's vector. Two primitives of one scene that overlap have different
`order`s, except inside a `paint_layer` (a text line), where all primitives
share the layer's order and keep their paint order.

So the diff walks each kind's vector, both sorted by `order`, one order
group at a time:

- a group equal in both scenes element for element (the usual case, a
  `memcmp`) damages nothing;
- otherwise the group's elements are matched by value (a hash multiset, with
  equality on the value, not the hash). Unmatched elements of either scene
  are damaged. The matched elements are then compared in sequence: those
  whose relative position changed are damaged too, which covers the paint
  order inside a layer.

What a primitive can touch is its bounds clipped by its content mask, rounded
out to whole device pixels, widened where a primitive draws outside its
bounds: shadows by three blur radii (inset shadows by their element bounds),
transformed sprites by their transformed bounds. Surfaces (macOS video) are
always damaged. Scroll layer tile sprites are equal as primitives when their
tile did not change; a tile listed in its layer's `dirty_tiles` under a new
`generation` is damaged.

Damaged rectangles are merged as they are added: a rectangle within 8 px of
one already kept is unioned into it, and past 16 rectangles the two whose
union adds the fewest pixels are merged, so changes spread over the window
stay apart. A damage that covers more than half the window is reported as one
rectangle over the whole window.

Atlas content is outside the scene: an atlas tile freed and allocated again to
another image keeps its id. The renderer, which owns the atlas, adds the
sprites over tiles written since its last frame to the damage (see 3).

### Statistics

`LayoutStats` (test-support) gains `damage_frames`, `full_damage_frames`,
`damaged_pixels`, `window_pixels`, `small_damage_frames` (damage at most a
sixteenth of the window), `changed_primitives` and `damage_time`, so
`gpui_perf` reports how much of each frame actually changes.

## 2. CPU rasterizer — `crates/gpui_wgpu/src/fast/cpu/raster.rs`

Draws a finished `Scene` into a `Canvas` (premultiplied `0xAARRGGBB` `u32`s,
the layout of `wl_shm` `ARGB8888` and of a 32-bit X11 `ZPixmap`), only inside
the given regions, reproducing `shaders.wgsl`:

- quads (solid, linear gradients in sRGB and Oklab, slash pattern,
  checkerboard; rounded corners; borders, solid and dashed), shadows (drop
  and inset, blurred), underlines (straight and wavy), monochrome sprites
  (with contrast and gamma correction, transformations), subpixel sprites
  (dual-source blending emulated), polychrome sprites (grayscale, opacity,
  rounded corners), paths (4× MSAA coverage as the intermediate texture
  resolves it, then composited with the paths blend), and scroll layer tile
  sprites (their tile content drawn translated over the layer background);
- each primitive is blended into the 8-bit canvas as the GPU blends it into
  the 8-bit target, rounding after every primitive;
- regions are drawn on several threads in horizontal bands when they are
  large.

Surfaces cannot be drawn; `raster::can_draw` reports scenes that need the GPU.
It is verified against `WgpuHeadlessRenderer`: random scenes of every
primitive drawn by both, compared with a tolerance of one or two levels per
channel (the GPU's own interpolation and rounding), and region drawing
compared with a whole-frame draw.

## 3. Atlas mirror — `crates/gpui_wgpu/src/fast/cpu/atlas.rs`

`WgpuAtlas` keeps a CPU copy of every texture it uploads (hooks in
`upload_texture`, texture removal and `clear`), as uploaded, and a log of the
rectangles written. The CPU rasterizer samples sprites from it. The adaptive
renderer drains the log each frame and damages the sprites of the new scene
that sample a written rectangle.

## 4. Adaptive policy — `crates/gpui_wgpu/src/fast/adaptive.rs`

`WgpuRenderer::draw` first asks the policy whether to draw on the CPU.
A window renders on the CPU only once its platform installed a presenter
(`WgpuRenderer::set_cpu_presenter`). Per frame:

```text
region  = damage ∪ stale ∪ sprites over written atlas tiles
          (whole window if the canvas is missing, resized, or the damage is
           not relative to the last scene drawn)
burst   = this frame began within 50 ms of the last one ending
changed = area(damage)

GPU when: no presenter | GPUI_CPU_RENDER=0 | scene needs the GPU (surfaces)
          | area(region) > 8 Mpx
          | burst and changed > window / 16
          | burst and the canvas would be drawn whole, unless the burst's
            frames each changed at most window / 16 for 250 ms
          | the burst's CPU frames cost more than a quarter of its time,
            once it lasted 250 ms (the rest of the burst draws on the GPU)
          | composition (native surfaces) is active
else CPU: draw region into the canvas, present the canvas with the region
```

After a GPU frame, its damage becomes *stale* for the canvas (it no longer
shows the scene), merged like damage; the next CPU frame redraws it. After a
second of GPU frames only, the canvas is released and the next CPU frame
draws it whole. Two seconds after the last frame the canvas and the
presenter's buffers are released.

`GPUI_CPU_RENDER=0` turns CPU frames off; `GPUI_CPU_RENDER=always` draws every
frame it can on the CPU (for measurements). `GPUI_RENDER_STATS=1` logs, every
second, how many frames each path drew, their pixels, and the CPU time each
took.

## 5. Presenters — `crates/gpui_linux/src/fast/cpu_present/`

- **Wayland** (`wayland.rs`): a `wl_shm` pool of two or three buffers the size
  of the window (`ARGB8888`, or `XRGB8888` when opaque), released by the
  compositor's `wl_buffer.release`. A frame copies the region into a free
  buffer (the buffer is brought up to date with the regions changed since it
  was last attached), attaches it, `damage_buffer`s the region and commits.
  The frame callback `WaylandWindow::draw` requested before drawing is
  committed with it. (As built, the frames go on a synchronized subsurface,
  and the window surface is made transparent under it with
  `wp_alpha_modifier_v1`; see `adaptive-rendering.md`.)
- **X11** (`x11.rs`): `PutImage` of each region (MIT-SHM when available) into
  the window with the window's depth.

## Measuring

`gpui_perf` adds real-window small-update scenarios (caret blink, hover,
ticking clock, a few quotes changing) and the measurement script
`script/measure-adaptive` runs each with `GPUI_CPU_RENDER=0` and with the
default, sampling process CPU time, the GPU's utilization, power and
performance state (`nvidia-smi`), and the renderer's own statistics.
