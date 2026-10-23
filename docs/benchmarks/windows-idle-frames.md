# Windows frame scheduling validation

## Earlier release benchmark

Measured before the paint-order follow-up, against `91f60ec0bc32`, with the same checkout, lock and release settings.

| Backend | Median process CPU cycles / 20 s | Range |
|---|---:|---:|
| Baseline | 250,852,000 | 248,406,720–261,243,420 |
| Demand-driven scheduler | 9,963,020 | 9,594,460–10,032,040 |

Reduction: 96.03%. Static 1280x800 window, 3 s warmup, three processes each, alternating order, zero view renders. Idle framework overhead, not whole-application utilization. Ryzen 7 5800X3D, RTX 4060, Windows 26200, 120 Hz, high-performance power plan, DirectComposition disabled. Rust 1.99.1 MSVC, thin LTO, one codegen unit, static CRT. Raw samples are retained locally. These figures have not been remeasured after the paint-order follow-up.

```powershell
cargo build -p gpui_perf --example windows_idle_frames --release --locked
.\target\release\examples\windows_idle_frames.exe idle.json
.\target\release\examples\windows_idle_frames.exe verify.json --verify
```

The measured release executable passed `--verify`: 50 updates across two windows and 120 callbacks.

## Native regression tests

Minimal native GPUI application, dev build, interactive Windows desktop; **not a full Zed application test**. Run each condition in a fresh process:

```powershell
$env:GPUI_NATIVE_CASE = 'paint_race' # or input_restore / recovery
$env:GPUI_NATIVE_DCOMP = '1'        # repeat with 0
cargo test -p gpui_windows --features test-support --lib fast::native_tests::interactive_frame_lifecycle -- --ignored --exact --nocapture --test-threads=1
```

- `paint_race`: synchronize actual worker invalidation after callbacks re-arm demand, before the current paint returns. Trailing validation restored: update region lost, callbacks stop at 1/12. Fixed order: region retained, 12/12. Both DirectComposition modes.
- `input_restore`: approximately 240 Hz input invalidates the view. Both modes produced 90 successful DXGI presents in the 100–850 ms tail, then parked. Minimize with a view change, restore without extra notify, 120 two-window callbacks, and survivor drawing passed.
- `recovery`: requires `GPUI_TEST_ALLOW_GPU_RESET=1` and `DXCap.exe`; this resets the actual GPU. Set `GPUI_NATIVE_CHURN=0` for the passing minimized-window case. Both modes detected `0x887A0007`, recreated both renderers, restored the changed view and completed 120 callbacks.
- **Known pre-existing failure:** recovery with `GPUI_NATIVE_CHURN=1` overlaps window close/open with GPU recovery and hangs in both this branch and baseline `91f60ec0bc32` with identical hooks. A test-only 350 ms pause after acquiring the window-list read lock forces the overlap with synchronous UI dispatch; the UI waits for a write lock. An 8 s watchdog terminates the test process. This issue remains unfixed.

Optional `GPUI_NATIVE_TEST_LOG` writes raw observations to a local JSON file. DXGI Present success is observed, not physical scanout.

Three scheduler unit tests, clippy, changed-file formatting and `script/check-upstream` pass; an existing unused import warning remains.
