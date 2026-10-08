# Scene damage and adaptive CPU rendering

Retained views, retained layout and scroll layers make the *logical* work of
a frame proportional to what changed. The *physical* work was not: every
frame, however small its change, acquired a swapchain image, recorded a
render pass over the whole window and presented it on the GPU. A blinking
caret, a hover, a ticking clock or one quote changing in a table woke the GPU
for a whole-window frame.

Two mechanisms change that:

1. **Scene damage** (`crates/gpui/src/fast/damage.rs`, every platform). Each
   finished scene carries the rectangles, in device pixels, where it can draw
   different pixels than the scene before it (`Scene::damage`).
2. **Adaptive CPU rendering** (`crates/gpui_wgpu/src/fast/adaptive/`, the
   wgpu renderer: Linux on Wayland and X11). A frame whose damage is small is
   drawn on the CPU into a frame kept in memory, only inside its damage, and
   shown without drawing the scene on the GPU. Frames that change much of the
   window, and bursts of frames whose CPU drawing costs too much, are drawn on
   the GPU as before.

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
    `PutImage`, through MIT-SHM for large rectangles.
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
