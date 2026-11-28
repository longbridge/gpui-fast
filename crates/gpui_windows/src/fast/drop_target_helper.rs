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
    use crate::fast::drop_target_helper::LazyDropTargetHelper;
    use crate::platform::WindowsPlatform;
    use std::process::Command;
    use std::time::Instant;
    use windows::Win32::System::{
        Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
        Ole::{OleInitialize, OleUninitialize},
    };
    use windows::Win32::UI::Shell::{CLSID_DragDropHelper, IDropTargetHelper};

    const BENCH_CHILD_ENV: &str = "GPUI_BENCH_DRAGDROP_CHILD";

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
