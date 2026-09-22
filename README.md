# gpui (standalone)

[GPUI](https://gpui.rs) extracted from the [Zed](https://github.com/zed-industries/zed)
monorepo as a self-contained cargo workspace, so it can be built and hacked on
without checking out or compiling the editor.

## Provenance

| | |
|---|---|
| Upstream | `zed-industries/zed` |
| Commit | `7960b2a7c9568e90fbe0727332149e5b2a5fd57a` (2026-09-12) |
| Extracted | 2026-09-22 |

Extraction = `crates/gpui` plus the transitive closure of the in-repo crates it
depends on (27 crates in total), the fonts they `include_bytes!`, and a root
manifest trimmed down to the dependencies those crates actually reference.
Source layout and crate paths are unchanged from upstream, so diffing or
re-syncing against zed is a straight path-for-path comparison:

```sh
diff -ru ~/Work/zed/crates/gpui ~/Work/gpui/crates/gpui
```

## Layout

```
crates/gpui                 the framework
crates/gpui_platform        platform backend dispatch
crates/gpui_{linux,macos,windows,web,apple,wgpu}
                            per-platform backends + renderer
crates/{collections,util,path,sum_tree,scheduler,refineable,...}
                            supporting crates lifted from zed
tooling/perf                test-perf harness (needed by util_macros)
assets/fonts                fonts embedded by gpui and its examples
```

## Build

```sh
cargo check -p gpui
cargo run -p gpui --example hello_world
cargo test -p gpui
```

## What was changed from upstream

Only the workspace plumbing; no crate source was touched.

- new root `Cargo.toml`: members list narrowed to the 27 extracted crates,
  `[workspace.dependencies]` filtered from 509 entries down to the 146 actually
  referenced, `[patch.crates-io]` reduced to the four patches that apply to this
  graph, per-package profile overrides filtered to crates that still exist
- new `.cargo/config.toml`: zed's rustflags and target settings, minus the
  `xtask`/collab aliases and the `tokio_unstable` cfg
- `Cargo.lock` copied from upstream so versions stay pinned

## License

Apache-2.0, same as upstream. See `LICENSE-APACHE`.
