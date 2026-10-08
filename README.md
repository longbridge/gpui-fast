# GPUI Fast

A performance-focused fork of [Zed's GPUI](https://gpui.rs) with incremental rendering and native window composition. [v0.1.0](https://github.com/longbridge/gpui-fast/releases/tag/v0.1.0) is available on crates.io. The project remains experimental.

## Features

- **[Retained Mode](#retained-mode)** reuses unchanged views, layout nodes and text measurements. Scroll layers reuse cached GPU tiles and list rows while scrolling, on macOS, Linux and Windows.
- **[Window Composition](#window-composition)** embeds native content, such as web views, while keeping GPUI menus, popovers and dialogs above it, on macOS, Windows and Linux.
- **[Adaptive rendering](#adaptive-rendering)** draws frames that change little on the CPU, only where they change, without waking the GPU (Linux).
- **[Scene damage](docs/adaptive-rendering.md#scene-damage)** works out, for every frame, exactly which pixels it can change from the frame before, on every platform. Adaptive rendering is built on it.

## Retained Mode

GPUI Fast tracks rendering dependencies and rebuilds only what changed. Recorded Linux release-build CPU time per frame:

| Workload | Baseline | GPUI Fast |
| --- | --- | --- |
| Unchanged window | 3.31 ms | 0.19 ms (−94%) |
| Sidebar scrolling | 5.95 ms | 0.79 ms (−87%) |
| Streaming quotes | 5.06 ms | 1.35 ms (−73%) |

The unchanged-window baseline disables retention in the same build; the other rows compare against upstream `gpui-pre` 0.3.7. Results depend on the workload. See [Retained Mode](docs/retained-mode.md) and [Scroll layers](docs/scroll-layers.md) for measurements and implementation details.

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

## Adaptive rendering

Most of the time an application window changes a few pixels: a caret blinks, a clock ticks, a quote changes. GPUI redraws the whole window on the GPU for each of these. On Linux, GPUI Fast works out exactly which pixels a frame changes, draws only those on the CPU, and shows them without waking the GPU. Larger changes, such as scrolling, still go to the GPU.

At the same or lower CPU cost, the application stops using the GPU for small updates. Measured on Wayland with an Intel integrated GPU and a 4K monitor:

| Small update | Process CPU | Application GPU time |
| --- | --- | --- |
| Caret blink | 0.4% → 0.2% | 24.6 → 0 ms/s |
| Streaming quotes | 1.9% → 1.4% | 64.7 → 0 ms/s |
| Spinner animation | 8.3% → 4.3% | 356 → 0 ms/s |

CPU use goes down too: drawing a few hundred pixels on the CPU costs less than recording and submitting a whole-window GPU frame.

The benefit is keeping the GPU idle, which mainly saves power and heat. It is largest on laptops and discrete GPUs, where every wake-up raises the GPU's clocks. Adaptive rendering does not make an application faster. Frame rate, latency and scrolling are unchanged. The compositor still composites the window, so its GPU time drops only for animations. Power savings have not been measured yet. See [Adaptive rendering](docs/adaptive-rendering.md) for the full measurements, including X11, and for the environment variables.

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
