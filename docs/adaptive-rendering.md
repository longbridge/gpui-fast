# Scene damage, adaptive CPU rendering and partial redraws

Retained views, retained layout and scroll layers make the *logical* work of
a frame proportional to what changed. The *physical* work was not: every
frame, however small its change, acquired a swapchain image, recorded a
render pass over the whole window and presented it on the GPU. A blinking
caret, a hover, a ticking clock or one quote changing in a table woke the GPU
for a whole-window frame.

Three mechanisms change that:

1. **Scene damage** (`crates/gpui/src/fast/damage.rs`, every platform). Each
   finished scene carries the rectangles, in device pixels, where it can draw
   different pixels than the scene before it (`Scene::damage`).
2. **Adaptive CPU rendering** (`crates/gpui_wgpu/src/fast/adaptive/`, the
   wgpu renderer: Linux on Wayland and X11). A frame whose damage is small is
   drawn on the CPU into a frame kept in memory, only inside its damage, and
   shown without drawing the scene on the GPU. Frames that change much of the
   window, and bursts of frames whose CPU drawing costs too much, are drawn on
   the GPU as before.
3. **Partial redraws** (`crates/gpui_apple/src/fast/partial.rs`,
   `crates/gpui_windows/src/fast/partial.rs`: Metal on macOS, Direct3D 11 on
   Windows). The GPU still draws every frame, but only inside its damage,
   into a frame it keeps; Windows also tells DWM which rectangles changed.

The design is in
[`superpowers/specs/2026-10-08-damage-and-adaptive-cpu-design.md`](superpowers/specs/2026-10-08-damage-and-adaptive-cpu-design.md).

## Scene damage

`Window::draw` diffs the scene it just finished against the scene drawn
before it (`fast::damage::finish_frame`) and fills `Scene::damage`:

