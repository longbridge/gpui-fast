# GPUI Fast

A performance-focused fork of [Zed's GPUI](https://gpui.rs) with incremental rendering and native window composition. [v0.1.0](https://github.com/longbridge/gpui-fast/releases/tag/v0.1.0) is available on crates.io. The project remains experimental.

- **Retained Mode** reuses unchanged views, layout nodes and text measurements.
- **Scroll layers** reuse cached GPU tiles and list rows while scrolling, on macOS, Linux and Windows.
- **Window composition** embeds native content while keeping GPUI menus, popovers and dialogs above it.

## Performance vs GPUI

The `gpui_perf` showcase is a component gallery written the way GPUI Kit's is: a sidebar of 243 pages, a 5,000-row data table, a list of 5,000 messages, and a trading workspace that a stream of quotes updates. Built once on GPUI Fast and once on upstream GPUI (the `gpui-pre` 0.3.7 snapshot), scrolling at 32 px a frame, at up to 144 frames per second on Linux, release builds. Main-thread CPU per frame (median) and the CPU of the whole process (100% = one core):

| Scenario | GPUI | GPUI Fast |
| --- | --- | --- |
| A spinner animating | 5.91 ms, 85% CPU | 0.73 ms, 11% CPU (−88%) |
| Scrolling the sidebar | 5.95 ms, 86% CPU | 0.79 ms, 12% CPU (−87%) |
| Scrolling a page of components | 5.96 ms, 82% CPU | 0.79 ms, 12% CPU (−87%) |
| Scrolling the data table | 3.07 ms, 44% CPU | 0.54 ms, 8% CPU (−82%) |
| Refreshing the table every 33 ms | 11% CPU | 2% CPU |
| Scrolling the list | 2.66 ms, 39% CPU | 0.39 ms, 6% CPU (−85%) |
| Streaming quotes into the workspace | 5.06 ms, 43% CPU | 1.35 ms, 15% CPU (−73%) |
| Scrolling the workspace's watchlist | 5.23 ms, 77% CPU | 1.33 ms, 24% CPU (−75%) |
| Hovering the workspace's watchlist | 5.16 ms, 42% CPU | 1.32 ms, 14% CPU (−74%) |

Each figure is the mean of two runs alternating between the two builds. While the watchlist scrolls, GPUI draws 124 frames a second and GPUI Fast 144, so that row's process CPU does not compare directly. Results depend on the workload. To reproduce:

```sh
cargo run -p gpui_perf --release -- --auto
cargo run -p gpui_perf --release --no-default-features --features upstream -- --auto
```

In GPUI Kit's own story gallery (`gpui-pre` 0.3.9 against GPUI Fast 0.1.7, 60 Hz), scrolling used less CPU on 75 of 76 stories, 37% less on average; see [Story scroll CPU against gpui-pre](docs/story-scroll-cpu.md).

## Retained Mode

GPUI Fast tracks rendering dependencies and rebuilds only what changed: a window of 60 panel views that stays still costs 0.19 ms a frame instead of 3.31 ms with retention turned off. See [Retained Mode](docs/retained-mode.md) and [Scroll layers](docs/scroll-layers.md) for measurements and implementation details.

## Window composition

Opt-in composition supports macOS, Windows and Linux (Wayland and X11), based on [zed#62379](https://github.com/zed-industries/zed/pull/62379). GPUI menus, popovers and dialogs, including their shadows and translucent backdrops, render above the native content on all three:

| Platform | Native content | GPUI overlays |
| --- | --- | --- |
| macOS | AppKit view (WKWebView) | Overlay layer |
| Windows | WebView2 composition | Overlay layer |
| Linux (Wayland) | Subsurface | Overlay subsurfaces |
| Linux (X11) | Child window (WebKitGTK) | Overlay window blended by the compositing manager; cut out of the native content without one |

```sh
# WKWebView on macOS, WebView2 on Windows, a GPU surface of its own on Linux
cargo run -p gpui_perf --example native_webview
# A WebKitGTK page on Linux X11 or XWayland
cargo run -p gpui_perf --example linux_webview
```

## Using it

Alias the published crates to GPUI's library names:

```toml
[dependencies]
gpui = { package = "gpui-fast", version = "0.1.0" }
gpui_platform = { package = "gpui-fast-platform", version = "0.1.0", features = ["wayland", "x11"] }
```

The platform features above select Linux backends; omit them on macOS and Windows. GPUI Kit applications should follow the [compatibility setup](compat/README.md) to use the same GPUI core.

Changes to render state outside tracked entities, globals and list/scroll state need `cx.notify()`. See [Retained Mode](docs/retained-mode.md#what-an-application-needs-to-know) for details.

## Following upstream

v0.1.0 includes Zed's GPUI through [`a1b71072e5`](https://github.com/zed-industries/zed/commit/a1b71072e5b43faef437b471e988fbb5f972c99c). [UPSTREAM](UPSTREAM) records the revision for the current checkout.

GPUI Fast keeps its implementation in `fast/` modules with small hooks into upstream code, making future syncs easier. See [Architecture](docs/architecture.md) and [Contributing](CONTRIBUTING.md) for design, building and testing.

## License

Apache-2.0 — copyright Zed Industries, Inc. This is a modified fork; see [LICENSE-APACHE](LICENSE-APACHE).
