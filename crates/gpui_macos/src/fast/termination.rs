use crate::platform::{MacPlatform, get_mac_platform};
use dispatch2::DispatchQueue;
use gpui::{ForegroundExecutor, Platform, Task};
use objc2::{
    Message as _, msg_send,
    rc::Retained,
    runtime::{AnyObject, ClassBuilder, Sel},
    sel,
};
use objc2_app_kit::{NSApplication, NSApplicationTerminateReply};
use std::{cell::RefCell, ffi::c_void, io, process::Child, rc::Rc};

type Handler = Rc<RefCell<Box<dyn FnMut() -> Task<bool>>>>;
type Restart = Box<dyn FnOnce() -> io::Result<()>>;

#[derive(Clone, Default)]
pub(crate) struct Termination(Rc<RefCell<State>>);

#[derive(Default)]
struct State {
    handler: Option<Handler>,
    next_id: u64,
    phase: Phase,
    restart: Option<Restart>,
}

#[derive(Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Idle,
    Queued,
    Pending(u64),
    Approved,
}

struct Request {
    id: u64,
    handler: Handler,
}

impl Termination {
    #[cfg(any(test, feature = "test-support"))]
    fn install(&self, handler: Box<dyn FnMut() -> Task<bool>>) {
        self.0.borrow_mut().handler = Some(Rc::new(RefCell::new(handler)));
    }

    fn defer(&self, schedule: impl FnOnce()) -> bool {
        self.defer_intent(None, schedule)
    }

    fn defer_intent(&self, restart: Option<Restart>, schedule: impl FnOnce()) -> bool {
        let mut state = self.0.borrow_mut();
        if state.handler.is_none() {
            return false;
        }
        if state.phase == Phase::Idle {
            state.phase = Phase::Queued;
            state.restart = restart;
            drop(state);
            schedule();
        }
        true
    }

    fn request(&self, schedule: impl FnOnce(Request)) -> NSApplicationTerminateReply {
        let mut state = self.0.borrow_mut();
        match state.phase {
            Phase::Pending(_) => return NSApplicationTerminateReply::TerminateLater,
            Phase::Approved => return NSApplicationTerminateReply::TerminateNow,
            Phase::Idle | Phase::Queued => {}
        }
        let Some(handler) = state.handler.clone() else {
            return NSApplicationTerminateReply::TerminateNow;
        };
        state.next_id += 1;
        let id = state.next_id;
        state.phase = Phase::Pending(id);
        drop(state);
        schedule(Request { id, handler });
        NSApplicationTerminateReply::TerminateLater
    }

    async fn prepare(&self, request: Request, reply: impl FnOnce(bool)) {
        let task = (request.handler.borrow_mut())();
        let mut allow = task.await;
        let restart = {
            let mut state = self.0.borrow_mut();
            if state.phase != Phase::Pending(request.id) {
                return;
            }
            state.restart.take()
        };
        if allow {
            if let Some(restart) = restart {
                if let Err(error) = restart() {
                    log::error!("failed to spawn restart script: {:?}", error);
                    allow = false;
                }
            }
        }
        if self.complete(request.id, allow) {
            reply(allow);
        }
    }