- `frame`: the scene's number in its window, from 1;
- `since`: the number of the scene `rects` compare with, or 0 when every
  pixel may differ (the window's first scene, or a resize);
- `rects`: where this scene can draw different pixels than scene `since`;
- `changed_primitives`.

**It is exact.** A pixel's colour is a function of the primitives that cover
it, in drawing order: by `(order, kind)`, then by position within the kind.
The diff compares each kind's two vectors one draw order at a time. An order
whose primitives are byte-for-byte equal in both scenes damages nothing;
otherwise its primitives are matched by value, and the unmatched ones, and
the matched ones whose relative position changed (inside a text line's
layer, where primitives share an order and may overlap), are damaged. A pixel
outside the damage is drawn by the same primitives, in the same order, with
the same inputs, in both scenes. Also damaged:

- surfaces (video frames), always;
- scroll layer tiles that their layer lists as dirty in a new generation;
- overlapping paths whose batch split or joined: a batch's paths are
  rasterized together and each is composited from the result, so a
  primitive of another kind drawn between two paths changes their overlap
  without either path changing.

What a primitive can touch is its geometry clipped by its content mask,
rounded out to whole pixels: shadows reach three blur radii beyond their
bounds, transformed sprites cover their transformed bounds, paths a pixel
more. Rectangles within 8 px of each other merge; past 16, the two whose union
adds the fewest pixels merge, so changes spread over the window stay apart.
A damage over half the window is reported as the whole window.

Atlas content is outside the scene (an atlas tile freed and allocated again to
another image keeps its id), so the renderer adds the sprites over atlas
tiles written since its last frame to the damage.

`LayoutStats` (test-support) reports the damage: `damage_frames`,
`full_damage_frames`, `damaged_pixels`, `window_pixels`,
`small_damage_frames` (at most a sixteenth of the window),
`changed_primitives` and `damage_time`; `gpui_perf --headless` prints them.

## Adaptive CPU rendering

### A frame's path

`WgpuRenderer::draw` asks `fast::adaptive::draw` first. Per frame, the policy
(`fast::adaptive::policy`) takes:

```text
region  = damage
        ∪ stale (what GPU frames changed since the canvas was last drawn)
        ∪ sprites over atlas rectangles written since the last frame
          (the whole window when the canvas is missing, resized, or the damage
           is not relative to the scene drawn last)
burst   = this frame began within 50 ms of the last one ending
changed = area(damage ∪ atlas sprites)
```

and draws on the **GPU** when:

- the scene has surfaces, samples an atlas texture the CPU has no copy of,
  or has paths and the GPU rasterizes paths with other than 4 samples;
- the scene is not numbered (a composed window's replayed scene);
- it is the renderer's first frame;
- the frame is in a burst and changes more than a sixteenth of the window;
- the frame is in a burst and would redraw the canvas whole, unless the
  burst's frames have each changed at most a sixteenth of the window for
  250 ms (`CATCH_UP_AFTER`): then the CPU catches the canvas up once, and the
  frames after draw only what they change, so an animation that never
  pauses (a spinner) leaves the GPU;
- the region is larger than 8 Mpx;
- the CPU frames of the burst have cost more than a quarter of its time,
  once it lasted 250 ms: the rest of the burst draws on the GPU.

Otherwise it draws on the **CPU**: `fast::cpu::raster` redraws the canvas
inside the region, and the frame is shown with the region as its damage.
After a GPU frame, its damage becomes stale for the canvas. After a second of
GPU frames only, the canvas is released; the next CPU frame draws it whole.

### The CPU rasterizer

`fast::cpu::raster` draws a finished scene the way the wgpu renderer records
it: the batches of `Scene::batches` in order, each primitive's fragments the
pixels whose centers its geometry covers (1/256-pixel snapping, top-left
rule), shaded by a port of its fragment shader in `f32`, blended into the
8-bit frame as the pipeline's blend state blends it. Every shader branch is
covered: quads (all backgrounds, rounded corners, borders, dashed borders),
shadows, underlines, monochrome sprites with contrast and gamma correction
and transformations, subpixel sprites (dual-source blending), polychrome
sprites, paths (4× multisampling into an intermediate, as the GPU), and scroll
layer tiles (their content drawn over the layer background).

Two GPU behaviours are reproduced to stay within a level of the GPU: blending
into the 8-bit target is done in fixed point after rounding the fragment to
8 bits, and texture filtering rounds coordinates and weights to 1/256. Large
regions are drawn in bands on up to 8 threads; the pixels do not depend on
how a region is split.

How a fragment becomes an 8-bit level depends on the GPU
(`RasterParams::fragment_bits`, from the adapter's PCI vendor): NVIDIA
truncates it to 12 bits of fixed point before rounding, Intel (Mesa,
measured on Arrow Lake) to 16 bits; other vendors are taken as 16. Both are
reproduced exactly. On Intel two rounding edges remain: a rotated sprite's
filtering can land 3 levels apart, and a checkerboard background whose cell
edge falls exactly on a pixel center can have that edge a pixel over (the
GPU's division comes out just below the whole number). Neither shows within
a frame: a CPU frame and a GPU frame never draw parts of the same picture,
the canvas is redrawn whole when it changes hands.

Sprites are sampled from the atlas's CPU copy (`fast::cpu::atlas`), which the
atlas keeps as it uploads (8-bit coverage, or BGRA before the swizzle).

### Showing a CPU frame

Two ways (`GPUI_CPU_PRESENT`):

- **native**: the platform's presenter (`WgpuRenderer::set_cpu_presenter`),
  without the GPU:
  - **Wayland** (`gpui_linux/src/fast/cpu_present/wayland.rs`): `wl_shm`
    buffers on a synchronized subsurface over the window. Not on the window
    surface itself: the Vulkan WSI gives it explicit synchronization
    (`wp_linux_drm_syncobj_surface_v1`, NVIDIA and recent Mesa), and a
    `wl_shm` buffer on such a surface is a protocol error. A CPU frame
    attaches its buffer to the subsurface and commits the window surface
    without a buffer, carrying the frame callback. The next GPU frame hides
    the subsurface in the same commit as its own buffer. While the subsurface
    shows, the window surface is made fully transparent with
    `wp_alpha_modifier_v1` (set in the commit that shows the subsurface,
    reset in the GPU frame's): where the compositor makes the window
    translucent, the last GPU frame would otherwise show through. Without
    that protocol no presenter is installed, and CPU frames are shown by
    blit.
  - **X11** (`x11.rs`): the damage uploaded into the window with
    `PutImage`, through MIT-SHM for large rectangles. The GPU's presents
    (FIFO, through the Present extension) land at a later vblank, possibly
    over CPU frames uploaded since: the presenter selects `CompleteNotify`
    on the window, and when a GPU present completes after CPU frames were
    shown, the window is refreshed and that frame uploads again everything
    the CPU frames since the GPU presented uploaded. Without a compositing
    manager, `Expose` rectangles are uploaded with the frame the client's
    refresh presents (the same scene, without damage).
- **blit**: the region is uploaded to a texture that a tiny pipeline copies to
  the swapchain image (`fast::adaptive::blit`). It still presents through the
  GPU, but records no scene: one upload of the damage and one full-screen
  triangle.

A presenter may refuse a frame (Wayland's before the GPU presented the
window's first frame, or while the compositor holds all its buffers): the
frame is then drawn on the GPU; after 64 refusals in a row the CPU path is
off for the window.

Unset, `GPUI_CPU_PRESENT` means **native** where the platform installed a
presenter and blit elsewhere (`fast::adaptive::default_present_mode`). Native
costs less on every measure (see the results below): the application's GPU
time drops to nothing, and the compositor recomposites only the damage, where
a swapchain image presented by blit has it recomposite the whole window.

### Window opacity on Wayland

The native presenter shows CPU frames on a subsurface over the window surface,
which still holds the last GPU frame. Where the compositor makes the window
translucent (a window-opacity rule; Omarchy sets one on every window), that
frame showed through the CPU frames, and the window came out more opaque than
the rule. Hence the window surface's alpha multiplier above. At window
opacity 0.5 in the `Hover` scenario, native frames match blit ones (36
pixels apart by more than 4 levels in a 2388×1488 window, against 54,095
without the multiplier).

## Partial redraws on macOS and Windows

The Metal and Direct3D 11 renderers use the damage on the GPU. Each keeps a
**canvas**: a texture of the window's size, drawn into every frame and
holding the last frame drawn, with the number of its scene. A frame is
drawn whole (cleared, every batch drawn, as upstream draws it) or
**partially**: only inside its damage rectangles, over what the canvas
holds. The canvas is then copied into what the window presents.

### When a frame is partial

A frame is drawn whole when:

- there is no canvas of the window's size: the first frame, a resize, a lost
  device (Windows), a canvas drawn for the other alpha mode after the
  window's transparency changed (macOS);
- on Windows, the window's background appearance clears to another color
  than the canvas was drawn with (opaque white against transparent black,
  after `set_background_appearance`, which changes no scene);
- the scene is not numbered, or its damage is not relative to the scene the
  canvas holds (`since` is not the scene drawn last): a frame dropped for
  want of a drawable, a composed window's replayed scenes, frames drawn
  through `fast::composition`, which do not touch the canvas;
- an atlas tile was written since the last frame, as atlas content is
  outside the scene. macOS counts the writes per atlas
  (`fast::partial::AtlasWrites`), Windows across the process (so a write in
  any window draws every window whole once);
- the scene has surfaces (video frames);
- its damage covers more than half the window;
- on Windows, a graphics debugger is capturing (upstream's labeled loop then
  draws the frame).

A frame with nothing to draw, a scene drawn again (its number is the one
the canvas holds) or one whose damage relative to it is empty, is neither
drawn nor presented: the window keeps showing the frame presented last, a
copy of the canvas, so neither the application's GPU nor the compositor
does anything for it. On macOS this is decided before a drawable is taken.
Scroll layer tiles are still rasterized for it, as their cache follows the
scenes; the tiles it changes are off screen, as a changed tile on screen is
damage.

### Drawing and presenting

A partial frame keeps the canvas's content and, for each damage rectangle,
clears the rectangle to the color a whole frame is cleared to and draws
every batch of the scene, scissored to it. Primitives outside a rectangle
cost vertex work only.

- **Windows**: the rectangles are cleared with `ClearView` and drawn with a
  copy of the renderer's rasterizer state that has the scissor test on. The
  batches are walked once (`fast::frame::draw_scene_in`): each is bound once
  and drawn through every rectangle (`RSSetScissorRects` per draw), and each
  path batch is rasterized into its 4× MSAA intermediate once, scissored to
  the rectangles' union, so a frame costs one full-texture clear and resolve
  per path batch, as a whole frame does, however many rectangles. The
  canvas is lent to the renderer as its render target view, so paths, which bind it again after their
  intermediate pass, draw into it too. The whole canvas is then copied into
  the back buffer and presented with `Present1` and the damage as dirty
  rectangles, so DWM recomposes only those. (A flip-sequential back buffer
  holds a frame several presents old; the copy keeps every back buffer
  right whatever DXGI does with them.)
- **macOS**: each render pass loads the canvas and draws every damage
  rectangle, scissored to it: a small fill pipeline clears the rectangle,
  then the batches draw. Paths are rasterized into their intermediate
  texture whole and composited through the scissor. A full-screen triangle
  that reads the canvas texel for texel copies it into the drawable (the
  layer's drawables are framebuffer-only, so a blit cannot write them; only
  debug builds with `test-support` turn that off, for screenshots), and the
  drawable is presented as before. Core Animation has no partial present:
  the compositor takes the whole drawable.

`render_to_image` and the other headless paths draw as upstream does, into
their own targets.

### What it saves, and what it costs

- The GPU still wakes for every frame. A partial frame saves fragment work
  outside the damage and, on Windows, DWM's recomposition of the rest of the
  window.
- Every window keeps one more window-sized BGRA8 texture, the canvas: 4
  bytes a pixel, about 33 MB at 4K (3840×2160). It is kept while the window
  is minimized or occluded, and freed only with the renderer (or, on
  Windows, a lost device).
- Every frame, whole or partial, copies the whole canvas into the drawable
  or back buffer, so a whole frame costs slightly more than upstream's,
  which draws straight into it.
- `GPUI_PARTIAL_REDRAW=0` turns all of it off: every frame is drawn whole,
  straight into the drawable or back buffer, and no canvas is created.
- On Apple's tile-based GPUs a render pass that loads the canvas reads and
  writes the whole attachment, and the copy reads it again and writes the
  drawable: a partial frame moves the window's pixels three times where a
  whole frame moves them once. Each path group starts another pass.

Measured on macOS (M4, a 1352×762 pt window at 2×, `gpui_perf --idle`, 15 s
per scenario, 3 rounds alternating the modes, GPU time from each process's
`accumulatedGPUTime` in `ioreg`), whole frames (`GPUI_PARTIAL_REDRAW=0`)
against partial ones, before frames with nothing to draw were skipped:

| Scenario | Process CPU | Application GPU ms/s | WindowServer GPU ms/s |
| --- | --- | --- | --- |
| CaretBlink | 1.89% → 1.89% | 9.8 → 7.5 (−24%) | 2.3 → 3.7 |
| Clock | 1.53% → 1.58% | 5.0 → 3.3 (−33%) | 2.3 → 2.7 |
| Hover | 3.62% → 3.71% | 14.4 → 11.2 (−22%) | 4.8 → 4.8 |
| Quotes | 4.64% → 4.49% | 16.9 → 6.1 (−64%) | 6.3 → 9.3 |
| Spinner | 47.7% → 51.2% | 501 → 389 (−22%) | 239 → 223 |
| Scroll | 46.8% → 47.0% | 535 → 440 (−18%) | 213 → 207 |

Every frame of the partial runs was partial. The application's GPU time
drops in every scenario, but a partial frame that changes a caret (96
pixels) still takes 75–80% of a whole frame's GPU time: loading, storing and
copying the canvas cost about what drawing everything does. CPU time does
not change (the GPU does the saving), and the window server, which
composites every presented drawable whole, saves nothing. Quotes saves
most, as many of its frames have empty damage; those frames are now
skipped rather than drawn and presented, which also spares the window
server. Windows has not been measured on real hardware. CI checks the
pixels of both (see Verifying); measure with `GPUI_RENDER_STATS=1` and the
platform's GPU tools (Instruments, PIX or GPUView).

## Controls

- `GPUI_CPU_RENDER=0`: never draw on the CPU (no presenter kept, no atlas
  copy). `GPUI_CPU_RENDER=always`: draw every frame the CPU can on the CPU,
  without the size, burst and cost limits, for measurements.
- `GPUI_CPU_PRESENT=native|blit`: how CPU frames are shown; unset,
  `fast::adaptive::default_present_mode` decides.
- `GPUI_RENDER_STATS=1`: every second, each window prints a line of
  `key=value` pairs to stderr: `cpu_frames`, `gpu_frames`, `cpu_px`, mean CPU
  and GPU frame times, per presentation mode, and `why_<reason>` counts of
  GPU frames.
- `GPUI_CPU_VERIFY=1`: after each CPU frame, draws its scene whole into a
  second canvas and compares the two pixel by pixel (`fast::adaptive::verify`).
  As the raster's pixels do not depend on how a region is split, a
  difference is a change the frame's region missed (damage, stale pixels or
  atlas writes); it is logged with the frame's damage and region and
  counted in the render stats (`verify_n`, `verify_bad`, `verify_bad_px`).
  It costs a whole CPU frame per frame: for checking real applications.
- `GPUI_PARTIAL_REDRAW=0` (macOS, Windows): draw every frame whole, as
  upstream does, without the canvas.
- On macOS and Windows, `GPUI_RENDER_STATS=1` prints, every second, each
  window's `partial_frames`, `full_frames`, `skipped_frames` (nothing to
  draw, not presented), `partial_px` (the pixels the partial frames drew)
  and `why_<reason>` counts of whole frames.

## Verifying

- `cargo test -p gpui --features test-support --lib fast::tests::damage`:
  unit tests of every damage rule, and property tests over 1300 random scenes
  and edits checking that every pixel outside the damage is covered by the
  same primitives in the same order.
- `cargo test -p gpui_wgpu fast::cpu`: random and hand-built scenes of every
  primitive drawn by the CPU and by the GPU (`WgpuHeadlessRenderer`),
  compared pixel by pixel; region drawing against whole drawing; threaded
  against single-threaded.
- `cargo test -p gpui_wgpu fast::adaptive`: the policy with a fake clock,
  presenter call order, refusals, the blit path read back exactly.
- `cargo test -p gpui_linux cpu_present`: the presenters' bookkeeping.
- `cargo test -p gpui_apple fast::partial` (Metal; CI runs it on macOS):
  a scene drawn partially over the canvas of the scene before it equals the
  scene drawn whole on a fresh renderer, pixel for pixel, opaque and
  transparent (a quad, a moving glyph, a growing shadow, an image, a path
  across the damage's edges, two rectangles); several partial frames in a
  row stay exact; empty damage leaves the canvas as it was; frames that are
  not comparable, follow an atlas write, a resize or a transparency change
  draw whole, and partial frames resume after them; the rules on their own.
- `cargo test -p gpui_windows --features test-support fast::partial`
  (Direct3D 11; CI runs it on Windows, on the runner's device): the
  renderer's devices, pipelines and resources, on a composition swap chain
  that needs no window, draw a scene of every primitive kind, paths included,
  into the damage of a canvas holding the scene before, and it equals the
  scene drawn whole, pixel for pixel, opaque and transparent; empty damage
  leaves the canvas as it was; the rules on their own.
- In a real application: `GPUI_CPU_VERIFY=1 GPUI_RENDER_STATS=1`, and look
  for `verify_bad=` other than 0. Every `gpui_perf --idle` scenario, in a
  tiled and in a 1600×1000 window, natively and by blit, checks clean
  (about 1300 CPU frames); it found a raster bug in path batches, now fixed.

## Measuring

`gpui_perf --idle` runs small-update scenarios in a real window;
`script/measure-adaptive` runs each with the GPU only, adaptively, and
adaptively with blit presenting (`--modes gpu,adaptive,blit`), sampling the
process's CPU; the GPU time of the process and of the compositor, from their
DRM clients (`drm-engine-*` in `/proc/<pid>/fdinfo`: i915, xe, amdgpu and
others), which the rest of the desktop does not disturb; the whole GPU's
utilization, power and performance state (`nvidia-smi`), or on Intel its
utilization (time out of RC6) and clock (i915's sysfs); and the renderer's
statistics. See `script/measure-adaptive --help`.

### Results

`script/measure-adaptive --modes gpu,adaptive,blit --reps 2` (20 s per run
after 8 s of warm-up, means of the two repetitions), Intel Arrow Lake
integrated GPU (i915, Mesa), one 4K monitor at scale 1.5, Hyprland 0.56. On
Wayland the window is 1600×1000 logical (2400×1500 pixels); through XWayland
it is 1440×900 at scale 1.67 (the same pixels). `adaptive` is the default:
native presenting. GPU columns are milliseconds of GPU time per second, from
the DRM clients of the application and of the compositor; the idle desktop
keeps the compositor at 30–50 ms/s on its own.

Wayland:

| Scenario   | Mode     | Process CPU | Main ms/frame | App GPU ms/s | Compositor GPU ms/s | CPU / GPU frames |
|------------|----------|------------:|--------------:|-------------:|--------------------:|-----------------:|
| CaretBlink | gpu      | 0.4 %       | 1.97          | 24.6         | 45.8                | 0 / 40           |
|            | adaptive | 0.2 %       | 0.99          | 0            | 46.7                | 42 / 0           |
|            | blit     | 0.3 %       | 1.44          | 1.6          | 44.8                | 42 / 0           |
| Clock      | gpu      | 0.6 %       | 4.40          | 10.5         | 76.0                | 0 / 28           |
|            | adaptive | 0.2 %       | 1.16          | 0            | 55.5                | 25 / 0           |
|            | blit     | 0.2 %       | 1.63          | 0.7          | 47.9                | 22 / 0           |
| Hover      | gpu      | 1.3 %       | 3.09          | 52.1         | 36.8                | 0 / 80           |
|            | adaptive | 1.1 %       | 2.51          | 0            | 36.4                | 84 / 0           |
|            | blit     | 1.3 %       | 3.12          | 3.7          | 39.6                | 82 / 0           |
| Spinner    | gpu      | 8.3 %       | 1.26          | 356.1        | 163.8               | 0 / 1192         |
|            | adaptive | 4.3 %       | 0.65          | 0            | 101.1               | 1257 / 0         |
|            | blit     | 5.2 %       | 0.80          | 38.7         | 162.3               | 1258 / 0         |
| Quotes     | gpu      | 1.9 %       | 3.76          | 64.7         | 32.0                | 0 / 100          |
|            | adaptive | 1.4 %       | 2.72          | 0            | 35.4                | 104 / 0          |
|            | blit     | 1.6 %       | 3.19          | 5.1          | 31.8                | 102 / 0          |
| Scroll     | gpu      | 16.1 %      | 2.59          | 376.6        | 173.0               | 0 / 1188         |
|            | adaptive | 16.4 %      | 2.68          | 368.0        | 176.1               | 0 / 1235         |
|            | blit     | 16.6 %      | 2.69          | 330.9        | 173.1               | 0 / 1245         |

X11 (XWayland):

| Scenario   | Mode     | Process CPU | Main ms/frame | App GPU ms/s | Compositor GPU ms/s | CPU / GPU frames |
|------------|----------|------------:|--------------:|-------------:|--------------------:|-----------------:|
| CaretBlink | gpu      | 0.5 %       | 2.21          | 21.0         | 62.6                | 0 / 40           |
|            | adaptive | 0.3 %       | 1.38          | 0            | 55.2                | 41 / 0           |
|            | blit     | 0.5 %       | 1.88          | 1.7          | 81.0                | 43 / 0           |
| Clock      | gpu      | 0.3 %       | 2.77          | 11.3         | 57.0                | 0 / 20           |
|            | adaptive | 0.2 %       | 1.96          | 0            | 56.7                | 22 / 0           |
|            | blit     | 0.3 %       | 2.30          | 0.8          | 64.8                | 22 / 0           |
| Hover      | gpu      | 1.3 %       | 3.28          | 40.2         | 62.2                | 0 / 80           |
|            | adaptive | 1.0 %       | 2.51          | 0            | 60.7                | 81 / 0           |
|            | blit     | 1.3 %       | 3.15          | 3.1          | 73.8                | 84 / 0           |
| Spinner    | gpu      | 8.3 %       | 1.19          | 319.6        | 236.8               | 0 / 1200         |
|            | adaptive | 4.3 %       | 0.59          | 0            | 137.8               | 1260 / 0         |
|            | blit     | 6.3 %       | 0.86          | 37.8         | 223.7               | 1260 / 0         |
| Quotes     | gpu      | 1.9 %       | 3.42          | 48.8         | 82.2                | 0 / 100          |
|            | adaptive | 1.4 %       | 2.72          | 0            | 40.7                | 102 / 0          |
|            | blit     | 1.6 %       | 2.96          | 3.7          | 81.4                | 102 / 0          |
| Scroll     | gpu      | 19.3 %      | 2.57          | 317.5        | 239.4               | 0 / 1198         |
|            | adaptive | 18.2 %      | 2.52          | 315.5        | 239.6               | 0 / 1263         |
|            | blit     | 18.8 %      | 2.58          | 316.8        | 235.7               | 0 / 1260         |

(The CPU / GPU frame counts are the renderer's, over the stretch its
statistics lines cover, a little longer than the measured one.)

- Small updates leave the GPU: the application's GPU time drops from
  10–65 ms/s (caret, clock, hover, quotes) and 320–360 ms/s (spinner) to
  nothing, and the process's CPU drops too, as a CPU frame of a few hundred
  pixels costs less than recording and submitting a GPU frame.
- Native presenting beats blit everywhere: blit keeps 5–10 % of the GPU
  time, and the compositor recomposites the whole window for a presented
  swapchain image, where it recomposites only the damage of a `wl_shm`
  buffer or a `PutImage`. For the spinner the compositor spends 101 ms/s
  natively against 162 ms/s by blit (Wayland), 138 against 224 (X11).
- Scrolling stays on the GPU (its burst's CPU frames cost too much,
  `why_cpu_heavy`), at the GPU's cost.
- Whole-GPU utilization and clock are noisy here: the desktop keeps the
  integrated GPU about 35 % busy. Where they move, they follow the spinner:
  60 % busy at 909 MHz on the GPU, 30 % at 394 MHz adaptively (Wayland).
  RAPL power needs root, so there are no watts.

CPU frames against GPU frames: the `Idle` workspace drawn whole on the GPU
(`GPUI_CPU_RENDER=0`) and on the CPU (`GPUI_CPU_RENDER=always`, natively and
by blit), screenshotted in place (`grim`), differ in 42–64 of 3.5 M pixels by
more than 3 levels, on Wayland and on X11: the toolbar's clock (a different
second), antialiased path edges in the charts (at most 5 levels) and, once,
two pixels of a moving-average line where one of the four path samples fell
the other way (45 levels).

Earlier, on an NVIDIA RTX 3060 Ti (4K at scale 1.6, Wayland, native,
before the spinner could leave the GPU):

| Scenario   | CPU frames | Region per frame | CPU ms per frame | GPU frames |
|------------|-----------:|-----------------:|-----------------:|-----------:|
| CaretBlink | all        | ~52 px           | 0.03–0.04        | 0          |
| Clock      | all        | ~180–370 px      | 0.03–0.05        | 0          |
| Quotes     | all        | ~8–17 Kpx        | 0.01–0.03        | 0          |

The first CPU frame after GPU frames draws the canvas whole (3.9 Mpx, about
9.7 ms there). Bursts that change the window whole, such as moving between
scenarios, go to the GPU (`why_whole_in_burst`).
