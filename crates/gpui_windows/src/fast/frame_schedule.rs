//! Demand-driven Windows frames, including idle device recovery and manipulation tails.

use crate::direct_manipulation::DirectManipulationHandler;
use crate::platform::{check_device_lost, handle_gpu_device_lost};
use crate::{
    DrawCoordinator, SafeHwnd, VSyncProvider, WindowsPlatform, WindowsWindow, WindowsWindowInner,
};
use gpui::{DispatchEventResult, PlatformFrameSignal, PlatformInput};
use gpui_util::ResultExt;
use std::{
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use windows::Win32::{
    Foundation::HWND,
    Graphics::{
        DirectManipulation::{DIRECTMANIPULATION_INERTIA, DIRECTMANIPULATION_RUNNING},
        Gdi::{RDW_INVALIDATE, RedrawWindow, ValidateRect},
    },
    UI::WindowsAndMessaging::{IsIconic, IsWindowVisible},
};

#[derive(Default)]
pub(crate) struct FrameWake {
    pending: parking_lot::Mutex<bool>,
    changed: parking_lot::Condvar,
}

impl FrameWake {
    pub(crate) fn request(&self) {
        *self.pending.lock() = true;
        // A replacement GPU thread may start before the previous one exits.
        self.changed.notify_all();
    }

    pub(crate) fn wait(&self, stopped: &AtomicBool, device_notifications: bool) -> bool {
        let mut pending = self.pending.lock();
        while !*pending && !stopped.load(Ordering::Acquire) {
            if device_notifications {
                self.changed.wait(&mut pending);
            } else if self
                .changed
                .wait_for(&mut pending, Duration::from_secs(1))
                .timed_out()
            {
                // Keep recovery available if Windows cannot register a device notification.
                break;
            }
        }
        if stopped.load(Ordering::Acquire) {
            return false;
        }
        *pending = false;
        true
    }
}

pub(crate) struct FrameRequest {
    pending: AtomicBool,
    wake: Arc<FrameWake>,
}

impl FrameRequest {
    pub(crate) fn new(wake: Arc<FrameWake>) -> Self {
        Self {
            pending: AtomicBool::new(false),
            wake,
        }
    }

    pub(crate) fn request(&self) {
        self.pending.store(true, Ordering::Release);
        self.wake.request();
    }

    pub(crate) fn take(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }
}

pub(crate) fn stop_vsync(stop: Arc<AtomicBool>, coordinator: &DrawCoordinator) {
    // Joining here would deadlock a worker sending a synchronous window message.
    stop.store(true, Ordering::Release);
    coordinator.fast_frame_wake.request();
}

pub(crate) fn new_request(coordinator: &DrawCoordinator) -> Arc<FrameRequest> {
    Arc::new(FrameRequest::new(coordinator.fast_frame_wake.clone()))
}

pub(crate) fn request(request: &FrameRequest) {
    request.request();
}

pub(crate) fn frame_waker(window: &WindowsWindow) -> Option<Rc<dyn Fn()>> {
    let request = window.state.fast_frame_request.clone();
    Some(Rc::new(move || request.request()))
}

pub(crate) fn schedule_frame(window: &WindowsWindow) {
    window.state.fast_frame_request.request();
}

pub(crate) fn on_input(
    window: &WindowsWindow,
    mut callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>,
) {
    let request = window.state.fast_frame_request.clone();
    #[cfg(test)]
    let hwnd = gpui::PlatformWindow::get_raw_handle(window).0 as usize;
    window
        .state
        .callbacks
        .input
        .set(Some(Box::new(move |event| {
            #[cfg(test)]
            crate::fast::native_tests::input(hwnd);
            let result = callback(event);
            request.request();
            result
        })));
}

pub(crate) fn defer_draw(window: &WindowsWindowInner, handle: HWND) {
    // Validate nested paints now and retry at VSync instead of spinning a modal message loop.
    unsafe {
        ValidateRect(Some(handle), None).ok().log_err();
    }
    window.state.fast_frame_request.request();
}

pub(crate) fn validate_current_paint(handle: HWND) {
    // Retire only the current paint, before callbacks can request another one.
    // A trailing ValidateRect would also erase a concurrent worker invalidation.
    unsafe {
        ValidateRect(Some(handle), None).ok().log_err();
    }
}

pub(crate) fn update_manipulation(handler: &DirectManipulationHandler) {
    unsafe {
        handler.update_manager.Update(None).log_err();
        if let Ok(status) = handler.viewport.GetStatus()
            && (status == DIRECTMANIPULATION_RUNNING || status == DIRECTMANIPULATION_INERTIA)
        {
            handler.fast_frame_request.request();
        }
    }
}

pub(crate) fn begin_vsync_thread(platform: &WindowsPlatform) {
    let Some(directx_devices) = platform.inner.state.directx_devices.borrow().clone() else {
        return;
    };
    let Some(direct_write_text_system) = &platform.direct_write_text_system else {
        return;
    };
    let stop = Arc::new(AtomicBool::new(false));
    *platform.vsync_stop.borrow_mut() = Some(stop.clone());
    let mut directx_device = directx_devices;
    let platform_window: SafeHwnd = platform.handle.into();
    let validation_number = platform.inner.validation_number;
    let all_windows = Arc::downgrade(&platform.raw_window_handles);
    let text_system = Arc::downgrade(direct_write_text_system);
    let invalidate_devices = platform.invalidate_devices.clone();
    let frame_wake = platform
        .inner
        .state
        .draw_coordinator
        .fast_frame_wake
        .clone();

    std::thread::Builder::new()
        .name("VSyncProvider".to_owned())
        .spawn(move || {
            let vsync_provider = VSyncProvider::new();
            let mut device_wake = crate::fast::device_wake::DeviceWake::new(
                &directx_device.device,
                frame_wake.clone(),
            )
            .log_err();
            while frame_wake.wait(&stop, device_wake.is_some()) {
                let signal_source = vsync_provider.wait_for_vsync();
                let signal_at = PlatformFrameSignal::capture(scheduler::Instant::now);
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if check_device_lost(&directx_device.device)
                    || invalidate_devices.fetch_and(false, Ordering::Acquire)
                {
                    if let Err(err) = handle_gpu_device_lost(
                        &mut directx_device,
                        platform_window.as_raw(),
                        validation_number,
                        &all_windows,
                        &text_system,
                    ) {
                        panic!("Device lost: {err}");
                    }
                    device_wake = crate::fast::device_wake::DeviceWake::new(
                        &directx_device.device,
                        frame_wake.clone(),
                    )
                    .log_err();
                }
                let Some(all_windows) = all_windows.upgrade() else {
                    break;
                };
                for hwnd in all_windows.read().iter() {
                    #[cfg(test)]
                    crate::fast::native_tests::before_frame_request(hwnd.as_raw());
                    if !hwnd.fast_frame_request.take() {
                        continue;
                    }
                    unsafe {
                        if let Some(signal_at) = signal_at {
                            if IsWindowVisible(hwnd.as_raw()).as_bool()
                                && !IsIconic(hwnd.as_raw()).as_bool()
                            {
                                hwnd.frame_signal.record(signal_at, signal_source);
                            } else {
                                // Hidden windows may not consume WM_PAINT until shown again.
                                hwnd.frame_signal.take();
                            }
                        }
                        RedrawWindow(Some(hwnd.as_raw()), None, None, RDW_INVALIDATE)
                            .ok()
                            .log_err();
                        #[cfg(test)]
                        crate::fast::native_tests::paint_invalidated(hwnd.as_raw());
                    }
                }
            }
        })
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::{FrameRequest, FrameWake};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    #[test]
    fn requests_coalesce_without_invalidating_other_windows() {
        let wake = Arc::new(FrameWake::default());
        let first = FrameRequest::new(wake.clone());
        let second = FrameRequest::new(wake.clone());
        first.request();
        first.request();
        assert!(first.take());
        assert!(!first.take());
        assert!(!second.take());
        assert!(wake.wait(&AtomicBool::new(false), true));
        assert!(!*wake.pending.lock());
    }

    #[test]
    fn stop_wakes_a_sleeping_worker() {
        let wake = Arc::new(FrameWake::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let worker = {
            let wake = wake.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                tx.send(wake.wait(&stop, true)).unwrap();
            })
        };
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        stop.store(true, Ordering::Release);
        wake.request();
        assert!(!rx.recv_timeout(Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn replacement_worker_cannot_consume_the_old_workers_stop() {
        let wake = Arc::new(FrameWake::default());
        let old_stop = Arc::new(AtomicBool::new(false));
        let new_stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let workers = [(true, old_stop.clone()), (false, new_stop)].map(|(old, stop)| {
            let wake = wake.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                tx.send((old, wake.wait(&stop, true))).unwrap();
            })
        });
        old_stop.store(true, Ordering::Release);
        wake.request();
        let mut results = [
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        ];
        results.sort_by_key(|(old, _)| *old);
        assert_eq!(results, [(false, true), (true, false)]);
        for worker in workers {
            worker.join().unwrap();
        }
    }
}
