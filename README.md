# GPUI Fast

**An experimental project exploring Retained Mode and window composition for
GPUI.**

- **Retained Mode**: redraw only what changed since the last frame. How it
  works, and why it is built this way: [Architecture](docs/architecture.md).
- **Window composition** (coming next): native views such as a WebView drawn
  inside a GPUI window, with GPUI's popovers, menus and dialogs still above
  them. We plan to bring the work proposed in
  [zed#62379](https://github.com/zed-industries/zed/pull/62379), still under
  review upstream, into this experimental branch.

Both take deep changes to GPUI, so they are tried out here first. Once they
work, we plan to propose them to [Zed's GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui).

Every change here keeps to two rules:

- **GPUI's existing API stays unchanged.** New capabilities are added beside
  it, and applications can opt into them without rewriting their UI code.
- **GPUI's own code is changed as little as possible.** gpui-fast's code lives
  in `fast/` directories beside upstream's, upstream files get only small
  hooks into it, and upstream's changes are merged in as Zed makes them. Each
  change reads as a diff against current GPUI and can be handed upstream piece
  by piece.

## Retained Mode

[GPUI](https://gpui.rs), the UI framework of the [Zed](https://github.com/zed-industries/zed)
editor, draws in immediate mode: outside subtrees an application explicitly
caches, a frame renders every view, builds a fresh layout tree, lays it out,
shapes its text, and paints the frame again, even when almost nothing changed. gpui-fast keeps what the last
frame worked out and redoes only what changed since. The existing GPUI API is
unchanged, so applications draw less without rewriting their UI code; state
read from outside entities, globals and list or scroll state needs a
`cx.notify()`, as described below.

A frame walks the element tree three times: **request_layout** renders views
and asks for layout, **prepaint** computes layout and places elements,
**paint** turns them into the scene handed to the GPU. Upstream normally
does all three from scratch, except where an explicitly cached view is
reused.
gpui-fast retains two things:

| What is retained | Drawn again from the last frame while                                                                                                                                                                                                                                           |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Views**        | nothing the view read while rendering changed — the entities it accessed, the globals it read, the list and scroll state it depends on — and it is drawn at the same place. It is then neither rendered, laid out, prepainted nor painted: its last frame's output is replayed. |
| **Layout nodes** | the element asks for the same style, children and measurement. Taffy's per-node cache survives, so unchanged parts of the tree are not laid out again. Elements keep their nodes by their path from the root, or by their `ElementId` wherever they move among their siblings.  |

Hover, scrolling, bounds, content masks and window refreshes invalidate
exactly what they affect, without the application doing anything. Retention
can be turned off, for comparison or debugging, with `GPUI_VIEW_RETENTION=0`.
[`docs/retained-mode.md`](docs/retained-mode.md) describes how it works, and
[`docs/architecture.md`](docs/architecture.md) why it is built this way.

What it is worth, in headless CPU time per frame for a window of 60 panel
views with 64 labels each, in release builds on Linux:

| Panels notified per frame | Upstream | gpui-fast      |
| ------------------------- | -------- | -------------- |
| None, still               | 3.31 ms  | 0.19 ms (−94%) |
| One                       | 6.72 ms  | 0.71 ms (−89%) |
| Six                       | 7.27 ms  | 1.46 ms (−80%) |
| All sixty                 | 10.33 ms | 7.38 ms (−29%) |

"Upstream" is the same build drawing every view from scratch, as with
`GPUI_VIEW_RETENTION=0`; the figures come from the `retained_bench` test. The
gain follows how little of the window changes.

Against upstream GPUI itself, in a real window: the `gpui_perf` showcase, a
component gallery written the way GPUI Kit's is (a sidebar of 243 pages whose
names the root view reads from each page's entity, a page of component
sections holding their state in keyed entities, a 5,000-row data table, a
`gpui::list` of 5,000 messages, application state in a global entity that
most views read and that changes every two seconds, and a trading workspace of
docked market panels that a stream of quotes updates), built once on gpui-fast
and once on the `gpui-pre` 0.3.7 snapshot of upstream GPUI, scrolling at
32 px a frame as a fast scrollbar drag does. Main-thread CPU per frame
(median), and the CPU of the whole process, at up to 144 frames per second on
Linux:

| Scenario                               | Upstream gpui-pre | gpui-fast                |
| -------------------------------------- | ----------------- | ------------------------ |
| A spinner animating                    | 5.91 ms, 85% CPU  | 0.73 ms, 11% CPU (−88%)  |
| Scrolling the sidebar                  | 5.95 ms, 86% CPU  | 0.79 ms, 12% CPU (−87%)  |
| Scrolling a page of components         | 5.96 ms, 82% CPU  | 0.79 ms, 12% CPU (−87%)  |
| Scrolling the data table               | 3.07 ms, 44% CPU  | 0.54 ms, 8% CPU (−82%)   |
| Refreshing the table every 33 ms       | 11% CPU           | 2% CPU                   |
| Scrolling the list                     | 2.66 ms, 39% CPU  | 0.39 ms, 6% CPU (−85%)   |
| Streaming quotes into the workspace    | 5.06 ms, 43% CPU  | 1.35 ms, 15% CPU (−73%)  |
| Scrolling the workspace's watchlist    | 5.23 ms, 77% CPU  | 1.33 ms, 24% CPU (−75%)  |
| Hovering the workspace's watchlist     | 5.16 ms, 42% CPU  | 1.32 ms, 14% CPU (−74%)  |

The refreshed table draws about 30 frames a second, which only its process
CPU describes; the workspace draws a frame per batch of quotes, about 60 a
second, and while its watchlist scrolls, 124 a second upstream and 144 on
gpui-fast, so that row's process CPU does not compare directly. Each figure
is the mean of two runs alternating between the two builds.

```sh
cargo run -p gpui_perf --release -- --auto
cargo run -p gpui_perf --release --no-default-features --features upstream -- --auto
```

A retained frame is checked against the frame drawing from scratch would
have produced: a test drives two windows through the same random history, one
drawing incrementally and one from scratch, and requires every frame to match.

## Using it

gpui-fast is for trying Retained Mode out, and for measuring it on real
applications; expect its internals to change as the experiment goes on, but
not its API: the public API is upstream's, and code written for upstream GPUI
compiles here untouched. One thing to know: state a view's render reads that
gpui-fast cannot observe — an `Rc<RefCell<..>>` outside an entity, the time —
needs a `cx.notify()` when it changes, as it already does for a cached view.

Point a project at it in place of upstream GPUI:

```toml
[dependencies]
gpui = { git = "https://github.com/longbridge/gpui-fast" }
```

## gpui-fast, gpui-pre and gpui-ce

Several projects build on GPUI outside Zed:

- **gpui-pre** publishes snapshots of upstream GPUI to crates.io, unmodified,
  so that libraries such as [GPUI Kit](https://github.com/longbridge/gpui-kit)
  can depend on a released GPUI. gpui-fast is a separate experiment and is
  not part of it.
- **gpui-ce** is a community-maintained GPUI.

gpui-fast has a narrower focus: Retained Mode and window composition for
GPUI. Work that makes it
into upstream GPUI reaches all of these projects, and Zed itself.

## Following upstream

GPUI Fast is based on Zed at the commit recorded in [`UPSTREAM`](UPSTREAM) and
takes upstream's changes as Zed makes them. Its own code is kept apart from
upstream's, so a new upstream is a merge rather than a port. See
[`CONTRIBUTING.md`](CONTRIBUTING.md) for how that is kept true, and for building,
testing and measuring.

## License

Apache-2.0, as upstream — copyright Zed Industries, Inc. See `LICENSE-APACHE`.
This is a modified fork; the changes are the commits after `11a44c4`, the
import of upstream.
