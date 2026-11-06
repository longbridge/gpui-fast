# GPUI Fast

A performance-focused fork of [Zed's GPUI](https://gpui.rs) with incremental rendering and native window composition. [v0.1.0](https://github.com/longbridge/gpui-fast/releases/tag/v0.1.0) is available on crates.io. The project remains experimental.

- **Retained Mode** reuses unchanged views, layout nodes and text measurements.
- **Scroll layers** reuse cached GPU tiles and list rows while scrolling, on macOS, Linux and Windows.
- **Window composition** embeds native content while keeping GPUI menus, popovers and dialogs above it.

## Performance vs GPUI

Scrolling every story of the [GPUI Kit](https://github.com/longbridge/gpui-component) story gallery, built once on upstream GPUI (`gpui-pre` 0.3.9) and once on GPUI Fast (0.1.7), with the same synthetic wheel input for 15 s. GPUI Fast used less CPU on 75 of the 76 stories that scrolled; the one that rose (+6%) is within noise.

| Process CPU while scrolling (100% = one core) | GPUI | GPUI Fast | Change |
| --- | ---: | ---: | ---: |
| **Mean of 76 stories** | **13.8%** | **8.7%** | **−37%** |
| Message | 27.2% | 7.7% | −72% |
| Table | 13.2% | 4.1% | −69% |
| Tabs | 22.1% | 8.1% | −63% |
| Button | 20.6% | 8.7% | −58% |
| Shell | 28.2% | 14.2% | −50% |
| Input Group | 48.5% | 26.0% | −46% |
| VirtualList | 32.2% | 19.1% | −41% |
| DataTable | 18.1% | 13.1% | −28% |
| Editor | 11.0% | 9.8% | −11% |

Linux (Hyprland, Wayland, wgpu), release builds, 875x600 window at scale 1.5. The median story fell 34%. Setup, the driver script and all 76 stories are in [Story scroll CPU against gpui-pre](docs/story-scroll-cpu.md).

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
