# Contributing to GPUI Fast

Issues are not open for now. Pull requests are welcome for fixes and
improvements to Retained Mode, and for needs that are clearly reasonable and
pressing and keep gpui-fast in step with GPUI upstream. Anything else belongs
in [GPUI upstream](https://github.com/zed-industries/zed); see the
[README](README.md#contributing).

## Staying in step with upstream

gpui-fast tracks Zed's `crates/gpui` and the Zed crates it depends on. The
upstream commit it is based on is recorded in [`UPSTREAM`](UPSTREAM); the
source layout and crate paths are upstream's, so any file compares path for
path with a Zed checkout.

To keep merging upstream cheap, gpui-fast's changes never spread through
upstream's files:

- **All of gpui-fast's logic lives in `crates/<crate>/src/fast/`**, one file per
  topic — `fast/retained.rs`, `fast/dependencies.rs`, `fast/layout.rs`, … — and is
  always referred to by its path, `crate::fast::retained::…`, so it is obvious
  where code comes from. A new topic gets a new `fast/<topic>.rs`.
- **Upstream files hold only hooks**: a field holding a `fast` struct, a
  one-line call into `fast`, a visibility bump. No algorithms, no new types,
  no tests, no reformatting.
- **Every hook names `fast`**, so whoever merges upstream can tell it from
  upstream's code: `crate::fast::dependencies::note_notify(&mut self.entities,
  entity_id)`, not `self.entities.note_notify(entity_id)`, even when the
  function is a method defined in `fast/`.
- **`script/check-upstream` enforces it**, comparing every upstream file with
  upstream's own copy and failing on anything more than a hook, or on a hook
  that doesn't name `fast`. CI runs it too. Run it before committing:

  ```sh
  script/check-upstream                      # against upstream as imported
  script/check-upstream --zed ~/github/zed   # against a Zed checkout
  ```

[`docs/upstream-sync.md`](docs/upstream-sync.md) has the rules in full and the
procedure for taking a new upstream commit.

## Building and testing

```sh
cargo run -p gpui --example hello_world
cargo test -p gpui --features test-support
script/check-upstream
```

## Measuring

Always measure a release build (`--release`): a debug build's costs are
dominated by what the optimizer removes, and say nothing about gpui-fast.

### The showcase

`cargo run -p gpui_perf --release` opens the showcase, a component gallery
laid out the way GPUI Kit's story gallery is: a sidebar of pages, a scrolled
page of component sections, and a data table refreshed by a timer. It is the
first place to look at a performance problem found in an application, without
setting one up.

- The toolbar picks what scrolls itself — `Off`, `Sidebar`, `Page`, `Table`
  (keys `1`–`4`) — and switches `Refresh data` (`R`), which updates table
  rows every 33 ms, and `Retained views` (`V`). The arrow keys move through
  the sidebar.
- The status bar shows, every half second: frames per second, the CPU of the
  whole process and of the main thread, on macOS the share of that CPU time
  spent on performance cores, the process's memory (resident on Linux, its
  footprint on macOS), what the main thread spent per frame
  and on build, prepaint, layout (Taffy's share of prepaint) and paint, and how
  many views were built and reused per frame.

`--auto` runs every scenario — idle, scrolling the sidebar, the page and the
table, refreshing the table — with retained views on and then off, prints
what each cost per frame, and quits:

```sh
cargo run -p gpui_perf --release -- --auto
cargo run -p gpui_perf --release -- --auto --only ScrollPage --retention on --frames 1000
```

On macOS, `--auto` holds the CPU's clock up while it measures, with a helper
process spinning on a performance core, as the performance governor does on
Linux. Without it macOS runs a lightly loaded process on efficiency cores and
at low clocks, so the less work a frame does, the longer each instruction of
it takes: CPU time per frame differs twofold from one run of the same build
to the next, and retained views can report as much CPU time as drawing from
scratch while retiring 40% fewer instructions. `--no-hold-clock` measures
without the helper, as an application runs. The report also gives the main
thread's instructions per frame (`instr p50`), which do not depend on the
clock, and the share of the process's CPU time spent on performance cores
(`p-cores`). The window has to be on screen: with the display asleep or the
screen locked, no frames are drawn and `--auto` waits.

To profile one scenario, run it for longer under a profiler, for instance
`samply record ./target/release/gpui_perf --auto --only ScrollPage --frames 5000`.

### Window composition

`--composition` composes the showcase's window the way one embedding a
webview does: an empty native surface over the top right of the page, between
GPUI's content and its overlays. A window cannot stop composing once it has
started, so what composition costs is the difference between two runs:

```sh
cargo run -p gpui_perf --release -- --auto
cargo run -p gpui_perf --release -- --auto --composition
```

The `present` column, and `Present` in the status bar, is the time per frame
spent handing the frame to the platform, which with composition includes
splitting the scene into one per surface. It is wall time, so a present that
waits for the GPU counts that wait; `cpu p50` says whether the main thread
did more work.

### Comparing with upstream GPUI

The `upstream` feature builds the same showcase against upstream GPUI, the
`gpui-pre` snapshot GPUI Kit pins (`gpui-pre` and `gpui-pre-platform`
`=0.3.7`), instead of this repository's, so both can run the same screens
and the same `--auto` scenarios:

```sh
cargo run -p gpui_perf --release -- --auto                       # gpui-fast
cargo run -p gpui_perf --release --features upstream -- --auto   # upstream
```

Upstream has no retained views and does not time the phases of a frame, so
its showcase hides the `Retained views` switch, and its status bar and report
show frame rate and CPU only (`-` in the other columns). Add
`--no-default-features` to the upstream build to skip building this
repository's GPUI. `--headless` runs on gpui-fast only.

### Headless scenarios

`--headless` measures simulated screens — forms, lists, tables, settings, a
docked trading workspace — without a window, with real text shaping,
retained and not, and `--verify` checks that both paint the same quads,
text, icons and images on every frame:

```sh
cargo run -p gpui_perf --release -- --headless
cargo run -p gpui_perf --release -- --headless --scenario table --frames 200
cargo run -p gpui_perf --release -- --headless --verify
cargo run -p gpui_perf --release -- --headless --list
```

### Other benchmarks

```sh
# A dashboard of panel views in a real window, 2 of 60 panels changing every frame
cargo run -p gpui_perf --example views_frames --release -- 60 64 2
GPUI_VIEW_RETENTION=0 cargo run -p gpui_perf --example views_frames --release -- 60 64 2
# 60 panel views of 64 labels, headless
cargo test -p gpui --lib --release retained_bench -- --ignored --nocapture
```

[`docs/retained-mode.md`](docs/retained-mode.md) describes how retained mode
works, and how it is verified and measured.

A retained frame has to be the frame that drawing from scratch would have
produced. An oracle test (`crates/gpui/src/fast/tests/oracle.rs`) drives two
windows through the same random history, one drawing incrementally and one
from scratch, and requires every frame to match; `gpui_perf --headless
--verify` does the same for whole simulated screens.

## Repository layout

```
crates/gpui                 the framework; gpui-fast's code is in src/fast/
crates/gpui_platform        platform backend dispatch
crates/gpui_{linux,macos,windows,web,apple,wgpu}
                            per-platform backends and renderers
crates/{collections,util,sum_tree,scheduler,refineable,...}
                            supporting crates from Zed
crates/gpui_perf            gpui-fast's benchmarks (not upstream)
tooling/perf                test-perf harness from Zed
```