    fn complete(&self, id: u64, allow: bool) -> bool {
        let mut state = self.0.borrow_mut();
        if state.phase != Phase::Pending(id) {
            return false;
        }
        state.phase = if allow { Phase::Approved } else { Phase::Idle };
        true
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn install(
    platform: &MacPlatform,
    handler: Box<dyn FnMut() -> Task<bool>>,
) -> gpui::Result<()> {
    let termination = platform.0.lock().fast_termination.clone();
    termination.install(handler);
    Ok(())
}

pub(crate) fn defer_quit(platform: &MacPlatform) -> bool {
    let termination = platform.0.lock().fast_termination.clone();
    termination.defer(schedule_quit)
}

fn schedule_quit() {
    let app = NSApplication::sharedApplication(objc2::MainThreadMarker::new().unwrap());
    unsafe {
        let delegate: *mut AnyObject = msg_send![&*app, delegate];
        let _: () = msg_send![delegate,
            performSelector: sel!(gpuiPerformDeferredQuit:),
            withObject: std::ptr::null_mut::<AnyObject>(),
            afterDelay: 0.0_f64,
        ];
    }
}

pub(crate) fn defer_legacy_quit() -> bool {
    let app = NSApplication::sharedApplication(objc2::MainThreadMarker::new().unwrap());
    unsafe {
        let delegate: *mut AnyObject = msg_send![&*app, delegate];
        defer_quit(get_mac_platform(&*delegate))
    }
}

pub(crate) fn restart(platform: &MacPlatform, spawn: impl FnOnce() -> io::Result<Child> + 'static) {
    let termination = platform.0.lock().fast_termination.clone();
    let has_handler = termination.0.borrow().handler.is_some();
    if has_handler {
        termination.defer_intent(Some(Box::new(move || spawn().map(|_| ()))), schedule_quit);
    } else {
        match spawn() {
            Ok(_) => platform.quit(),
            Err(error) => log::error!("failed to spawn restart script: {:?}", error),
        }
    }
}

pub(crate) fn register(builder: &mut ClassBuilder) {
    unsafe {
        builder.add_method(
            sel!(gpuiPerformDeferredQuit:),
            perform_deferred_quit as extern "C-unwind" fn(_, Sel, *mut AnyObject),
        );
        builder.add_method(
            sel!(applicationShouldTerminate:),
            should_terminate
                as extern "C-unwind" fn(_, Sel, *mut NSApplication) -> NSApplicationTerminateReply,
        );
    }
}

extern "C-unwind" fn perform_deferred_quit(delegate: &AnyObject, _: Sel, _: *mut AnyObject) {
    let platform = unsafe { get_mac_platform(delegate) };
    let termination = platform.0.lock().fast_termination.clone();
    let queued = termination.0.borrow().phase == Phase::Queued;
    if queued {
        let app = NSApplication::sharedApplication(objc2::MainThreadMarker::new().unwrap());
        app.terminate(None);
    }
}

extern "C-unwind" fn should_terminate(
    delegate: &AnyObject,
    _: Sel,
    app: *mut NSApplication,
) -> NSApplicationTerminateReply {
    let platform = unsafe { get_mac_platform(delegate) };
    let app = unsafe { &*app };
    let (termination, executor) = {
        let state = platform.0.lock();
        (
            state.fast_termination.clone(),
            state.foreground_executor.clone(),
        )
    };
    termination.request(|request| {
        let start = Box::new(Start {
            termination: termination.clone(),
            executor,
            app: app.retain(),
            request,
        });
        unsafe {
            DispatchQueue::main().exec_async_f(Box::into_raw(start).cast(), start_request);
        }
    })
}

struct Start {
    termination: Termination,
    executor: ForegroundExecutor,
    app: Retained<NSApplication>,
    request: Request,
}

extern "C" fn start_request(context: *mut c_void) {
    let Start {
        termination,
        executor,
        app,
        request,
    } = *unsafe { Box::from_raw(context.cast::<Start>()) };
    executor
        .spawn(async move {
            termination
                .prepare(request, |allow| {
                    app.replyToApplicationShouldTerminate(allow)
                })
                .await;
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::{Phase, Request, Restart, Termination};
    use futures::{FutureExt as _, channel::oneshot, executor::block_on};
    use gpui::{Task, TestAppContext};
    use objc2_app_kit::NSApplicationTerminateReply;
    use std::{
        cell::{Cell, RefCell},
        io,
        rc::Rc,
    };

    fn request(termination: &Termination) -> Request {
        let mut queued = None;
        assert_eq!(
            termination.request(|request| queued = Some(request)),
            NSApplicationTerminateReply::TerminateLater
        );
        queued.unwrap()
    }

    fn spawn_counter(count: &Rc<Cell<usize>>, fail: bool) -> Restart {
        let count = count.clone();
        Box::new(move || {
            count.set(count.get() + 1);
            if fail {
                Err(io::Error::other("synthetic spawn failure"))
            } else {
                Ok(())
            }
        })
    }

    #[test]
    fn restart_spawns_only_after_approval_and_merges_duplicates() {
        for allow in [false, true] {
            let termination = Termination::default();
            termination.install(Box::new(move || Task::ready(allow)));
            let count = Rc::new(Cell::new(0));
            assert!(termination.defer_intent(Some(spawn_counter(&count, false)), || {}));
            assert!(
                termination
                    .defer_intent(Some(spawn_counter(&count, false)), || panic!("duplicate"))
            );
            assert_eq!(count.get(), 0);
            let pending = request(&termination);
            assert!(
                termination.defer_intent(Some(spawn_counter(&count, false)), || panic!("pending"))
            );
            assert!(termination.defer(|| panic!("ordinary quit must merge")));
            block_on(termination.prepare(pending, |reply| assert_eq!(reply, allow)));
            assert_eq!(count.get(), usize::from(allow));
            if !allow {
                termination.install(Box::new(|| Task::ready(true)));
                assert!(termination.defer(|| {}));
                block_on(termination.prepare(request(&termination), |reply| assert!(reply)));
                assert_eq!(
                    count.get(),
                    0,
                    "cancelled restart leaked into ordinary quit"
                );
            }
        }
    }

    #[test]
    fn late_restart_cannot_replace_an_ordinary_quit() {
        for pending in [false, true] {
            let termination = Termination::default();
            termination.install(Box::new(|| Task::ready(true)));
            assert!(termination.defer(|| {}));
            let request = pending.then(|| request(&termination));
            let count = Rc::new(Cell::new(0));
            assert!(
                termination
                    .defer_intent(Some(spawn_counter(&count, false)), || panic!("duplicate"))
            );
            block_on(termination.prepare(
                request.unwrap_or_else(|| self::request(&termination)),
                |allow| assert!(allow),
            ));
            assert_eq!(count.get(), 0);
        }
    }

    #[test]
    fn failed_restart_cancels_and_can_retry() {
        let termination = Termination::default();
        termination.install(Box::new(|| Task::ready(true)));
        let count = Rc::new(Cell::new(0));
        for fail in [true, false] {
            assert!(termination.defer_intent(Some(spawn_counter(&count, fail)), || {}));
            block_on(termination.prepare(request(&termination), |allow| assert_eq!(allow, !fail)));
            assert!(
                termination.0.borrow().phase == if fail { Phase::Idle } else { Phase::Approved }
            );
        }
        assert_eq!(count.get(), 2);
    }

    #[test]
    fn no_handler_preserves_synchronous_termination() {
        assert!(!Termination::default().defer(|| panic!("unexpected scheduling")));
        assert_eq!(
            Termination::default().request(|_| panic!("unexpected request")),
            NSApplicationTerminateReply::TerminateNow
        );
    }

    #[test]
    fn deferred_requests_merge_until_the_decision_finishes() {
        let termination = Termination::default();
        termination.install(Box::new(|| Task::ready(false)));
        let scheduled = Cell::new(0);
        for _ in 0..2 {
            assert!(termination.defer(|| scheduled.set(scheduled.get() + 1)));
        }
        assert_eq!(scheduled.get(), 1);
        let pending = request(&termination);
        assert!(termination.defer(|| panic!("pending request must merge")));
        block_on(termination.prepare(pending, |allow| assert!(!allow)));
        assert!(termination.0.borrow().phase == Phase::Idle);
        assert!(termination.defer(|| scheduled.set(scheduled.get() + 1)));
        assert_eq!(scheduled.get(), 2);
    }

    #[test]
    fn immediate_results_reply_once_after_scheduling_and_allow_retry() {
        for allow in [false, true] {
            let termination = Termination::default();
            let calls = Rc::new(Cell::new(0));
            let observed = calls.clone();
            termination.install(Box::new(move || {
                observed.set(observed.get() + 1);
                Task::ready(allow)
            }));
            let queued = request(&termination);
            let id = queued.id;
            assert_eq!(calls.get(), 0);
            assert_eq!(
                termination.request(|_| panic!("duplicate")),
                NSApplicationTerminateReply::TerminateLater
            );
            let mut replies = Vec::new();
            block_on(termination.prepare(queued, |value| replies.push(value)));
            assert_eq!(calls.get(), 1);
            assert_eq!(replies, [allow]);
            assert!(!termination.complete(id, allow));
            if allow {
                assert_eq!(
                    termination.request(|_| panic!("approved")),
                    NSApplicationTerminateReply::TerminateNow
                );
            } else {
                let retry = request(&termination);
                assert_ne!(retry.id, id);
                assert!(!termination.complete(id, true));
                block_on(termination.prepare(retry, |value| replies.push(value)));
                assert_eq!(replies, [false, false]);
            }
        }
    }

    #[test]
    fn callback_can_reenter_and_replace_itself() {
        let termination = Termination::default();
        let nested = termination.clone();
        termination.install(Box::new(move || {
            assert_eq!(
                nested.request(|_| panic!("reentrant request")),
                NSApplicationTerminateReply::TerminateLater
            );
            nested.install(Box::new(|| Task::ready(true)));
            Task::ready(false)
        }));
        block_on(termination.prepare(request(&termination), |allow| assert!(!allow)));
        block_on(termination.prepare(request(&termination), |allow| assert!(allow)));
        assert!(termination.0.borrow().phase == Phase::Approved);
    }

    #[test]
    fn queued_request_retains_the_handler_it_captured() {
        let termination = Termination::default();
        termination.install(Box::new(|| Task::ready(false)));
        let queued = request(&termination);
        termination.install(Box::new(|| Task::ready(true)));
        block_on(termination.prepare(queued, |allow| assert!(!allow)));
        block_on(termination.prepare(request(&termination), |allow| assert!(allow)));
    }

    #[gpui::test]
    fn pending_future_keeps_one_request_and_uses_its_original_handler(cx: &mut TestAppContext) {
        let termination = Termination::default();
        let (send, receive) = oneshot::channel();
        let mut receive = Some(receive);
        let executor = cx.foreground_executor.clone();
        termination.install(Box::new(move || {
            let receive = receive.take().unwrap();
            executor.spawn(async move { receive.await.unwrap() })
        }));
        let replies = Rc::new(RefCell::new(Vec::new()));
        let observed = replies.clone();
        let queued = request(&termination);
        let mut preparation =
            Box::pin(termination.prepare(queued, move |allow| observed.borrow_mut().push(allow)));
        assert!(preparation.as_mut().now_or_never().is_none());
        termination.install(Box::new(|| Task::ready(true)));
        assert_eq!(
            termination.request(|_| panic!("duplicate")),
            NSApplicationTerminateReply::TerminateLater
        );
        assert!(replies.borrow().is_empty());
        send.send(false).unwrap();
        cx.run_until_parked();
        assert!(preparation.now_or_never().is_some());
        assert_eq!(*replies.borrow(), [false]);
        block_on(termination.prepare(request(&termination), |allow| assert!(allow)));
    }
}
