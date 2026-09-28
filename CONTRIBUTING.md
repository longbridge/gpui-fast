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
- **`script/check-upstream` enforces it**, comparing every upstream file with
  upstream's own copy and failing on anything more than a hook. Run it before
  committing:

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

```sh
# Simulated forms, lists, tables and settings screens, retained and not
cargo run -p gpui_perf --release
# A dashboard of panel views in a real window, 2 of 60 panels changing every frame
cargo run -p gpui_perf --example views_frames --release -- 60 64 2
GPUI_VIEW_RETENTION=0 cargo run -p gpui_perf --example views_frames --release -- 60 64 2
```

[`docs/retained-mode.md`](docs/retained-mode.md) describes how retained mode
works, and how it is verified and measured.

A retained frame has to be the frame that drawing from scratch would have
produced. An oracle test (`crates/gpui/src/fast/tests/oracle.rs`) drives two
windows through the same random history, one drawing incrementally and one
from scratch, and requires every frame to match; `gpui_perf --verify` does the
same for whole simulated screens.

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
