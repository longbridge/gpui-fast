//! Lazily initializes the Shell `CLSID_DragDropHelper` COM object on first drag,
//! while prewarming its in-process COM server off the UI thread during startup.
//!
//! Upstream instantiates `CLSID_DragDropHelper` synchronously inside
//! `WindowsPlatform::new()`, paying ~10-13 ms on every GUI startup to activate
//! the Shell drag-and-drop COM server (`shell32.dll`, `ThreadingModel =
//! Apartment`) even when an application never receives an external drag.
//!
//! Pure lazy initialization moves that ~10-12 ms into the first `DragEnter`
//! callback on the UI thread, which exceeds a 120 Hz frame budget (8.33 ms).
//! Instead, `LazyDropTargetHelper::new()` spawns a short-lived background
//! thread with its own temporary STA (`COINIT_APARTMENTTHREADED`) that creates
//! and immediately drops a throwaway `CLSID_DragDropHelper` instance before
//! calling `CoUninitialize()`. No COM pointer crosses thread or apartment
//! boundaries, while process-global DLLs and COM class factory caches are
//! warmed off the UI thread. On the first `DragEnter`, the UI thread creates
//! its own STA `IDropTargetHelper` in ~0.14 ms (matching eager baseline).

use std::{cell::OnceCell, rc::Rc};

use gpui_util::ResultExt;
use windows::Win32::{
    Foundation::{HWND, POINT},
    System::{
        Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize, IDataObject,
        },
        Ole::DROPEFFECT,
    },
    UI::Shell::{CLSID_DragDropHelper, IDropTargetHelper},
};

#[derive(Clone, Default)]
pub(crate) struct LazyDropTargetHelper(Rc<OnceCell<Option<IDropTargetHelper>>>);

impl LazyDropTargetHelper {
    pub(crate) fn new() -> Self {
        spawn_com_server_prewarm();
        Self(Rc::new(OnceCell::new()))
    }

    fn get_or_init(&self) -> Option<&IDropTargetHelper> {
        self.0
            .get_or_init(|| {
                match unsafe {
                    CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER)
                } {
                    Ok(helper) => Some(helper),
                    Err(error) => {
                        log::warn!(
                            "Failed to create CLSID_DragDropHelper; continuing without drag image preview: {error}"
                        );
                        None
                    }
                }
            })
            .as_ref()
    }

    fn get_initialized(&self) -> Option<&IDropTargetHelper> {
        self.0.get().and_then(Option::as_ref)
    }
}

/// Warms `CLSID_DragDropHelper`'s in-process COM server on a dedicated
/// short-lived STA thread so neither `WindowsPlatform::new()` nor the first
/// UI-thread `DragEnter` pays the one-time ~10-13 ms activation cost.
///
/// Using a dedicated thread rather than the shared `BackgroundExecutor` pool
/// avoids apartment-mode conflicts (`RPC_E_CHANGED_MODE`) with worker threads
/// that may already be initialized as MTA.
fn spawn_com_server_prewarm() -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("gpui-dragdrop-prewarm".into())
        .spawn(|| {
            let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            if initialized.is_ok() {
                let helper: windows::core::Result<IDropTargetHelper> =
                    unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) };
                std::mem::drop(helper);
                unsafe {
                    CoUninitialize();
                }
            }
        })
        .ok()
}

pub(crate) fn drag_enter(
    helper: &LazyDropTargetHelper,
    hwnd: HWND,
    data_object: &IDataObject,
    cursor_position: &POINT,
    effect: DROPEFFECT,
) {
    if let Some(helper) = helper.get_or_init() {
        unsafe {
            helper
                .DragEnter(hwnd, data_object, cursor_position, effect)
                .log_err();
        }
    }
}

pub(crate) fn drag_over(
    helper: &LazyDropTargetHelper,
    cursor_position: &POINT,
    effect: DROPEFFECT,
) {
    if let Some(helper) = helper.get_initialized() {
        unsafe {
            helper.DragOver(cursor_position, effect).log_err();
        }
    }
}

