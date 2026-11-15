//! Defers GPUI event handling while the app's `AppCell` is borrowed.
//!
//! Some synchronous OS calls pump the thread's message queue while GPUI
//! still holds a borrow of the app — WebView2 creation
//! (`webview2_com::wait_with_pump`) and `Command::spawn` on Windows. A
//! `WM_GPUI_*` message dispatched by that nested pump would run
//! `run_foreground_task`, whose tasks call `AsyncApp::update` and panic on
//! the second `AppCell::borrow_mut`.
//!
//! Re-posting the message from the handler is not an option: a nested pump
//! that keeps dispatching would deliver the re-posted copy while the app is
//! still borrowed, spinning forever. Instead the message is recorded and
//! consumed, and [`flush_deferred`] re-posts everything once the platform's
//! own message loop regains control — which only happens after the outer
//! borrow is released.

use std::sync::Mutex;

use gpui_util::ResultExt;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

// `HWND` is not `Send`, so the raw values are stored and rebuilt on flush.
static DEFERRED: Mutex<Vec<(usize, u32, usize, isize)>> = Mutex::new(Vec::new());

/// Returns `true` when `gpui::app_is_borrowed()` holds; the message is then
/// queued for [`flush_deferred`] and the caller should return `Some(0)`.
pub(crate) fn defer_while_app_borrowed(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> bool {
    if !gpui::app_is_borrowed() {
        return false;
    }
    DEFERRED.lock().unwrap().push((
        hwnd.0 as usize,
        message,
        wparam.0,
        lparam.0,
    ));
    true
}

/// Re-posts every message deferred while the app was borrowed. Called from
/// the platform's message loop before each blocking `GetMessageW`, so the
/// re-posted messages are dispatched by the regular loop.
pub(crate) fn flush_deferred() {
    let deferred = std::mem::take(&mut *DEFERRED.lock().unwrap());
    for (hwnd, message, wparam, lparam) in deferred {
        unsafe {
            PostMessageW(
                Some(HWND(hwnd as _)),
                message,
                WPARAM(wparam),
                LPARAM(lparam),
            )
            .log_err();
        }
    }
}
