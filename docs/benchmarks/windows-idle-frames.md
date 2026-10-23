# Windows idle frame scheduling

## Release idle benchmark

2026-10-23, compared with `91f60ec0bc32`. Both executables were built in the same checkout with the same lock and workspace release settings; only the Windows backend changed. The median process CPU cycle count fell **96.03%**.

| Backend | Median process CPU cycles | Range |
|---|---:|---:|
| Unmodified | 250,852,000 | 248,406,720–261,243,420 |
| Requested frames + sleeping worker + device notification | 9,963,020 | 9,594,460–10,032,040 |

A static 1280×800 logical-pixel window, 3 seconds warmup followed by 20 seconds measurement, three fresh processes per variant. Order: baseline/patched, patched/baseline, baseline/patched. All six intervals had zero view renders. This measures idle framework overhead, not whole-application CPU utilization. `QueryProcessCycleTime` is the comparison metric; raw `GetProcessTimes` readings are retained but are too coarse for the small patched workload.

Ryzen 7 5800X3D, RTX 4060, Windows build 26200, 1920×1080 at 120 Hz, high-performance power plan. DirectComposition is disabled in this reproducer. Rust 1.99.1 MSVC, thin LTO, one codegen unit, static CRT. [Raw samples and settings](windows-idle-frames.json).

Run on Windows:

```powershell
cargo build -p gpui_perf --example windows_idle_frames --release --locked
.\target\release\examples\windows_idle_frames.exe idle.json
.\target\release\examples\windows_idle_frames.exe verify.json --verify
```

`--verify` additionally updates two windows 50 times and runs 120 chained frame callbacks. It passed on the measured patched release executable. The idle assertion deliberately rejects runs disturbed by a view redraw.

## GPU recovery: separate minimal test application

The GPU-reset check used a **dev-build minimal native GPUI test application** linked to the modified `gpui_windows` backend and its extracted `fast::frame_schedule` implementation. This was separate from the release benchmark above. Two idle windows were open on the same RTX 4060 setup, with DirectComposition disabled.

An actual `DXCap.exe -forcetdr` caused `GetDeviceRemovedReason` to return `0x887A0007` (`DXGI_ERROR_DEVICE_RESET`). Temporary observation counters confirmed that the device-removal notification woke the scheduler, both windows recreated their renderers, view rendering resumed, 120 chained callbacks completed, and the worker returned to idle. No simulated device-loss result was substituted in this check. The temporary test application and counters were removed afterwards; neither measured release executable contains them.

The test target was the Windows backend and extracted scheduler within this minimal application. **Zed itself was not built or tested; this is not an end-to-end Zed application recovery test.**

## Unit tests

The three unit tests cover per-window request coalescing, stopping a sleeping worker, and overlapping old/new workers during GPU replacement. They do not trigger a GPU reset.
