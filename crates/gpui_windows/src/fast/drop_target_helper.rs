//! Lazily initializes the Shell `CLSID_DragDropHelper` COM object on first drag.
//!
//! Upstream instantiates `CLSID_DragDropHelper` synchronously inside
//! `WindowsPlatform::new()`, loading the Shell drag-and-drop COM server during
//! process startup even when an application never receives an external drag.
//!
//! `LazyDropTargetHelper` shares one `OnceCell<Option<IDropTargetHelper>>`
//! across all windows on the OLE STA UI thread and only calls
//! `CoCreateInstance` on the first `IDropTarget::DragEnter`. If activation
//! fails, the error is logged once and drag-preview calls become no-ops without
//! disrupting file drop handling.

use std::{cell::OnceCell, rc::Rc};

use gpui_util::ResultExt;
use windows::Win32::{
    Foundation::{HWND, POINT},
    System::{
        Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IDataObject},
        Ole::DROPEFFECT,
    },
    UI::Shell::{CLSID_DragDropHelper, IDropTargetHelper},
};

#[derive(Clone, Default)]
pub(crate) struct LazyDropTargetHelper(Rc<OnceCell<Option<IDropTargetHelper>>>);

impl LazyDropTargetHelper {
    pub(crate) fn new() -> Self {
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
    };
    use crate::platform::WindowsPlatform;
    use std::process::Command;
    use std::time::Instant;
    use windows::Win32::Foundation::{DV_E_FORMATETC, E_NOTIMPL, HWND, POINT, S_FALSE, S_OK};
    use windows::Win32::System::{
        Com::{
            CLSCTX_INPROC_SERVER, CoCreateInstance, FORMATETC, IAdviseSink, IDataObject,
            IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM,
        },
        Ole::{DROPEFFECT_COPY, OleInitialize, OleUninitialize},
    };
    use windows::Win32::UI::{
        Shell::{CLSID_DragDropHelper, IDropTargetHelper},
        WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WINDOW_STYLE},
    };
    use windows::core::{BOOL, HRESULT, Ref, Result, implement, w};

    const BENCH_CHILD_ENV: &str = "GPUI_BENCH_DRAGDROP_CHILD";
    const BENCH_DRAG_CHILD_ENV: &str = "GPUI_BENCH_DRAG_ENTER_CHILD";

    /// Minimal COM `IDataObject` for testing and benchmarking `IDropTargetHelper`
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
        let helper = LazyDropTargetHelper::new();
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
        let helper_win1 = LazyDropTargetHelper::new();
        let helper_win2 = helper_win1.clone();
        let win1 = TestHwnd::new();
        let win2 = TestHwnd::new();
        let data_object: IDataObject = MinimalDataObject.into();
        let pt = POINT { x: 20, y: 20 };

        // Calling over/leave before any DragEnter stays uninitialized and is a safe no-op.
        drag_over(&helper_win1, &pt, DROPEFFECT_COPY);
        drag_leave(&helper_win1);
        assert!(helper_win1.get_initialized().is_none());
        assert!(helper_win2.get_initialized().is_none());

        // First drag on window 1: Enter -> Over -> Leave.
        drag_enter(&helper_win1, win1.0, &data_object, &pt, DROPEFFECT_COPY);
        assert!(helper_win1.get_initialized().is_some());
        assert!(helper_win2.get_initialized().is_some());
        drag_over(&helper_win1, &pt, DROPEFFECT_COPY);
        drag_leave(&helper_win1);

        // Repeated drag on window 1: Enter -> Over -> Drop.
        drag_enter(&helper_win1, win1.0, &data_object, &pt, DROPEFFECT_COPY);
        drag_over(&helper_win1, &pt, DROPEFFECT_COPY);
        drag_drop(&helper_win1, &data_object, &pt, DROPEFFECT_COPY);

        // Drag on second window sharing the same LazyDropTargetHelper: Enter -> Over -> Drop.
        drag_enter(&helper_win2, win2.0, &data_object, &pt, DROPEFFECT_COPY);
        drag_over(&helper_win2, &pt, DROPEFFECT_COPY);
        drag_drop(&helper_win2, &data_object, &pt, DROPEFFECT_COPY);

        drop(data_object);
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
            let platform = WindowsPlatform::new(false).unwrap();
            let helper_win1 = LazyDropTargetHelper::new();
            let helper_win2 = helper_win1.clone();
            let win1 = TestHwnd::new();
            let win2 = TestHwnd::new();
            let data_object: IDataObject = MinimalDataObject.into();
            let pt = POINT { x: 20, y: 20 };

            if mode == "eager" {
                assert!(helper_win1.get_or_init().is_some());
            } else {
                assert!(helper_win1.get_initialized().is_none());
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
        let mut lazy_1 = Vec::with_capacity(runs);
        let mut lazy_2 = Vec::with_capacity(runs);
        let mut lazy_w2 = Vec::with_capacity(runs);
        for _ in 0..runs {
            let (e1, e2, ew2) = run_once("eager");
            eager_1.push(e1);
            eager_2.push(e2);
            eager_w2.push(ew2);
            let (l1, l2, lw2) = run_once("lazy");
            lazy_1.push(l1);
            lazy_2.push(l2);
            lazy_w2.push(lw2);
        }

        let print_stats = |label: &str, values: &mut [f64]| {
            values.sort_by(f64::total_cmp);
            let avg = values.iter().sum::<f64>() / values.len() as f64;
            let median = (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0;
            println!(
                "{label:<36} ({runs} procs): avg={avg:6.3} ms median={median:6.3} ms min={:6.3} ms max={:6.3} ms",
                values[0],
                values[values.len() - 1]
            );
        };
        print_stats("eager 1st DragEnter (win1)", &mut eager_1);
        print_stats("lazy  1st DragEnter (win1)", &mut lazy_1);
        print_stats("eager 2nd DragEnter (win1)", &mut eager_2);
        print_stats("lazy  2nd DragEnter (win1)", &mut lazy_2);
        print_stats("eager 1st DragEnter (win2)", &mut eager_w2);
        print_stats("lazy  1st DragEnter (win2)", &mut lazy_w2);
    }

    #[test]
    #[ignore = "manual multi-process startup benchmark"]
    #[allow(clippy::disallowed_methods)]
    fn bench_windows_platform_startup() {
        if let Ok(mode) = std::env::var(BENCH_CHILD_ENV) {
            let start = Instant::now();
            let platform = WindowsPlatform::new(false).unwrap();
            let co_create_ms = if mode == "eager" {
                let t = Instant::now();
                let _helper: IDropTargetHelper = unsafe {
                    CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER).unwrap()
                };
                t.elapsed().as_secs_f64() * 1000.0
            } else {
                0.0
            };
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            println!("RESULT_MS={elapsed_ms:.3},{co_create_ms:.3}");
            drop(platform);
            return;
        }

        let exe = std::env::current_exe().unwrap();
        let runs = 10;
        let run_once = |mode: &str| -> (f64, f64) {
            let output = Command::new(&exe)
                .args([
                    "--exact",
                    "fast::drop_target_helper::tests::bench_windows_platform_startup",
                    "--ignored",
                    "--nocapture",
                ])
                .env(BENCH_CHILD_ENV, mode)
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
                .find_map(|line| line.strip_prefix("RESULT_MS="))
                .expect("missing RESULT_MS line");
            let (total, co_create) = line.split_once(',').expect("expected total,co_create");
            (total.parse().unwrap(), co_create.parse().unwrap())
        };

        let mut eager = Vec::with_capacity(runs);
        let mut co_create = Vec::with_capacity(runs);
        let mut lazy = Vec::with_capacity(runs);
        for _ in 0..runs {
            let (e, c) = run_once("eager");
            eager.push(e);
            co_create.push(c);
            lazy.push(run_once("lazy").0);
        }
        let summarize = |values: &mut [f64]| {
            values.sort_by(f64::total_cmp);
            let avg = values.iter().sum::<f64>() / values.len() as f64;
            let median = (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0;
            (avg, median, values[0], values[values.len() - 1])
        };
        let (e_avg, e_med, e_min, e_max) = summarize(&mut eager);
        let (l_avg, l_med, l_min, l_max) = summarize(&mut lazy);
        let (c_avg, c_med, c_min, c_max) = summarize(&mut co_create);
        println!(
            "WindowsPlatform::new(false) eager  ({runs} procs): avg={e_avg:.2} ms median={e_med:.2} ms min={e_min:.2} ms max={e_max:.2} ms"
        );
        println!(
            "WindowsPlatform::new(false) lazy   ({runs} procs): avg={l_avg:.2} ms median={l_med:.2} ms min={l_min:.2} ms max={l_max:.2} ms"
        );
        println!(
            "CoCreateInstance(DragDropHelper)   ({runs} procs): avg={c_avg:.2} ms median={c_med:.2} ms min={c_min:.2} ms max={c_max:.2} ms"
        );
        println!("End-to-end delta: {:.2} ms", e_avg - l_avg);
    }
}
