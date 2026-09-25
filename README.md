# gpui-fast

[GPUI](https://gpui.rs) extracted from the [Zed](https://github.com/zed-industries/zed)
monorepo as a self-contained cargo workspace, so it can be built and hacked on
without checking out or compiling the editor — and then worked on, mostly in the
layout engine.

On a grid of 2500 live labels drawn into a real window, a frame's main-thread
work goes from **8.30 ms to 3.72 ms** when the grid is still and from 8.70 ms to
6.37 ms when every cell changes; a wide table scrolled back and forth goes from
8.19 ms to 4.47 ms. That is on an Apple M4, against gpui as extracted.
`docs/frame-budget.html` is the measurement in full, step by step.

The public API is unchanged from upstream: everything added is additive, nothing
was removed or altered, so code written against upstream gpui compiles here
untouched.

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

## Changes from upstream

### The extraction itself

Workspace plumbing only; no crate source was touched.

- new root `Cargo.toml`: members list narrowed to the 27 extracted crates,
  `[workspace.dependencies]` filtered from 509 entries down to the 146 actually
  referenced, `[patch.crates-io]` reduced to the four patches that apply to this
  graph, per-package profile overrides filtered to crates that still exist
- new `.cargo/config.toml`: zed's rustflags and target settings, minus the
  `xtask`/collab aliases and the `tokio_unstable` cfg
- `Cargo.lock` copied from upstream so versions stay pinned

### Since then

Each of these is one commit, with its own measurements in the commit message.

- **Taffy layout nodes are kept between frames.** The engine used to clear its
  whole tree at the end of every frame, so taffy's per-node layout cache never
  survived long enough to be used once. Nodes are now keyed by an element's path
  from the root and released the first frame they go unclaimed in, and every
  write that would dirty a node is preceded by a comparison against what the
  previous frame asked for.
- **An `ElementId` identifies a layout node wherever it is laid out.** List items
  are laid out only once the list knows how many fit, and used to be keyed by the
  order they happened to be laid out in. A row carrying an `ElementId` now keeps
  its nodes as it slides.
- **Shaped text is recoloured without being reshaped**, and text that already
  fits the width it is offered is not reshaped at all.
- **Text a retained node holds stays in the line layout cache**, so a row that
  slides onto a neighbour's node finds its text already shaped, instead of every
  line in view being shaped again each time a slowly scrolled list crosses a
  row.
- **A paint operation records where its primitive went rather than copying it**,
  which takes the scene 1.7 MB lighter.
- **The bounds tree that orders primitives stays balanced.** It never split a
  full node, so bounds arriving in painting order nested it dozens of levels
  deep; splitting like an R-tree halves what paint costs.
- **List items without an id are matched by their index**, so a scrolled
  `uniform_list` or `list` keeps the layout of every row still in view whether
  or not its rows are identified.
- **Anything can carry a key.** `.key(id)` gives any element, components
  included, an identity among its siblings without adding a layout box. A
  component's own id never reached that far.
- **Diagnostics**: `Window::layout_stats()` reports where a frame's time went,
  text shaping included.
- **Benchmarks that draw through a real window**: a grid whose labels change,
  `cargo run -p gpui --example grid_frames --release -- 50 50 25`, and a list
  being scrolled,
  `cargo run -p gpui --example scroll_frames --release -- uniform oscillate 12 index`.

### Getting the most out of it

One thing is worth doing on your side: key list items by the data rather than
by the loop index, so a row keeps its identity when something is inserted ahead
of it.

```rust
.children(rows.iter().map(|row| render_row(row).key(row.id)))
```

The key has to be on the item itself, the element placed directly in the list.
A `div().id(..)` there works as well, but a component built with `RenderOnce`
reports no id of its own, whatever the element it renders into has, so a list
of components is matched by position unless each one is keyed. `.key()` works
on anything and adds nothing to the layout.

Inserting at the head of a list, unkeyed against keyed: 200 rows, 7.43 ms →
1.97 ms; 800 rows, 33.95 ms → 9.98 ms. Rows without an id keep the old
behaviour — matched by position, rebuilt when something is inserted ahead.
In a `uniform_list` or `list` they are matched by index instead, which holds
while the list scrolls but not when something is inserted ahead.

## License

Apache-2.0, same as upstream — copyright Zed Industries, Inc. See
`LICENSE-APACHE`. This is a modified fork; the changes are the commits after
`11a44c4`, and are summarised above.