pub(crate) fn drag_leave(helper: &LazyDropTargetHelper) {
    if let Some(helper) = helper.get_initialized() {
        unsafe {
            helper.DragLeave().log_err();
        }
    }
}

pub(crate) fn drop(
    helper: &LazyDropTargetHelper,
    data_object: &IDataObject,
    cursor_position: &POINT,
    effect: DROPEFFECT,
) {
    if let Some(helper) = helper.get_initialized() {
        unsafe {
            helper.Drop(data_object, cursor_position, effect).log_err();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::fast::drop_target_helper::{
        LazyDropTargetHelper, drag_enter, drag_leave, drag_over, drop as drag_drop,
        spawn_com_server_prewarm,
    };
    use crate::platform::WindowsPlatform;
    use std::cell::OnceCell;
    use std::process::Command;
    use std::rc::Rc;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{
        COLORREF, DV_E_FORMATETC, E_NOTIMPL, HWND, POINT, S_FALSE, S_OK, SIZE,
    };
    use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
    use windows::Win32::System::{
        Com::{
            CLSCTX_INPROC_SERVER, CoCreateInstance, FORMATETC, IAdviseSink, IDataObject,
            IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM,
        },
        Ole::{DROPEFFECT_COPY, OleInitialize, OleUninitialize},
    };
    use windows::Win32::UI::{
        Shell::{
            BHID_DataObject, CLSID_DragDropHelper, IDragSourceHelper, IShellItem,
            SHCreateItemFromParsingName, SHDRAGIMAGE,
        },
        WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WINDOW_STYLE},
    };
    use windows::core::{BOOL, HRESULT, HSTRING, Ref, Result, implement, w};

    const BENCH_CHILD_ENV: &str = "GPUI_BENCH_DRAGDROP_CHILD";
    const BENCH_DRAG_CHILD_ENV: &str = "GPUI_BENCH_DRAG_ENTER_CHILD";

    fn new_cold_helper() -> LazyDropTargetHelper {
        LazyDropTargetHelper(Rc::new(OnceCell::new()))
    }

    /// Minimal COM `IDataObject` for timing `IDropTargetHelper::DragEnter`
    /// without pre-loading `shell32.dll` via `SHCreateItemFromParsingName`.
    #[implement(IDataObject)]
    struct MinimalDataObject;

    #[allow(non_snake_case)]
    impl IDataObject_Impl for MinimalDataObject_Impl {
        fn GetData(&self, _pformatetcin: *const FORMATETC) -> Result<STGMEDIUM> {
            Err(DV_E_FORMATETC.into())
        }

        fn GetDataHere(
            &self,
            _pformatetc: *const FORMATETC,
            _pmedium: *mut STGMEDIUM,
        ) -> Result<()> {
            Err(DV_E_FORMATETC.into())
        }

        fn QueryGetData(&self, _pformatetc: *const FORMATETC) -> HRESULT {
            DV_E_FORMATETC
        }

        fn GetCanonicalFormatEtc(
            &self,
            _pformatectin: *const FORMATETC,
            pformatetcout: *mut FORMATETC,
        ) -> HRESULT {
            if !pformatetcout.is_null() {
                unsafe {
                    (*pformatetcout).ptd = std::ptr::null_mut();
                }
            }
            S_OK
        }

        fn SetData(
            &self,
            _pformatetc: *const FORMATETC,
            _pmedium: *const STGMEDIUM,
            _frelease: BOOL,
        ) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn EnumFormatEtc(&self, _dwdirection: u32) -> Result<IEnumFORMATETC> {
            Err(E_NOTIMPL.into())
        }

        fn DAdvise(
            &self,
            _pformatetc: *const FORMATETC,
            _advf: u32,
            _padvsink: Ref<'_, IAdviseSink>,
        ) -> Result<u32> {
            Err(E_NOTIMPL.into())
        }

        fn DUnadvise(&self, _dwconnection: u32) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
            Err(S_FALSE.into())
        }
    }

    struct TestHwnd(HWND);

    impl TestHwnd {
        fn new() -> Self {
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("gpui_drag_test"),
                    WINDOW_STYLE(0),
                    0,
                    0,
                    100,
                    100,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap()
            };
            Self(hwnd)
        }
    }

    impl Drop for TestHwnd {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }

    #[test]
    fn lazy_drop_target_helper_initializes_on_demand_and_shares_instance() {
        unsafe { OleInitialize(None).unwrap() };
        let helper = new_cold_helper();
        let cloned = helper.clone();

        assert!(helper.get_initialized().is_none());
        assert!(cloned.get_initialized().is_none());

        let first = helper.get_or_init();
        assert!(first.is_some());
        assert!(cloned.get_initialized().is_some());

        drop(helper);
        drop(cloned);
        unsafe { OleUninitialize() };
    }

    #[test]
    fn drag_lifecycle_across_two_windows_and_repeated_drags() {
        unsafe { OleInitialize(None).unwrap() };
        let prewarm = spawn_com_server_prewarm().unwrap();
        prewarm.join().unwrap();

        let helper_win1 = new_cold_helper();
        let helper_win2 = helper_win1.clone();
        let win1 = TestHwnd::new();
        let win2 = TestHwnd::new();
        let exe_path = HSTRING::from(std::env::current_exe().unwrap().as_os_str());
        let shell_item: IShellItem =
            unsafe { SHCreateItemFromParsingName(&exe_path, None) }.unwrap();
        let data_object: IDataObject =
            unsafe { shell_item.BindToHandler(None, &BHID_DataObject) }.unwrap();
        // Attach a real Shell drag image (`DragImageBits` / `DragContext`), matching
        // what Explorer sets on the `IDataObject` before calling `DoDragDrop`.
        let source_helper: IDragSourceHelper =
            unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) }.unwrap();
        let bitmap = unsafe { CreateBitmap(16, 16, 1, 32, None) };
        assert!(!bitmap.is_invalid());
        let mut drag_image = SHDRAGIMAGE {
            sizeDragImage: SIZE { cx: 16, cy: 16 },
            ptOffset: POINT { x: 8, y: 8 },
            hbmpDragImage: bitmap,
            crColorKey: COLORREF(0),
        };
        if let Err(error) =
            unsafe { source_helper.InitializeFromBitmap(&mut drag_image, &data_object) }
        {
            unsafe {
                let _ = DeleteObject(bitmap.into());
            }
            panic!("IDragSourceHelper::InitializeFromBitmap failed: {error}");
        }
        let pt = POINT { x: 20, y: 20 };

        // Calling over/leave before any DragEnter stays uninitialized and is a safe no-op.
        drag_over(&helper_win1, &pt, DROPEFFECT_COPY);
        drag_leave(&helper_win1);
        assert!(helper_win1.get_initialized().is_none());
        assert!(helper_win2.get_initialized().is_none());

        // First drag on window 1: Enter -> Over -> Leave; assert COM methods succeed.
        let com_helper = helper_win1.get_or_init().expect("CLSID_DragDropHelper");
        assert!(helper_win2.get_initialized().is_some());
        unsafe {
            com_helper
                .DragEnter(win1.0, &data_object, &pt, DROPEFFECT_COPY)
                .unwrap();
            com_helper.DragOver(&pt, DROPEFFECT_COPY).unwrap();
            com_helper.DragLeave().unwrap();
        }

        // Repeated drag on window 1: Enter -> Over -> Drop.
        unsafe {
            com_helper
                .DragEnter(win1.0, &data_object, &pt, DROPEFFECT_COPY)
                .unwrap();
            com_helper.DragOver(&pt, DROPEFFECT_COPY).unwrap();
            com_helper.Drop(&data_object, &pt, DROPEFFECT_COPY).unwrap();
        }

        // Drag on second window sharing the same LazyDropTargetHelper: Enter -> Over -> Drop.
        let com_helper_win2 = helper_win2.get_initialized().expect("shared helper");
        unsafe {
            com_helper_win2
                .DragEnter(win2.0, &data_object, &pt, DROPEFFECT_COPY)
                .unwrap();
            com_helper_win2.DragOver(&pt, DROPEFFECT_COPY).unwrap();
            com_helper_win2
                .Drop(&data_object, &pt, DROPEFFECT_COPY)
                .unwrap();
        }

        // Also exercise the forwarding wrappers end-to-end.
        drag_enter(&helper_win2, win2.0, &data_object, &pt, DROPEFFECT_COPY);
        drag_over(&helper_win2, &pt, DROPEFFECT_COPY);
        drag_drop(&helper_win2, &data_object, &pt, DROPEFFECT_COPY);

        drop(data_object);
        drop(shell_item);
        drop(win2);
        drop(win1);
        drop(helper_win2);
        drop(helper_win1);
        unsafe { OleUninitialize() };
    }

    #[test]
    #[ignore = "manual multi-process DragEnter latency benchmark"]
    #[allow(clippy::disallowed_methods)]
    fn bench_drag_enter_latency() {
        if let Ok(mode) = std::env::var(BENCH_DRAG_CHILD_ENV) {
            // `WindowsPlatform::new(false)` spawns the production background
            // prewarm thread automatically. Do not spawn or join any extra
            // prewarm thread here.
            let platform = WindowsPlatform::new(false).unwrap();
            let helper_win1 = new_cold_helper();
            let helper_win2 = helper_win1.clone();
            let win1 = TestHwnd::new();
            let win2 = TestHwnd::new();
            let data_object: IDataObject = MinimalDataObject.into();
            let pt = POINT { x: 20, y: 20 };

            match mode.as_str() {
                "eager" => {
                    assert!(helper_win1.get_or_init().is_some());
                }
                "after_50ms" => {
                    std::thread::sleep(Duration::from_millis(50));
                    assert!(helper_win1.get_initialized().is_none());
                }
                "immediate_0ms" => {
                    assert!(helper_win1.get_initialized().is_none());
                }
                other => panic!("unknown mode: {other}"),
            }

            // 1. First DragEnter on window 1
            let t1 = Instant::now();
            drag_enter(&helper_win1, win1.0, &data_object, &pt, DROPEFFECT_COPY);
            let first_win1_ms = t1.elapsed().as_secs_f64() * 1000.0;
            drag_over(&helper_win1, &pt, DROPEFFECT_COPY);
            drag_leave(&helper_win1);

            // 2. Second (repeated) DragEnter on window 1
            let t2 = Instant::now();
            drag_enter(&helper_win1, win1.0, &data_object, &pt, DROPEFFECT_COPY);
            let second_win1_ms = t2.elapsed().as_secs_f64() * 1000.0;
            drag_over(&helper_win1, &pt, DROPEFFECT_COPY);
            drag_drop(&helper_win1, &data_object, &pt, DROPEFFECT_COPY);

            // 3. First DragEnter on window 2 (reusing shared helper)
            let t3 = Instant::now();
            drag_enter(&helper_win2, win2.0, &data_object, &pt, DROPEFFECT_COPY);
            let first_win2_ms = t3.elapsed().as_secs_f64() * 1000.0;
            drag_over(&helper_win2, &pt, DROPEFFECT_COPY);
            drag_drop(&helper_win2, &data_object, &pt, DROPEFFECT_COPY);

            println!("RESULT={first_win1_ms:.3},{second_win1_ms:.3},{first_win2_ms:.3}");

            drop(data_object);
            drop(win2);
            drop(win1);
            drop(helper_win2);
            drop(helper_win1);
            drop(platform);
            return;
        }

        let exe = std::env::current_exe().unwrap();
        let runs = 10;
        let run_once = |mode: &str| -> (f64, f64, f64) {
            let output = Command::new(&exe)
                .args([
                    "--exact",
                    "fast::drop_target_helper::tests::bench_drag_enter_latency",
                    "--ignored",
                    "--nocapture",
                ])
                .env(BENCH_DRAG_CHILD_ENV, mode)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            let line = stdout
                .lines()
                .find_map(|line| line.strip_prefix("RESULT="))
                .expect("missing RESULT line");
            let mut parts = line.split(',');
            let a = parts.next().unwrap().parse().unwrap();
            let b = parts.next().unwrap().parse().unwrap();
            let c = parts.next().unwrap().parse().unwrap();
            (a, b, c)
        };

        let mut eager_1 = Vec::with_capacity(runs);
        let mut eager_2 = Vec::with_capacity(runs);
        let mut eager_w2 = Vec::with_capacity(runs);
        let mut imm_1 = Vec::with_capacity(runs);
        let mut imm_2 = Vec::with_capacity(runs);
        let mut imm_w2 = Vec::with_capacity(runs);
        let mut steady_1 = Vec::with_capacity(runs);
        let mut steady_2 = Vec::with_capacity(runs);
        let mut steady_w2 = Vec::with_capacity(runs);
        for _ in 0..runs {
            let (e1, e2, ew2) = run_once("eager");
            eager_1.push(e1);
            eager_2.push(e2);
            eager_w2.push(ew2);
            let (i1, i2, iw2) = run_once("immediate_0ms");
            imm_1.push(i1);
            imm_2.push(i2);
            imm_w2.push(iw2);
            let (s1, s2, sw2) = run_once("after_50ms");
            steady_1.push(s1);
            steady_2.push(s2);
            steady_w2.push(sw2);
        }

        let print_stats = |label: &str, values: &mut [f64]| {
            values.sort_by(f64::total_cmp);
            let avg = values.iter().sum::<f64>() / values.len() as f64;
            let median = (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0;
            println!(
                "{label:<40} ({runs} procs): avg={avg:6.3} ms median={median:6.3} ms min={:6.3} ms max={:6.3} ms",
                values[0],
                values[values.len() - 1]
            );
        };
        print_stats("eager         1st DragEnter (win1)", &mut eager_1);
        print_stats("immediate_0ms 1st DragEnter (win1)", &mut imm_1);
        print_stats("after_50ms    1st DragEnter (win1)", &mut steady_1);
        print_stats("eager         2nd DragEnter (win1)", &mut eager_2);
        print_stats("immediate_0ms 2nd DragEnter (win1)", &mut imm_2);
        print_stats("after_50ms    2nd DragEnter (win1)", &mut steady_2);
        print_stats("eager         1st DragEnter (win2)", &mut eager_w2);
        print_stats("immediate_0ms 1st DragEnter (win2)", &mut imm_w2);
        print_stats("after_50ms    1st DragEnter (win2)", &mut steady_w2);
    }

    #[test]
    #[ignore = "manual multi-process startup benchmark"]
    #[allow(clippy::disallowed_methods)]
    fn bench_windows_platform_startup() {
        if std::env::var_os(BENCH_CHILD_ENV).is_some() {
            let start = Instant::now();
            let platform = WindowsPlatform::new(false).unwrap();
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            println!("RESULT_MS={elapsed_ms:.3}");
            drop(platform);
            return;
        }

        let exe = std::env::current_exe().unwrap();
        let runs = 10;
        let mut samples: Vec<f64> = (0..runs)
            .map(|_| {
                let output = Command::new(&exe)
                    .args([
                        "--exact",
                        "fast::drop_target_helper::tests::bench_windows_platform_startup",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env(BENCH_CHILD_ENV, "1")
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "child failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout
                    .lines()
                    .find_map(|line| line.strip_prefix("RESULT_MS="))
                    .and_then(|value| value.parse::<f64>().ok())
                    .expect("missing RESULT_MS line")
            })
            .collect();

        samples.sort_by(f64::total_cmp);
        let avg = samples.iter().sum::<f64>() / samples.len() as f64;
        let median = (samples[samples.len() / 2 - 1] + samples[samples.len() / 2]) / 2.0;
        println!(
            "WindowsPlatform::new(false) ({runs} procs): avg={avg:.2} ms median={median:.2} ms min={:.2} ms max={:.2} ms",
            samples[0],
            samples[samples.len() - 1]
        );
    }
}
