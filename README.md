# GPUI Fast

**An experimental project exploring Retained Mode and window composition for
GPUI.**

- **Retained Mode**: redraw only what changed since the last frame.
- **Window composition** (coming next): native views such as a WebView drawn
  inside a GPUI window, with GPUI's popovers, menus and dialogs still above
  them. This will merge [zed#62379](https://github.com/zed-industries/zed/pull/62379),
  proposed to GPUI upstream and still under review there.

Both take deep changes to GPUI, so they are tried out here first. Once they
work, we plan to propose them to [Zed's GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui).

Every change here keeps to two rules:

- **GPUI's existing API stays unchanged.** Code written for upstream GPUI
  compiles and runs here as is. New capabilities are added beside the
  existing API, and applications opt into them.
- **GPUI's own code is changed as little as possible.** gpui-fast's code lives
  in `fast/` directories beside upstream's, upstream files get only small
  hooks into it, and upstream's changes are merged in as Zed makes them. Each
  change reads as a diff against current GPUI and can be handed upstream piece
  by piece.

## Retained Mode

[GPUI](https://gpui.rs), the UI framework of the [Zed](https://github.com/zed-industries/zed)
editor, draws in immediate mode: every frame renders every view, builds a
fresh layout tree, lays it out, shapes its text, and paints the whole window
again, even when almost nothing changed. gpui-fast keeps what the last frame
worked out and redoes only what changed since. Applications are written
exactly as for upstream GPUI; they just draw less.

A frame walks the element tree three times: **build** renders views and asks
for layout, **prepaint** computes layout and places elements, **paint** turns
them into the scene handed to the GPU. Upstream does all three from scratch.
gpui-fast retains two things:

| What is retained | Drawn again from the last frame while                                                                                                                                                                                                                                           |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Views**        | nothing the view read while rendering changed — the entities it accessed, the globals it read, the list and scroll state it depends on — and it is drawn at the same place. It is then neither rendered, laid out, prepainted nor painted: its last frame's output is replayed. |
| **Layout nodes** | the element asks for the same style, children and measurement. Taffy's per-node cache survives, so unchanged parts of the tree are not laid out again. Elements keep their nodes by their path from the root, or by their `ElementId` wherever they move among their siblings.  |

Hover, scrolling, bounds, content masks and window refreshes invalidate
exactly what they affect, without the application doing anything. Retention
can be turned off, for comparison or debugging, with `GPUI_VIEW_RETENTION=0`.
[`docs/retained-mode.md`](docs/retained-mode.md) describes how it works.

What it is worth, in headless CPU time per frame for a window of 60 panel
views with 64 labels each, in release builds on Linux:

| Panels notified per frame | Upstream | gpui-fast       |
| ------------------------- | -------- | --------------- |
| None, still               | 10.43 ms | 0.25 ms (−98%)  |
| One                       | 11.58 ms | 1.35 ms (−88%)  |
| Six                       | 13.06 ms | 3.86 ms (−70%)  |
| All sixty                 | 18.25 ms | 14.57 ms (−20%) |

"Upstream" is the same build drawing every view from scratch, as with
`GPUI_VIEW_RETENTION=0`; the figures come from the `retained_bench` test. The
gain follows how little of the window changes.

Against upstream GPUI itself, in a real window: the `gpui_perf` showcase, a
component gallery laid out like GPUI Kit's (a sidebar of 243 pages, a page of
component sections, a 5,000-row data table, a `gpui::list` of 5,000 messages),
built once on gpui-fast and once on the `gpui-pre` 0.3.7 snapshot of upstream
GPUI, scrolling at 32 px a frame as a fast scrollbar drag does. Main-thread
CPU per frame (median), and the CPU of the whole process, at 144 frames per
second on Linux:

| Scenario                         | Upstream gpui-pre | gpui-fast              |
| -------------------------------- | ----------------- | ---------------------- |
| Idle, a status bar redrawn       | 6.19 ms, 84% CPU  | 0.36 ms, 6% CPU (−94%) |
| Scrolling the sidebar            | 5.51 ms, 83% CPU  | 0.88 ms, 13% CPU (−84%) |
| Scrolling a page of components   | 5.51 ms, 82% CPU  | 1.31 ms, 18% CPU (−76%) |
| Scrolling the data table         | 3.20 ms, 53% CPU  | 1.18 ms, 17% CPU (−63%) |
| Refreshing the table every 33 ms | 3.03 ms, 53% CPU  | 0.59 ms, 12% CPU (−81%) |
| Scrolling the list               | 2.74 ms, 47% CPU  | 0.92 ms, 14% CPU (−66%) |

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
compiles here untouched. One thing to know: state a view's render reads
outside entities and globals — an `Rc<RefCell<..>>`, the time,
`window.modifiers()` — needs a `cx.notify()` when it changes, as it already
does for a cached view.

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
